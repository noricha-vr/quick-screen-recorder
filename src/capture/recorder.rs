use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{AVFileTypeMPEG4, AVVideoCodecTypeH264, AVVideoCodecTypeHEVC};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_media::{CMTime, CMTimeFlags};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol, NSURL};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCRecordingOutput, SCRecordingOutputConfiguration,
    SCRecordingOutputDelegate, SCStream, SCStreamConfiguration, SCWindow,
};

use crate::capture::shareable::query_shareable_content;
use crate::capture::{CaptureConfig, CaptureTarget, RecordingCodec};
use crate::fs_safety::ExclusiveStagingDir;
use crate::observe::{PerfEvent, PerfKind, now_unix_secs, record_perf_event};

#[derive(Default)]
struct DelegateSignal {
    started: bool,
    finished: bool,
    error: Option<String>,
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

impl RecordingSession {
    pub fn start(config: &CaptureConfig, output_path: &Path) -> Result<Self, String> {
        let staging = ExclusiveStagingDir::create_for_target(output_path, "mp4")?;

        let snapshot = query_shareable_content()?;
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
            if let Some(rect) = source_rect_opt {
                stream_cfg.setSourceRect(rect);
            }
        }

        let out_ns_str = objc2_foundation::NSString::from_str(
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

        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| "Timed out starting ScreenCaptureKit stream".to_string())??;

