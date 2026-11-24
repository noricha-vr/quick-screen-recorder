use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextGetData, CGColorSpace, CGContext, CGImage,
    CGImageAlphaInfo,
};
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{SCContentFilter, SCScreenshotManager, SCStreamConfiguration};

use crate::capture::cadence::{TIMELAPSE_PLAYBACK_FPS, aspect_fit_rect};
use crate::capture::filter::build_filter_and_dimensions;
use crate::capture::recorder::StopPollState;
use crate::capture::shareable::query_shareable_content_for_capture;
use crate::capture::{CaptureConfig, RecordingCodec};
use crate::export::av_writer::{StreamingMp4Writer, WRITER_FINISH_TIMEOUT};
use crate::export::avfoundation::RgbaFrame;
use crate::fs_safety::ExclusiveStagingDir;
use crate::observe::{PerfEvent, PerfKind, now_unix_secs, record_perf_event};

const START_INIT_TIMEOUT: Duration = Duration::from_secs(12);
const FRAME_CAPTURE_TIMEOUT: Duration = Duration::from_secs(6);
const MAX_CONSECUTIVE_CAPTURE_FAILURES: u32 = 3;

/// Exceeds in-flight frame capture + `AVAssetWriter` finalization (`WRITER_FINISH_TIMEOUT`) so
/// normal stop/finalization is never prematurely abandoned while the writer is still flushing.
pub(crate) const TIMELAPSE_STOP_TIMEOUT: Duration =
    Duration::from_secs(WRITER_FINISH_TIMEOUT.as_secs() + FRAME_CAPTURE_TIMEOUT.as_secs() + 5);

#[derive(Default)]
struct TimelapseControl {
    stop_requested: bool,
}

fn signal_stop(control: &(Mutex<TimelapseControl>, Condvar)) {
    let (lock, cvar) = control;
    if let Ok(mut guard) = lock.lock() {
        guard.stop_requested = true;
        cvar.notify_all();
    }
}

fn is_stop_requested(control: &(Mutex<TimelapseControl>, Condvar)) -> bool {
    control.0.lock().map(|g| g.stop_requested).unwrap_or(true)
}

/// Long-running interval timelapse capture session backed by `SCScreenshotManager` and
/// incremental `AVAssetWriter` encoding at 30 fps playback speed (0% idle CPU between ticks,
/// O(1) memory across multi-hour recordings, audio automatically OFF).
pub struct TimelapseSession {
    control: Arc<(Mutex<TimelapseControl>, Condvar)>,
    frames_written: Arc<AtomicU64>,
    worker_done: Arc<AtomicBool>,
    done_rx: Option<mpsc::Receiver<Result<PathBuf, String>>>,
    started_at: Instant,
}

pub struct PendingTimelapseStop {
    control: Arc<(Mutex<TimelapseControl>, Condvar)>,
    done_rx: mpsc::Receiver<Result<PathBuf, String>>,
    result: Option<Result<PathBuf, String>>,
    stop_initiated_at: Instant,
    timeout: Duration,
}

impl TimelapseSession {
    pub fn start(config: &CaptureConfig, output_path: &Path) -> Result<Self, String> {
        let interval_secs = config
            .cadence
            .timelapse_interval_secs()
            .ok_or("TimelapseSession requires a timelapse CaptureCadence")?;
        let staging = ExclusiveStagingDir::create_for_target(output_path, "mp4")?;
        let out_path = output_path.to_path_buf();
        let cfg = config.clone();

        let control = Arc::new((Mutex::new(TimelapseControl::default()), Condvar::new()));
        let control_worker = Arc::clone(&control);
        let frames_written = Arc::new(AtomicU64::new(0));
        let frames_worker = Arc::clone(&frames_written);
        let worker_done = Arc::new(AtomicBool::new(false));
        let done_flag_worker = Arc::clone(&worker_done);

        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let (done_tx, done_rx) = mpsc::sync_channel::<Result<PathBuf, String>>(1);
        let started_at = Instant::now();

        thread::spawn(move || {
            let outcome = run_timelapse_worker(
                cfg,
                staging,
                out_path,
                Duration::from_secs(u64::from(interval_secs)),
                control_worker,
                frames_worker,
                started_at,
                init_tx,
            );
            done_flag_worker.store(true, Ordering::Release);
            let _ = done_tx.send(outcome);
        });

        match init_rx.recv_timeout(START_INIT_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                control,
                frames_written,
                worker_done,
                done_rx: Some(done_rx),
                started_at,
            }),
            Ok(Err(err)) => {
                signal_stop(&control);
                Err(err)
            }
            Err(_) => {
                // Cancel the worker immediately so delayed initialization can never continue capturing.
                signal_stop(&control);
                Err("Timed out starting timelapse capture".to_string())
            }
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    pub fn captured_frames(&self) -> u64 {
        self.frames_written.load(Ordering::Relaxed)
    }

