use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{AVFileTypeMPEG4, AVVideoCodecTypeH264, AVVideoCodecTypeHEVC};
use objc2_core_media::{CMTime, CMTimeFlags};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol, NSString, NSURL};
use objc2_screen_capture_kit::{
    SCRecordingOutput, SCRecordingOutputConfiguration, SCRecordingOutputDelegate, SCStream,
    SCStreamConfiguration, SCStreamErrorCode,
};

use crate::capture::filter::build_filter_and_dimensions;
use crate::capture::shareable::query_shareable_content_for_capture;
use crate::capture::{CaptureConfig, RecordingCodec};
use crate::fs_safety::ExclusiveStagingDir;
use crate::observe::{PerfEvent, PerfKind, now_unix_secs, record_perf_event};

const STOP_FALLBACK_AFTER: Duration = Duration::from_secs(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Default)]
struct DelegateSignal {
    started: bool,
    finished: bool,
    error: Option<String>,
    stream_stopped: bool,
    stream_stop_error: Option<String>,
}

struct DelegateIvars {
    signal: Arc<(Mutex<DelegateSignal>, Condvar)>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "QSRRecordingOutputDelegate"]
    #[ivars = DelegateIvars]
    struct RecordingDelegate;

    unsafe impl NSObjectProtocol for RecordingDelegate {}

    unsafe impl SCRecordingOutputDelegate for RecordingDelegate {
        #[unsafe(method(recordingOutputDidStartRecording:))]
        fn did_start(&self, _output: &SCRecordingOutput) {
            let (lock, cvar) = &*self.ivars().signal;
            if let Ok(mut guard) = lock.lock() {
                guard.started = true;
                cvar.notify_all();
            }
        }

        #[unsafe(method(recordingOutput:didFailWithError:))]
        fn did_fail(&self, _output: &SCRecordingOutput, error: &NSError) {
            let msg = error.localizedDescription().to_string();
            let (lock, cvar) = &*self.ivars().signal;
            if let Ok(mut guard) = lock.lock() {
                guard.error = Some(msg);
                guard.finished = true;
                cvar.notify_all();
            }
        }

        #[unsafe(method(recordingOutputDidFinishRecording:))]
        fn did_finish(&self, _output: &SCRecordingOutput) {
            let (lock, cvar) = &*self.ivars().signal;
            if let Ok(mut guard) = lock.lock() {
                guard.finished = true;
                cvar.notify_all();
            }
        }
    }
);

impl RecordingDelegate {
    fn new(signal: Arc<(Mutex<DelegateSignal>, Condvar)>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { signal });
        // SAFETY: Calling `[super init]` on freshly allocated NSObject subclass.
        unsafe { msg_send![super(this), init] }
    }
}