        // Wait for SCRecordingOutput to confirm file writer initialization
        {
            let (lock, cvar) = &*signal;
            let guard = lock.lock().map_err(|_| "Mutex poisoned")?;
            let (guard, _) = cvar
                .wait_timeout_while(guard, Duration::from_secs(5), |s| {
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

    pub fn stop(self) -> Result<PathBuf, String> {
        let elapsed_ms = self.started_at.elapsed().as_millis() as u64;

        let (tx, rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let stop_block = RcBlock::new(move |err: *mut NSError| {
            if !err.is_null() {
                let msg = unsafe { (&*err).localizedDescription().to_string() };
                let _ = tx.send(Err(format!("Error stopping stream: {msg}")));
            } else {
                let _ = tx.send(Ok(()));
            }
        });

        unsafe {
            let _ = self
                .stream
                .removeRecordingOutput_error(&self.recording_output);
            self.stream
                .stopCaptureWithCompletionHandler(Some(&stop_block));
        }

        let stop_res = rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "Timed out stopping capture stream".to_string())?;

        // Wait for SCRecordingOutputDelegate::recordingOutputDidFinishRecording so MP4 moov atom is flushed
        let (finished, finish_err) = {
            let (lock, cvar) = &*self.signal;
            let guard = lock.lock().map_err(|_| "Mutex poisoned")?;
            let (guard, _) = cvar
                .wait_timeout_while(guard, Duration::from_secs(8), |s| !s.finished)
                .map_err(|_| "Mutex poisoned")?;
            (guard.finished, guard.error.clone())
        };

        if let Err(e) = stop_res {
            record_perf_event(&PerfEvent {
                ts_unix: now_unix_secs(),
                kind: PerfKind::Record,
                format: self.codec.label().to_ascii_lowercase(),
                fps: self.fps,
                input_duration_ms: elapsed_ms,
                elapsed_ms,
                output_bytes: 0,
                ok: false,
            });
            return Err(e);
        }
        if let Some(err) = finish_err {
            return Err(format!("Recording finalization error: {err}"));
        }
        if !finished {
            return Err(
                "Timed out waiting for MP4 recording output to flush container trailer".to_string(),
            );
        }

        let size = self.staging.commit()?;
        record_perf_event(&PerfEvent {
            ts_unix: now_unix_secs(),
            kind: PerfKind::Record,
            format: self.codec.label().to_ascii_lowercase(),
            fps: self.fps,
            input_duration_ms: elapsed_ms,
            elapsed_ms,
            output_bytes: size,
            ok: true,
        });

        Ok(self.output_path)
    }
}

fn build_filter_and_dimensions(
    content: &objc2_screen_capture_kit::SCShareableContent,
    config: &CaptureConfig,
) -> Result<(Retained<SCContentFilter>, usize, usize, Option<CGRect>), String> {
    match &config.target {
        CaptureTarget::Window { window_id } => {
            let sc_windows = unsafe { content.windows() };
            let mut found: Option<Retained<SCWindow>> = None;
            for i in 0..sc_windows.count() {
                let w = sc_windows.objectAtIndex(i);
                if unsafe { w.windowID() } == *window_id {
                    found = Some(w);
                    break;
                }
            }
            let win =
                found.ok_or_else(|| format!("Window ID {window_id} is no longer available"))?;
            let frame = unsafe { win.frame() };
            let filter = unsafe {
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &win)
            };
            let scale = unsafe { f64::from(filter.pointPixelScale()).max(1.0) };
            let pw = even_usize((frame.size.width * scale).round());
            let ph = even_usize((frame.size.height * scale).round());
            Ok((filter, pw, ph, None))
        }
        CaptureTarget::Display {
            display_id,
            scale_factor,
        } => {
            let display = find_display(content, *display_id)?;
            let excluded = collect_excluded_windows(content, &config.excluded_window_ids);
            let filter = unsafe {
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &excluded,
                )
            };
            let scale = if *scale_factor > 0.5 {
                *scale_factor
            } else {
                unsafe { f64::from(filter.pointPixelScale()).max(1.0) }
            };
            let dw = unsafe { display.width().max(2) as f64 };
            let dh = unsafe { display.height().max(2) as f64 };
            let pw = even_usize((dw * scale).round());
            let ph = even_usize((dh * scale).round());
            Ok((filter, pw, ph, None))
        }
        CaptureTarget::Region {
            display_id,
            rect_points,
            scale_factor,
        } => {
            let display = find_display(content, *display_id)?;
            let excluded = collect_excluded_windows(content, &config.excluded_window_ids);
            let filter = unsafe {
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &excluded,
                )
            };
            let scale = if *scale_factor > 0.5 {
                *scale_factor
            } else {
                unsafe { f64::from(filter.pointPixelScale()).max(1.0) }
            };
            let dw = unsafe { display.width().max(32) as f64 };
            let dh = unsafe { display.height().max(32) as f64 };
            let clamped = rect_points
                .clamp_and_align_for_scale(crate::geometry::Rect::new(0.0, 0.0, dw, dh), scale);
            let cg_rect = CGRect::new(
                CGPoint::new(clamped.x, clamped.y),
                CGSize::new(clamped.width, clamped.height),
            );
            let pw = even_usize((clamped.width * scale).round());
            let ph = even_usize((clamped.height * scale).round());
            Ok((filter, pw, ph, Some(cg_rect)))
        }
    }
}

fn find_display(
    content: &objc2_screen_capture_kit::SCShareableContent,
    display_id: u32,
) -> Result<Retained<SCDisplay>, String> {
    let displays = unsafe { content.displays() };
    if displays.count() == 0 {
        return Err("No active displays detected".to_string());
    }
    if display_id == 0 {
        return Ok(displays.objectAtIndex(0));
    }
    for i in 0..displays.count() {
        let d = displays.objectAtIndex(i);
        if unsafe { d.displayID() } == display_id {
            return Ok(d);
        }
    }
    Err(format!(
        "Selected display (ID {display_id}) is no longer connected"
    ))
}

fn collect_excluded_windows(
    content: &objc2_screen_capture_kit::SCShareableContent,
    extra_window_ids: &[u32],
) -> Retained<NSArray<SCWindow>> {
    let current_pid = std::process::id() as i32;
    let sc_windows = unsafe { content.windows() };
    let mut vec_win: Vec<Retained<SCWindow>> = Vec::new();
    for i in 0..sc_windows.count() {
        let w = sc_windows.objectAtIndex(i);
        let wid = unsafe { w.windowID() };
        let own_pid = unsafe { w.owningApplication().map(|a| a.processID()).unwrap_or(-1) };
        if own_pid == current_pid || extra_window_ids.contains(&wid) {
            vec_win.push(w);
        }
    }
    NSArray::from_retained_slice(&vec_win)
}

fn even_usize(v: f64) -> usize {
    let n = (v.max(32.0) as usize) & !1;
    n.max(32)
}
