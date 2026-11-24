use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_av_foundation::{AVFileTypeMPEG4, AVVideoCodecTypeH264, AVVideoCodecTypeHEVC};
use objc2_core_media::{CMTime, CMTimeFlags};
use objc2_foundation::{NSError, NSString, NSURL};
use objc2_screen_capture_kit::{
    SCRecordingOutput, SCRecordingOutputConfiguration, SCStream, SCStreamConfiguration,
};

#[path = "recorder_delegate.rs"]
mod delegate;
#[path = "recorder_finish.rs"]
mod finish;
#[path = "recorder_stop.rs"]
mod stop;

use delegate::{
    DelegateSignal, RecordingDelegate, SendableStreamStop, StartWaitOutcome,
    handle_start_callback_result, wait_for_capture_start_with_interrupt,
    wait_for_recording_started,
};
pub(crate) use finish::commit_staged_to_recovery_directly;
pub use finish::is_valid_salvageable_mp4;
use finish::{evaluate_stop_poll, finalize_stop_staging_outcome};
use stop::{
    RemovePhase, STOP_SINGLE_AUDIO_MIN, STOP_STREAM_CONFIRM_TIMEOUT, StopControl, StopMode,
};

use crate::capture::filter::build_filter_and_dimensions;
use crate::capture::shareable::query_shareable_content_for_capture;
use crate::capture::system_events::{InterruptLatch, StopHook, current_session_interrupt};
use crate::capture::{AudioCaptureMode, CaptureConfig, RecordingCodec, RecordingOutcome};
use crate::fs_safety::ExclusiveStagingDir;
use crate::observe::{PerfEvent, PerfKind, now_unix_secs, record_perf_event};
use crate::staging::private_recovery_dir;

pub struct RecordingSession {
    stopper: Arc<SendableStreamStop>,
    _delegate: Retained<RecordingDelegate>,
    signal: Arc<(Mutex<DelegateSignal>, Condvar)>,
    staging: ExclusiveStagingDir,
    output_path: PathBuf,
    started_at: Instant,
    fps: u32,
    codec: RecordingCodec,
    effective_audio: AudioCaptureMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopPollState {
    Pending,
    Finished,
    TimedOut,
}

/// Non-blocking handle returned by [`RecordingSession::begin_stop`] so the AppKit main runloop
/// never blocks while CoreAudio / ScreenCaptureKit / `RPAudioMixUtility` finalize the MP4 and
/// tear down stream/mic capture via the shared serialized stop worker.
pub struct PendingRecordingStop {
    stopper: Arc<SendableStreamStop>,
    _delegate: Retained<RecordingDelegate>,
    signal: Arc<(Mutex<DelegateSignal>, Condvar)>,
    staging: ExclusiveStagingDir,
    output_path: PathBuf,
    elapsed_ms: u64,
    fps: u32,
    codec: RecordingCodec,
    effective_audio: AudioCaptureMode,
}

impl RecordingSession {
    pub fn start(config: &CaptureConfig, output_path: &Path) -> Result<Self, String> {
        Self::start_with_interrupt(config, output_path, None)
    }