pub struct RecordingSession {
    stream: Retained<SCStream>,
    recording_output: Retained<SCRecordingOutput>,
    _delegate: Retained<RecordingDelegate>,
    signal: Arc<(Mutex<DelegateSignal>, Condvar)>,
    staging: ExclusiveStagingDir,
    output_path: PathBuf,
    started_at: Instant,
    fps: u32,
    codec: RecordingCodec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopPollState {
    Pending,
    Finished,
    TimedOut,
}

/// Non-blocking handle returned by [`RecordingSession::begin_stop`] so the AppKit main runloop
/// never blocks while CoreAudio / ScreenCaptureKit finalizes the MP4 trailer and tears down mic capture.
pub struct PendingRecordingStop {
    stream: Retained<SCStream>,
    _recording_output: Retained<SCRecordingOutput>,
    _delegate: Retained<RecordingDelegate>,
    signal: Arc<(Mutex<DelegateSignal>, Condvar)>,
    staging: ExclusiveStagingDir,
    output_path: PathBuf,
    elapsed_ms: u64,
    fps: u32,
    codec: RecordingCodec,
    stop_initiated_at: Instant,
    stop_capture_sent: bool,
}

impl RecordingSession {
    pub fn start(config: &CaptureConfig, output_path: &Path) -> Result<Self, String> {
        let staging = ExclusiveStagingDir::create_for_target(output_path, "mp4")?;

        let snapshot = query_shareable_content_for_capture()?;
        let (filter, pixel_w, pixel_h, source_rect_opt) =
            build_filter_and_dimensions(&snapshot.content, config)?;

        let stream_cfg = unsafe { SCStreamConfiguration::new() };
        let fps = config.fps.clamp(1, 120);
        let frame_interval = CMTime {
            value: 1,
            timescale: fps as i32,
            flags: CMTimeFlags(1), // kCMTimeFlags_Valid
            epoch: 0,
        };

        // SAFETY: Setting value properties on our owned SCStreamConfiguration instance.
        unsafe {
            stream_cfg.setWidth(pixel_w);
            stream_cfg.setHeight(pixel_h);
            stream_cfg.setMinimumFrameInterval(frame_interval);
            stream_cfg.setQueueDepth(6);
            stream_cfg.setShowsCursor(config.show_cursor);
            stream_cfg.setShowMouseClicks(config.highlight_clicks);
            stream_cfg.setCapturesAudio(config.audio_mode.captures_system());
            stream_cfg.setExcludesCurrentProcessAudio(true);
            stream_cfg.setCaptureMicrophone(config.audio_mode.captures_mic());
            if config.audio_mode.captures_mic() {
                if let Some(dev_id) = config.microphone_device_id.as_deref() {
                    if !dev_id.is_empty() {
                        stream_cfg.setMicrophoneCaptureDeviceID(Some(&NSString::from_str(dev_id)));
                    }
                }
            }
            if let Some(rect) = source_rect_opt {
                stream_cfg.setSourceRect(rect);
            }
        }

        let out_ns_str = NSString::from_str(
            staging
                .staged_file()
                .to_str()
                .ok_or("Staging path contains invalid UTF-8")?,
        );
        let output_url = NSURL::fileURLWithPath(&out_ns_str);

        let rec_cfg = unsafe { SCRecordingOutputConfiguration::new() };
        // SAFETY: Configuring output URL, MPEG-4 container, and hardware H.264/HEVC codec.
        unsafe {
            rec_cfg.setOutputURL(&output_url);
            if let Some(mp4_type) = AVFileTypeMPEG4 {
                rec_cfg.setOutputFileType(mp4_type);
            }
            let desired_codec = match config.codec {
                RecordingCodec::H264 => AVVideoCodecTypeH264,
                RecordingCodec::Hevc => AVVideoCodecTypeHEVC,
            };
            if let Some(codec) = desired_codec {
                let available = rec_cfg.availableVideoCodecTypes();
                if available.containsObject(codec) {
                    rec_cfg.setVideoCodecType(codec);
                }
            }
        }

        let signal = Arc::new((Mutex::new(DelegateSignal::default()), Condvar::new()));
        let delegate = RecordingDelegate::new(Arc::clone(&signal));
        let proto_delegate = ProtocolObject::from_ref(&*delegate);

        let recording_output = unsafe {
            SCRecordingOutput::initWithConfiguration_delegate(
                SCRecordingOutput::alloc(),
                &rec_cfg,
                proto_delegate,
            )
        };

        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &stream_cfg,
                None,
            )
        };

        unsafe {
            stream
                .addRecordingOutput_error(&recording_output)
                .map_err(|e| {
                    format!(
                        "Failed to attach hardware recording output: {}",
                        e.localizedDescription()
                    )
                })?;
        }