    /// Returns `true` if the background worker already exited (e.g. unrecoverable capture failure).
    pub fn has_worker_exited(&self) -> bool {
        self.worker_done.load(Ordering::Acquire)
    }

    pub fn begin_stop(mut self) -> PendingTimelapseStop {
        signal_stop(&self.control);
        let done_rx = self
            .done_rx
            .take()
            .expect("TimelapseSession::done_rx present until stop");
        PendingTimelapseStop {
            control: Arc::clone(&self.control),
            done_rx,
            result: None,
            stop_initiated_at: Instant::now(),
            timeout: TIMELAPSE_STOP_TIMEOUT,
        }
    }

    pub fn stop(self) -> Result<PathBuf, String> {
        let mut pending = self.begin_stop();
        match pending.done_rx.recv_timeout(pending.timeout) {
            Ok(res) => {
                pending.result = Some(res);
                pending.finish()
            }
            Err(_) => Err("Timed out finalizing timelapse MP4".to_string()),
        }
    }
}

impl Drop for TimelapseSession {
    fn drop(&mut self) {
        signal_stop(&self.control);
    }
}

impl PendingTimelapseStop {
    pub fn poll(&mut self) -> StopPollState {
        if self.result.is_some() {
            return StopPollState::Finished;
        }
        match self.done_rx.try_recv() {
            Ok(res) => {
                self.result = Some(res);
                StopPollState::Finished
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.result = Some(Err("Timelapse worker exited unexpectedly".to_string()));
                StopPollState::Finished
            }
            Err(mpsc::TryRecvError::Empty) => {
                if self.stop_initiated_at.elapsed() >= self.timeout {
                    StopPollState::TimedOut
                } else {
                    StopPollState::Pending
                }
            }
        }
    }

