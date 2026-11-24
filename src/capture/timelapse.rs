use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
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
use crate::export::av_writer::StreamingMp4Writer;
use crate::export::avfoundation::RgbaFrame;
use crate::fs_safety::ExclusiveStagingDir;
use crate::observe::{PerfEvent, PerfKind, now_unix_secs, record_perf_event};

const FRAME_CAPTURE_TIMEOUT: Duration = Duration::from_secs(6);
const TIMELAPSE_STOP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
struct TimelapseControl {
    stop_requested: bool,
}

/// Long-running interval timelapse capture session backed by `SCScreenshotManager` and
/// incremental `AVAssetWriter` encoding at 30 fps playback speed (0% idle CPU between ticks,
/// O(1) memory across multi-hour recordings, audio automatically OFF).
pub struct TimelapseSession {
    control: Arc<(Mutex<TimelapseControl>, Condvar)>,
    frames_written: Arc<AtomicU64>,
    done_rx: mpsc::Receiver<Result<PathBuf, String>>,
    started_at: Instant,
}

pub struct PendingTimelapseStop {
    done_rx: mpsc::Receiver<Result<PathBuf, String>>,
    result: Option<Result<PathBuf, String>>,
    stop_initiated_at: Instant,
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
            let _ = done_tx.send(outcome);
        });

        init_rx
            .recv_timeout(Duration::from_secs(12))
            .map_err(|_| "Timed out starting timelapse capture".to_string())??;

        Ok(Self {
            control,
            frames_written,
            done_rx,
            started_at,
        })
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    pub fn captured_frames(&self) -> u64 {
        self.frames_written.load(Ordering::Relaxed)
    }

    pub fn begin_stop(self) -> PendingTimelapseStop {
        let (lock, cvar) = &*self.control;
        if let Ok(mut guard) = lock.lock() {
            guard.stop_requested = true;
            cvar.notify_all();
        }
        PendingTimelapseStop {
            done_rx: self.done_rx,
            result: None,
            stop_initiated_at: Instant::now(),
        }
    }

    pub fn stop(self) -> Result<PathBuf, String> {
        let mut pending = self.begin_stop();
        match pending.done_rx.recv_timeout(TIMELAPSE_STOP_TIMEOUT) {
            Ok(res) => {
                pending.result = Some(res);
                pending.finish()
            }
            Err(_) => Err("Timed out finalizing timelapse MP4".to_string()),
        }
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
                if self.stop_initiated_at.elapsed() >= TIMELAPSE_STOP_TIMEOUT {
                    StopPollState::TimedOut
                } else {
                    StopPollState::Pending
                }
            }
        }
    }

    pub fn finish(mut self) -> Result<PathBuf, String> {
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

    // Capture Frame 0 immediately so target/permission failures surface synchronously on start
    // and even short stops have at least the opening frame.
    let first_frame = match capture_screenshot_frame(&filter, &stream_cfg, pixel_w, pixel_h) {
        Ok(f) => f,
        Err(e) => {
            let _ = init_tx.send(Err(e.clone()));
            return Err(e);
        }
    };
    if let Err(e) = writer.append_rgba_frame(&first_frame) {
        let _ = init_tx.send(Err(e.clone()));
        return Err(e);
    }
    frames_written.store(1, Ordering::Relaxed);
    let mut held_first_frame: Option<RgbaFrame> = Some(first_frame);
    let _ = init_tx.send(Ok(()));

    let mut next_tick = Instant::now() + interval;
    loop {
        let wait_for = next_tick.saturating_duration_since(Instant::now());
        let stopped = {
            let (lock, cvar) = &*control;
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
            // If stopped before the 2nd interval elapsed, grab one final frame (or duplicate Frame 0)
            // so the resulting 30 fps MP4 has >= 2 frames and valid duration in QuickTime & Editor.
            if writer.frames_written() == 1 {
                let closing = if started_at.elapsed() >= Duration::from_millis(250) {
                    capture_screenshot_frame(&filter, &stream_cfg, pixel_w, pixel_h)
                        .ok()
                        .or_else(|| held_first_frame.take())
                } else {
                    held_first_frame.take()
                };
                if let Some(frame) = closing {
                    let _ = writer.append_rgba_frame(&frame);
                    frames_written.store(writer.frames_written(), Ordering::Relaxed);
                }
            }
            break;
        }

        if Instant::now() >= next_tick {
            if let Ok(frame) = capture_screenshot_frame(&filter, &stream_cfg, pixel_w, pixel_h) {
                if let Err(e) = writer.append_rgba_frame(&frame) {
                    record_timelapse_perf(cfg.codec, started_at.elapsed(), 0, false);
                    return Err(e);
                }
                frames_written.store(writer.frames_written(), Ordering::Relaxed);
                if writer.frames_written() >= 2 {
                    held_first_frame = None;
                }
            }
            let now = Instant::now();
            next_tick += interval;
            if next_tick <= now {
                next_tick = now + interval;
            }
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
