use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use objc2_screen_capture_kit::SCStreamConfiguration;

use crate::capture::cadence::TIMELAPSE_PLAYBACK_FPS;
use crate::capture::filter::build_filter_and_dimensions;
use crate::capture::recorder::StopPollState;
use crate::capture::shareable::query_shareable_content_for_capture;
use crate::capture::timelapse_frame::{FRAME_CAPTURE_TIMEOUT, capture_screenshot_frame};
use crate::capture::{CaptureConfig, RecordingCodec, RecordingOutcome};
use crate::export::av_writer::{StreamingMp4Writer, WRITER_FINISH_TIMEOUT};
use crate::export::avfoundation::RgbaFrame;
use crate::fs_safety::ExclusiveStagingDir;
use crate::observe::{PerfEvent, PerfKind, now_unix_secs, record_perf_event};

const START_INIT_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_CONSECUTIVE_CAPTURE_FAILURES: u32 = 3;

/// Exceeds in-flight frame capture + `AVAssetWriter` finalization (`WRITER_FINISH_TIMEOUT`) so
/// normal stop/finalization is never prematurely abandoned while the writer is still flushing.
pub(crate) const TIMELAPSE_STOP_TIMEOUT: Duration =
    Duration::from_secs(WRITER_FINISH_TIMEOUT.as_secs() + FRAME_CAPTURE_TIMEOUT.as_secs() + 5);

#[derive(Default)]
struct TimelapseControl {
    start_cancelled: bool,
    stop_requested: bool,
}

fn signal_start_cancelled(control: &(Mutex<TimelapseControl>, Condvar)) {
    let (lock, cvar) = control;
    if let Ok(mut guard) = lock.lock() {
        guard.start_cancelled = true;
        guard.stop_requested = true;
        cvar.notify_all();
    }
}

fn signal_stop(control: &(Mutex<TimelapseControl>, Condvar)) {
    let (lock, cvar) = control;
    if let Ok(mut guard) = lock.lock() {
        guard.stop_requested = true;
        cvar.notify_all();
    }
}

fn is_start_cancelled(control: &(Mutex<TimelapseControl>, Condvar)) -> bool {
    control.0.lock().map(|g| g.start_cancelled).unwrap_or(true)
}

/// Long-running interval timelapse capture session backed by `SCScreenshotManager` and
/// incremental `AVAssetWriter` encoding at 30 fps playback speed (0% idle CPU between ticks,
/// O(1) memory across multi-hour recordings, audio automatically OFF).
pub struct TimelapseSession {
    control: Arc<(Mutex<TimelapseControl>, Condvar)>,
    frames_written: Arc<AtomicU64>,
    worker_done: Arc<AtomicBool>,
    done_rx: Option<mpsc::Receiver<Result<RecordingOutcome, String>>>,
    started_at: Instant,
}

pub struct PendingTimelapseStop {
    control: Arc<(Mutex<TimelapseControl>, Condvar)>,
    done_rx: mpsc::Receiver<Result<RecordingOutcome, String>>,
    result: Option<Result<RecordingOutcome, String>>,
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

        // Rendezvous channel (capacity 0): `init_tx.send(Ok(()))` succeeds if and only if
        // `init_rx.recv_timeout(...)` actually received the handoff, eliminating start/stop races.
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(0);
        let (done_tx, done_rx) = mpsc::sync_channel::<Result<RecordingOutcome, String>>(1);
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