    pub fn finish(mut self) -> Result<PathBuf, String> {
        signal_stop(&self.control);
        if let Some(res) = self.result.take() {
            return res;
        }
        match self.done_rx.try_recv() {
            Ok(res) => res,
            Err(_) => Err("Timed out finalizing timelapse MP4".to_string()),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_timelapse_worker(
    cfg: CaptureConfig,
    staging: ExclusiveStagingDir,
    output_path: PathBuf,
    interval: Duration,
    control: Arc<(Mutex<TimelapseControl>, Condvar)>,
    frames_written: Arc<AtomicU64>,
    started_at: Instant,
    init_tx: mpsc::SyncSender<Result<(), String>>,
) -> Result<PathBuf, String> {
    if is_stop_requested(&control) {
        return Err("Timelapse start cancelled before initialization".to_string());
    }
    let snapshot = match query_shareable_content_for_capture() {
        Ok(s) => s,
        Err(e) => {
            let _ = init_tx.send(Err(e.clone()));
            return Err(e);
        }
    };
    let (filter, pixel_w, pixel_h, source_rect_opt) =
        match build_filter_and_dimensions(&snapshot.content, &cfg) {
            Ok(v) => v,
            Err(e) => {
                let _ = init_tx.send(Err(e.clone()));
                return Err(e);
            }
        };

    let stream_cfg = unsafe { SCStreamConfiguration::new() };
    unsafe {
        stream_cfg.setWidth(pixel_w);
        stream_cfg.setHeight(pixel_h);
        stream_cfg.setShowsCursor(cfg.show_cursor);
        stream_cfg.setShowMouseClicks(cfg.highlight_clicks);
        stream_cfg.setCapturesAudio(false);
        stream_cfg.setCaptureMicrophone(false);
        if let Some(rect) = source_rect_opt {
            stream_cfg.setSourceRect(rect);
        }
    }

    let mut writer = match StreamingMp4Writer::new(
        pixel_w as u32,
        pixel_h as u32,
        TIMELAPSE_PLAYBACK_FPS,
        cfg.codec,
        staging.staged_file(),
    ) {
        Ok(w) => w,
        Err(e) => {
            let _ = init_tx.send(Err(e.clone()));
            return Err(e);
        }
    };

    let loop_outcome = run_timelapse_loop(
        interval,
        &control,
        &frames_written,
        started_at,
        init_tx,
        || capture_screenshot_frame(&filter, &stream_cfg, pixel_w, pixel_h),
        |frame| writer.append_rgba_frame(frame),
    );
    match loop_outcome {
        Ok(LoopExit::CancelledBeforeStart) => {
            // Drop writer and staging without committing any file.
            return Err("Timelapse start cancelled before handoff".to_string());
        }
        Ok(LoopExit::Stopped) => {}
        Err(e) => {
            record_timelapse_perf(cfg.codec, started_at.elapsed(), 0, false);
            return Err(e);
        }
    }

    if let Err(e) = writer.finish() {
        record_timelapse_perf(cfg.codec, started_at.elapsed(), 0, false);
        return Err(e);
    }
    let size = staging.commit()?;
    record_timelapse_perf(cfg.codec, started_at.elapsed(), size, true);
    Ok(output_path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopExit {
    CancelledBeforeStart,
    Stopped,
}

/// Pure, testable capture-loop state machine shared by production `TimelapseSession` and unit tests.
fn run_timelapse_loop<F, W>(
    interval: Duration,
    control: &(Mutex<TimelapseControl>, Condvar),
    frames_written: &AtomicU64,
    started_at: Instant,
    init_tx: mpsc::SyncSender<Result<(), String>>,
    mut capture_frame: F,
    mut write_frame: W,
) -> Result<LoopExit, String>
where
    F: FnMut() -> Result<RgbaFrame, String>,
    W: FnMut(&RgbaFrame) -> Result<(), String>,
{
    if is_stop_requested(control) {
        return Ok(LoopExit::CancelledBeforeStart);
    }

    // Capture Frame 0 before reporting startup success so permission/target errors surface synchronously.
    let first_frame = match capture_frame() {
        Ok(f) => f,
        Err(e) => {
            let _ = init_tx.send(Err(e.clone()));
            return Err(e);
        }
    };
    if is_stop_requested(control) {
        return Ok(LoopExit::CancelledBeforeStart);
    }
    if let Err(e) = write_frame(&first_frame) {
        let _ = init_tx.send(Err(e.clone()));
        return Err(e);
    }
    frames_written.store(1, Ordering::Relaxed);
    let mut held_first_frame: Option<RgbaFrame> = Some(first_frame);

    // If caller already timed out and dropped `init_rx`, abort immediately without continuing capture.
    if init_tx.send(Ok(())).is_err() || is_stop_requested(control) {
        return Ok(LoopExit::CancelledBeforeStart);
    }

    let mut consecutive_failures: u32 = 0;
    let mut next_tick = Instant::now() + interval;
    loop {
        let wait_for = next_tick.saturating_duration_since(Instant::now());
        let stopped = {
            let (lock, cvar) = control;
            match lock.lock() {
                Ok(guard) => {
                    let (g, _) = cvar
                        .wait_timeout_while(guard, wait_for, |c| !c.stop_requested)
                        .unwrap_or_else(|p| p.into_inner());
                    g.stop_requested
                }
                Err(_) => true,
            }
        };

        if stopped {
            if frames_written.load(Ordering::Relaxed) == 1 {
                let closing = if started_at.elapsed() >= Duration::from_millis(250) {
                    capture_frame().ok().or_else(|| held_first_frame.take())
                } else {
                    held_first_frame.take()
                };
                if let Some(frame) = closing {
                    if write_frame(&frame).is_ok() {
                        frames_written.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            return Ok(LoopExit::Stopped);
        }

        if Instant::now() >= next_tick {
            match capture_frame() {
                Ok(frame) => {
                    consecutive_failures = 0;
                    write_frame(&frame)?;
                    let count = frames_written.fetch_add(1, Ordering::Relaxed) + 1;
                    if count >= 2 {
                        held_first_frame = None;
                    }
                }
                Err(err) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_CONSECUTIVE_CAPTURE_FAILURES {
                        let captured = frames_written.load(Ordering::Relaxed);
                        return Err(format!(
                            "Timelapse stopped after {consecutive_failures} consecutive capture failures ({captured} frames captured): {err}"
                        ));
                    }
                }
            }
            let now = Instant::now();
            next_tick += interval;
            if next_tick <= now {
                next_tick = now + interval;
            }
        }
    }
}

fn capture_screenshot_frame(
    filter: &SCContentFilter,
    stream_cfg: &SCStreamConfiguration,
    target_w: usize,
    target_h: usize,
) -> Result<RgbaFrame, String> {
    let (tx, rx) = mpsc::sync_channel::<Result<RgbaFrame, String>>(1);
    let block = RcBlock::new(move |img_ptr: *mut CGImage, err_ptr: *mut NSError| {
        if !err_ptr.is_null() {
            let msg = unsafe { (&*err_ptr).localizedDescription().to_string() };
            let _ = tx.send(Err(format!("Timelapse frame capture failed: {msg}")));
            return;
        }
        if img_ptr.is_null() {
            let _ = tx.send(Err("Timelapse frame capture returned no image".to_string()));
            return;
        }
        // Render immediately while the borrowed CGImageRef is guaranteed alive inside this callback.
        let image = unsafe { &*img_ptr };
        let res = render_cgimage_aspect_fit_rgba(image, target_w, target_h);
        let _ = tx.send(res);
    });

    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            filter,
            stream_cfg,
            Some(&block),
        );
    }

    rx.recv_timeout(FRAME_CAPTURE_TIMEOUT)
        .map_err(|_| "Timed out capturing timelapse frame".to_string())?
}

fn render_cgimage_aspect_fit_rgba(
    image: &CGImage,
    dst_w: usize,
    dst_h: usize,
) -> Result<RgbaFrame, String> {
    let src_w = CGImage::width(Some(image));
    let src_h = CGImage::height(Some(image));
    if dst_w == 0 || dst_h == 0 || src_w == 0 || src_h == 0 {
        return Err("Captured timelapse frame has zero dimensions".to_string());
    }

    let color_space = CGColorSpace::new_device_rgb().ok_or("Failed to allocate RGB color space")?;
    let bytes_per_row = dst_w * 4;
    let bitmap_info = CGImageAlphaInfo::PremultipliedLast.0;

    let ctx = unsafe {
        CGBitmapContextCreate(
            ptr::null_mut(),
            dst_w,
            dst_h,
            8,
            bytes_per_row,
            Some(&color_space),
            bitmap_info,
        )
    }
    .ok_or("Failed to create CGBitmapContext for timelapse frame")?;

    let full = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(dst_w as f64, dst_h as f64),
    );
    CGContext::set_rgb_fill_color(Some(&ctx), 0.0, 0.0, 0.0, 1.0);
    CGContext::fill_rect(Some(&ctx), full);

    let (dx, dy, dw, dh) = aspect_fit_rect(src_w as f64, src_h as f64, dst_w as f64, dst_h as f64);
    let draw_rect = CGRect::new(CGPoint::new(dx, dy), CGSize::new(dw, dh));
    CGContext::draw_image(Some(&ctx), draw_rect, Some(image));

    let data_ptr = CGBitmapContextGetData(Some(&ctx)) as *const u8;
    if data_ptr.is_null() {
        return Err("CGBitmapContext returned null pixel buffer".to_string());
    }
    let pixels = unsafe { std::slice::from_raw_parts(data_ptr, bytes_per_row * dst_h).to_vec() };
    Ok(RgbaFrame {
        width: dst_w as u32,
        height: dst_h as u32,
        pixels,
    })
}

fn record_timelapse_perf(codec: RecordingCodec, elapsed: Duration, output_bytes: u64, ok: bool) {
    let elapsed_ms = elapsed.as_millis() as u64;
    record_perf_event(&PerfEvent {
        ts_unix: now_unix_secs(),
        kind: PerfKind::Record,
        format: format!("{}-timelapse", codec.label().to_ascii_lowercase()),
        fps: TIMELAPSE_PLAYBACK_FPS,
        input_duration_ms: elapsed_ms,
        elapsed_ms,
        output_bytes,
        ok,
    });
}

#[cfg(test)]
#[path = "timelapse_tests.rs"]
mod tests;