        let (tx, rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let start_block = RcBlock::new(move |err: *mut NSError| {
            if !err.is_null() {
                let msg = unsafe { (&*err).localizedDescription().to_string() };
                let _ = tx.send(Err(format!("Failed to start screen capture: {msg}")));
            } else {
                let _ = tx.send(Ok(()));
            }
        });

        unsafe {
            stream.startCaptureWithCompletionHandler(Some(&start_block));
        }

        rx.recv_timeout(Duration::from_secs(8))
            .map_err(|_| "Timed out starting ScreenCaptureKit stream".to_string())??;

        {
            let (lock, cvar) = &*signal;
            let guard = lock.lock().map_err(|_| "Mutex poisoned")?;
            let (guard, _) = cvar
                .wait_timeout_while(guard, Duration::from_secs(8), |s| {
                    !s.started && s.error.is_none()
                })
                .map_err(|_| "Mutex poisoned")?;
            if let Some(err) = &guard.error {
                unsafe { stream.stopCaptureWithCompletionHandler(None) };
                return Err(format!("Recording output failed to start: {err}"));
            }
            if !guard.started {
                unsafe { stream.stopCaptureWithCompletionHandler(None) };
                return Err("Timed out waiting for hardware recording writer to start".to_string());
            }
        }

        Ok(Self {
            stream,
            recording_output,
            _delegate: delegate,
            signal,
            staging,
            output_path: output_path.to_path_buf(),
            started_at: Instant::now(),
            fps,
            codec: config.codec,
        })
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    /// Initiates recording finalization without blocking the caller's thread or AppKit runloop.
    pub fn begin_stop(self) -> PendingRecordingStop {
        let elapsed_ms = self.started_at.elapsed().as_millis() as u64;
        let remove_failed = unsafe {
            self.stream
                .removeRecordingOutput_error(&self.recording_output)
                .is_err()
        };
        let mut pending = PendingRecordingStop {
            stream: self.stream,
            _recording_output: self.recording_output,
            _delegate: self._delegate,
            signal: self.signal,
            staging: self.staging,
            output_path: self.output_path,
            elapsed_ms,
            fps: self.fps,
            codec: self.codec,
            stop_initiated_at: Instant::now(),
            stop_capture_sent: false,
        };
        if remove_failed {
            pending.send_stop_capture();
        }
        pending
    }

    /// Blocking stop helper for headless CLI sessions and app shutdown cleanup.
    pub fn stop(self) -> Result<PathBuf, String> {
        let mut pending = self.begin_stop();
        let signal = Arc::clone(&pending.signal);
        {
            let (lock, cvar) = &*signal;
            if let Ok(guard) = lock.lock() {
                let (guard, _) = cvar
                    .wait_timeout_while(guard, STOP_FALLBACK_AFTER, |s| !s.finished)
                    .map_err(|_| "Mutex poisoned")?;
                drop(guard);
                pending.send_stop_capture();
                let remaining = STOP_TIMEOUT.saturating_sub(pending.stop_initiated_at.elapsed());
                let guard = lock.lock().map_err(|_| "Mutex poisoned")?;
                let _ = cvar
                    .wait_timeout_while(guard, remaining, |s| !(s.finished && s.stream_stopped));
            }
        }
        pending.finish()
    }
}

impl PendingRecordingStop {
    fn send_stop_capture(&mut self) {
        if !self.stop_capture_sent {
            self.stop_capture_sent = true;
            let sig = Arc::clone(&self.signal);
            let stop_block = RcBlock::new(move |err: *mut NSError| {
                let stop_err = if !err.is_null() {
                    let ns_err = unsafe { &*err };
                    // `-3808` (AttemptToStopStreamState) means macOS already stopped the stream.
                    if ns_err.code() == SCStreamErrorCode::AttemptToStopStreamState.0 {
                        None
                    } else {
                        Some(ns_err.localizedDescription().to_string())
                    }
                } else {
                    None
                };
                let (lock, cvar) = &*sig;
                if let Ok(mut guard) = lock.lock() {
                    guard.stream_stopped = true;
                    if stop_err.is_some() {
                        guard.stream_stop_error = stop_err;
                    }
                    cvar.notify_all();
                }
            });
            unsafe {
                self.stream
                    .stopCaptureWithCompletionHandler(Some(&stop_block));
            }
        }
    }

    /// Non-blocking poll designed for a short `NSTimer` tick on the main thread.
    pub fn poll(&mut self) -> StopPollState {
        let (finished, stream_stopped) = self
            .signal
            .0
            .lock()
            .map(|g| (g.finished, g.stream_stopped))
            .unwrap_or((true, true));
        if finished {
            self.send_stop_capture();
            if stream_stopped {
                return StopPollState::Finished;
            }
        }
        let elapsed = self.stop_initiated_at.elapsed();
        if !self.stop_capture_sent && elapsed >= STOP_FALLBACK_AFTER {
            self.send_stop_capture();
        }
        if elapsed >= STOP_TIMEOUT {
            StopPollState::TimedOut
        } else {
            StopPollState::Pending
        }
    }

    /// Stops the underlying capture stream (if not already stopped), verifies both trailer
    /// finalization and stream stop completion, commits the MP4, and records local telemetry.
    pub fn finish(mut self) -> Result<PathBuf, String> {
        self.send_stop_capture();
        let (finished, finish_err, stream_stopped, stream_stop_err) = self
            .signal
            .0
            .lock()
            .map(|g| {
                (
                    g.finished,
                    g.error.clone(),
                    g.stream_stopped,
                    g.stream_stop_error.clone(),
                )
            })
            .unwrap_or((
                false,
                Some("Recording state lock poisoned".to_string()),
                false,
                None,
            ));

        if let Some(err) = finish_err {
            self.record_perf(0, false);
            return Err(format!("Recording finalization error: {err}"));
        }
        if !finished {
            self.record_perf(0, false);
            return Err(
                "Timed out waiting for MP4 recording output to flush container trailer".to_string(),
            );
        }
        if let Some(err) = stream_stop_err {
            self.record_perf(0, false);
            return Err(format!("Failed to stop screen/audio capture stream: {err}"));
        }
        if !stream_stopped {
            self.record_perf(0, false);
            return Err("Timed out confirming capture stream stopped".to_string());
        }

        let size = self.staging.commit()?;
        self.record_perf(size, true);
        Ok(self.output_path)
    }

    fn record_perf(&self, output_bytes: u64, ok: bool) {
        record_perf_event(&PerfEvent {
            ts_unix: now_unix_secs(),
            kind: PerfKind::Record,
            format: self.codec.label().to_ascii_lowercase(),
            fps: self.fps,
            input_duration_ms: self.elapsed_ms,
            elapsed_ms: self.elapsed_ms,
            output_bytes,
            ok,
        });
    }
}