    pub fn start_with_interrupt(
        config: &CaptureConfig,
        output_path: &Path,
        interrupt_latch: Option<InterruptLatch>,
    ) -> Result<Self, String> {
        let check_interrupted = || -> Option<String> {
            interrupt_latch
                .as_ref()
                .and_then(InterruptLatch::reason)
                .or_else(current_session_interrupt)
                .map(|r| format!("Cannot start recording because {}", r.clause()))
        };
        if let Some(msg) = check_interrupted() {
            return Err(msg);
        }
        let staging = ExclusiveStagingDir::create_for_recording(output_path, "mp4")?;

        let snapshot = query_shareable_content_for_capture()?;
        if let Some(msg) = check_interrupted() {
            return Err(msg);
        }
        let (filter, pixel_w, pixel_h, source_rect_opt) =
            build_filter_and_dimensions(&snapshot.content, config)?;

        let stream_cfg = unsafe { SCStreamConfiguration::new() };
        let fps = config.cadence.realtime_fps().unwrap_or(60).clamp(1, 120);
        let effective_audio = config.cadence.effective_audio_mode(config.audio_mode);
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
            stream_cfg.setCapturesAudio(effective_audio.captures_system());
            stream_cfg.setExcludesCurrentProcessAudio(true);
            stream_cfg.setCaptureMicrophone(effective_audio.captures_mic());
            if effective_audio.captures_mic() {
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

        if let Some(msg) = check_interrupted() {
            return Err(msg);
        }
        // Single recording clock shared across startup hooks, stop deadline calculation, and UI/CLI.
        let started_at = Instant::now();
        let stop_control = Arc::new(StopControl::new(started_at, effective_audio.is_dual()));
        let stopper = SendableStreamStop::new(
            stream.clone(),
            recording_output.clone(),
            Arc::clone(&signal),
            Arc::clone(&stop_control),
        );

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
        if let Some(msg) = check_interrupted() {
            let _ = unsafe { stream.removeRecordingOutput_error(&recording_output) };
            return Err(msg);
        }

        let (tx, rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let cancelled_after_start = Arc::new(AtomicBool::new(false));

        // Attach the non-blocking stop hook BEFORE invoking `startCapture` so watchdog/notifications
        // can cancel and route teardown through the serial worker even if `startCapture` is delayed.
        if let Some(ref latch) = interrupt_latch {
            let s_hook = Arc::clone(&stopper);
            let cancel_hook = Arc::clone(&cancelled_after_start);
            let sig_hook = Arc::clone(&signal);
            latch.attach_stop_hook(Arc::new(move || {
                let already_started = sig_hook.0.lock().map(|g| g.started).unwrap_or(false);
                if !already_started {
                    cancel_hook.store(true, std::sync::atomic::Ordering::Release);
                }
                s_hook.request_stop_async(if already_started {
                    StopMode::Normal
                } else {
                    StopMode::StartupAbort
                });
            }));
        }
        if let Some(msg) = check_interrupted() {
            cancelled_after_start.store(true, std::sync::atomic::Ordering::Release);
            // Route through the shared serial stop worker (never calling `removeRecordingOutput`
            // concurrently with the hook just attached to `interrupt_latch`).
            stopper.request_stop_async(StopMode::StartupAbort);
            stopper
                .control()
                .wait_until_worker_idle(Duration::from_secs(2));
            return Err(msg);
        }

        let cancel_cb = Arc::clone(&cancelled_after_start);
        let stopper_cb = Arc::clone(&stopper);
        let sig_cb = Arc::clone(&signal);
        let start_block = RcBlock::new(move |err: *mut NSError| {
            let res = if !err.is_null() {
                let msg = unsafe { (&*err).localizedDescription().to_string() };
                Err(format!("Failed to start screen capture: {msg}"))
            } else {
                Ok(())
            };
            handle_start_callback_result(res, &sig_cb, &cancel_cb, &tx, |late_gen| {
                stopper_cb.request_late_start_stop(late_gen);
            });
        });

        unsafe {
            stream.startCaptureWithCompletionHandler(Some(&start_block));
        }

        let writer_started = || signal.0.lock().map(|g| g.started).unwrap_or(false);
        let start_outcome = wait_for_capture_start_with_interrupt(
            &rx,
            &signal,
            Duration::from_secs(8),
            &cancelled_after_start,
            check_interrupted,
            writer_started,
            || stopper.stop_and_wait_stopped(STOP_STREAM_CONFIRM_TIMEOUT),
        )?;

        if start_outcome == StartWaitOutcome::Started {
            wait_for_recording_started(&signal, Duration::from_secs(8), check_interrupted, || {
                stopper.stop_and_wait_stopped(STOP_STREAM_CONFIRM_TIMEOUT)
            })?;
        }

        Ok(Self {
            stopper,
            _delegate: delegate,
            signal,
            staging,
            output_path: output_path.to_path_buf(),
            started_at,
            fps,
            codec: config.codec,
            effective_audio,
        })
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    /// Returns `true` if ScreenCaptureKit already stopped or failed the recording output early.
    pub fn has_ended_unexpectedly(&self) -> bool {
        self.signal
            .0
            .lock()
            .map(|g| g.finished || g.error.is_some())
            .unwrap_or(false)
    }

    /// Thread-safe non-blocking stop hook routing lock/sleep teardown through the shared serial
    /// stop worker so `mainQueue` never stalls and `stopCapture` waits for audio mixing to finish.
    pub fn stop_hook(&self) -> StopHook {
        let stopper = Arc::clone(&self.stopper);
        Arc::new(move || stopper.request_stop_async(StopMode::Normal))
    }

    /// Initiates recording finalization without blocking the caller's thread or AppKit runloop.
    pub fn begin_stop(self) -> PendingRecordingStop {
        let now = Instant::now();
        self.stopper.request_stop_async(StopMode::Normal);
        let elapsed_ms = self.stopper.control().recording_elapsed_ms_at_stop(now);
        PendingRecordingStop {
            stopper: self.stopper,
            _delegate: self._delegate,
            signal: self.signal,
            staging: self.staging,
            output_path: self.output_path,
            elapsed_ms,
            fps: self.fps,
            codec: self.codec,
            effective_audio: self.effective_audio,
        }
    }

    /// Blocking stop helper for headless CLI sessions and app shutdown cleanup.
    pub fn stop(self) -> Result<RecordingOutcome, String> {
        let mut pending = self.begin_stop();
        let signal = Arc::clone(&pending.signal);
        loop {
            if pending.poll() != StopPollState::Pending {
                break;
            }
            let (lock, cvar) = &*signal;
            let guard = lock.lock().map_err(|_| "Mutex poisoned")?;
            if guard.finished && guard.start_completed && guard.stream_stopped {
                break;
            }
            let _ = cvar.wait_timeout(guard, Duration::from_millis(50));
        }
        pending.finish()
    }
}

impl PendingRecordingStop {
    /// Non-blocking poll designed for a short `NSTimer` tick on the main thread.
    pub fn poll(&mut self) -> StopPollState {
        // Retry worker spawn automatically if an earlier spawn failed under transient thread pressure.
        self.stopper.request_stop_async(StopMode::Normal);
        let (finished, stream_settled) = self
            .signal
            .0
            .lock()
            .map(|g| (g.finished, g.start_completed && g.stream_stopped))
            .unwrap_or((true, true));
        let now = Instant::now();
        let snap = self.stopper.control().snapshot();
        let finalize_deadline = snap
            .finalize_deadline
            .unwrap_or(now + STOP_SINGLE_AUDIO_MIN);
        evaluate_stop_poll(
            now,
            finished,
            stream_settled,
            snap.remove_phase,
            snap.stop_capture_sent_at,
            finalize_deadline,
        )
    }

    /// Finalizes staging ownership using the shared stop state, retaining staging untouched in
    /// place whenever stream stop is unconfirmed or forced on timeout.
    pub fn finish(self) -> Result<RecordingOutcome, String> {
        self.finish_with_policy(false)
    }

    /// Finalizes and preserves completed recordings directly from staging into permanent recovery
    /// (or protected staging on double failure / timeout), never writing a prunable cache file.
    pub fn finish_to_recovery(self) -> Result<RecordingOutcome, String> {
        self.finish_with_policy(true)
    }

    fn finish_with_policy(mut self, preserve_directly: bool) -> Result<RecordingOutcome, String> {
        let now = Instant::now();
        let snap = self.stopper.control().snapshot();
        let finalize_ms = self.stopper.control().finalization_elapsed_ms(now);
        let (start_completed, finished, finish_err, stream_stopped, stream_stop_err) = self
            .signal
            .0
            .lock()
            .map(|g| {
                (
                    g.start_completed,
                    g.finished,
                    g.error.clone(),
                    g.stream_stopped,
                    g.stream_stop_error.clone(),
                )
            })
            .unwrap_or((
                false,
                false,
                Some("Recording state lock poisoned".to_string()),
                false,
                None,
            ));

        let stop_issue = if let Some(err) = stream_stop_err {
            Some(format!("Failed to stop screen/audio capture stream: {err}"))
        } else if snap.remove_phase == RemovePhase::InFlight {
            Some(
                "Timed out waiting for ScreenCaptureKit removeRecordingOutput to return"
                    .to_string(),
            )
        } else if !start_completed {
            Some("Timed out confirming capture stream start/stop settled".to_string())
        } else if !stream_stopped {
            Some("Timed out confirming capture stream stopped".to_string())
        } else {
            None
        };

        let (final_path, size, final_warning) = match finalize_stop_staging_outcome(
            &mut self.staging,
            self.output_path.clone(),
            finished,
            finish_err,
            stop_issue,
            snap.forced_stop_on_timeout,
            preserve_directly,
            private_recovery_dir(),
        ) {
            Ok(v) => v,
            Err(err) => {
                self.record_perf(0, false, finalize_ms, snap.forced_stop_on_timeout);
                return Err(err);
            }
        };
        self.record_perf(
            size,
            final_warning.is_none(),
            finalize_ms,
            snap.forced_stop_on_timeout,
        );
        Ok(RecordingOutcome {
            output_path: final_path,
            partial_error: final_warning,
        })
    }

    fn record_perf(
        &self,
        output_bytes: u64,
        ok: bool,
        finalize_ms: Option<u64>,
        forced_timeout: bool,
    ) {
        record_perf_event(&PerfEvent {
            ts_unix: now_unix_secs(),
            kind: PerfKind::Record,
            format: self.codec.label().to_ascii_lowercase(),
            fps: self.fps,
            input_duration_ms: self.elapsed_ms,
            elapsed_ms: self.elapsed_ms,
            output_bytes,
            ok,
            finalize_ms,
            audio_mode: Some(self.effective_audio.slug().to_string()),
            forced_timeout: Some(forced_timeout),
        });
    }
}

#[cfg(test)]
#[path = "recorder_tests.rs"]
mod tests;