        let init_res = init_rx.recv_timeout(START_INIT_TIMEOUT);
        drop(init_rx);
        match init_res {
            Ok(Ok(())) => Ok(Self {
                control,
                frames_written,
                worker_done,
                done_rx: Some(done_rx),
                started_at,
            }),
            Ok(Err(err)) => {
                signal_start_cancelled(&control);
                Err(err)
            }
            Err(_) => {
                // Cancel worker immediately so delayed initialization can never continue capturing.
                signal_start_cancelled(&control);
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

    pub fn stop(self) -> Result<RecordingOutcome, String> {
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

    pub fn finish(mut self) -> Result<RecordingOutcome, String> {
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
) -> Result<RecordingOutcome, String> {
    if is_start_cancelled(&control) {
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

    let partial_error = match run_timelapse_loop(
        interval,
        &control,
        &frames_written,
        init_tx,
        || capture_screenshot_frame(&filter, &stream_cfg, pixel_w, pixel_h),
        |frame| writer.append_rgba_frame(frame),
    ) {
        Ok(LoopExit::CancelledBeforeStart) => {
            // Drop writer and staging without committing any file.
            return Err("Timelapse start cancelled before handoff".to_string());
        }
        Ok(LoopExit::Stopped) => None,
        Ok(LoopExit::AbortedAfterPartialCapture(err)) => Some(err),
        Err(e) => {
            record_timelapse_perf(cfg.codec, started_at.elapsed(), 0, false);
            return Err(e);
        }
    };

    // Always finalize & commit frames already captured before an abort so hours of timelapse aren't lost.
    if let Err(fin_err) = writer.finish() {
        record_timelapse_perf(cfg.codec, started_at.elapsed(), 0, false);
        return Err(match partial_error {
            Some(cap_err) => {
                format!("{cap_err}; additionally failed saving partial video: {fin_err}")
            }
            None => fin_err,
        });
    }
    let size = match staging.commit() {
        Ok(s) => s,
        Err(com_err) => {
            record_timelapse_perf(cfg.codec, started_at.elapsed(), 0, false);
            return Err(match partial_error {
                Some(cap_err) => {
                    format!("{cap_err}; additionally failed committing partial video: {com_err}")
                }
                None => com_err,
            });
        }
    };

    let clean_ok = partial_error.is_none();
    record_timelapse_perf(cfg.codec, started_at.elapsed(), size, clean_ok);
    Ok(RecordingOutcome {
        output_path,
        partial_error,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LoopExit {
    CancelledBeforeStart,
    Stopped,
    /// Unrecoverable capture error occurred after >= 1 frame was written; caller should still
    /// finalize and commit already-written frames and surface the error message to the user.
    AbortedAfterPartialCapture(String),
}

/// Pure, testable capture-loop state machine shared by production `TimelapseSession` and unit tests.
fn run_timelapse_loop<F, W>(
    interval: Duration,
    control: &(Mutex<TimelapseControl>, Condvar),
    frames_written: &AtomicU64,
    init_tx: mpsc::SyncSender<Result<(), String>>,
    mut capture_frame: F,
    mut write_frame: W,
) -> Result<LoopExit, String>
where
    F: FnMut() -> Result<RgbaFrame, String>,
    W: FnMut(&RgbaFrame) -> Result<(), String>,
{
    if is_start_cancelled(control) {
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
    if is_start_cancelled(control) {
        return Ok(LoopExit::CancelledBeforeStart);
    }
    if let Err(e) = write_frame(&first_frame) {
        let _ = init_tx.send(Err(e.clone()));
        return Err(e);
    }
    frames_written.store(1, Ordering::Relaxed);
    let mut held_first_frame: Option<RgbaFrame> = Some(first_frame);

    // Rendezvous handoff: returns `Err` if caller timed out and dropped `init_rx`.
    // Once `Ok(())` returns, the caller owns a live session; any subsequent `stop_requested`
    // (even immediately after start) is treated as a normal stop, never `CancelledBeforeStart`.
    if init_tx.send(Ok(())).is_err() {
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
            // Never capture a new frame after stop was requested (privacy boundary):
            // if only Frame 0 was written, duplicate that pre-stop frame once so the MP4 has >= 2 frames.
            duplicate_single_frame_if_needed(
                frames_written,
                &mut held_first_frame,
                &mut write_frame,
            );
            return Ok(LoopExit::Stopped);
        }

        if Instant::now() >= next_tick {
            match capture_frame() {
                Ok(frame) => {
                    consecutive_failures = 0;
                    if let Err(w_err) = write_frame(&frame) {
                        let captured = frames_written.load(Ordering::Relaxed);
                        return Ok(LoopExit::AbortedAfterPartialCapture(format!(
                            "Encoder write failed after {captured} frames: {w_err}"
                        )));
                    }
                    let count = frames_written.fetch_add(1, Ordering::Relaxed) + 1;
                    if count >= 2 {
                        held_first_frame = None;
                    }
                }
                Err(err) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_CONSECUTIVE_CAPTURE_FAILURES {
                        let captured = frames_written.load(Ordering::Relaxed);
                        duplicate_single_frame_if_needed(
                            frames_written,
                            &mut held_first_frame,
                            &mut write_frame,
                        );
                        return Ok(LoopExit::AbortedAfterPartialCapture(format!(
                            "Timelapse stopped after {consecutive_failures} consecutive capture failures ({captured} frames captured): {err}"
                        )));
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

fn duplicate_single_frame_if_needed<W>(
    frames_written: &AtomicU64,
    held_first_frame: &mut Option<RgbaFrame>,
    write_frame: &mut W,
) where
    W: FnMut(&RgbaFrame) -> Result<(), String>,
{
    if frames_written.load(Ordering::Relaxed) == 1 {
        if let Some(frame) = held_first_frame.take() {
            if write_frame(&frame).is_ok() {
                frames_written.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
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
