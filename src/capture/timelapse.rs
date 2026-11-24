use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::autoreleasepool;
use objc2_screen_capture_kit::SCStreamConfiguration;

use crate::capture::cadence::TIMELAPSE_PLAYBACK_FPS;
use crate::capture::filter::build_filter_and_dimensions;
use crate::capture::recorder::StopPollState;
use crate::capture::shareable::query_shareable_content_for_capture;
use crate::capture::timelapse_frame::{FRAME_CAPTURE_TIMEOUT, dispatch_screenshot_frame};
use crate::capture::timelapse_loop::{LoopExit, SyncState, run_timelapse_loop};
use crate::capture::{CaptureConfig, RecordingCodec, RecordingOutcome};
use crate::export::av_writer::{StreamingMp4Writer, WRITER_FINISH_TIMEOUT};
use crate::fs_safety::{ExclusiveStagingDir, format_tilde_path};
use crate::observe::{PerfEvent, PerfKind, now_unix_secs, record_perf_event};
use crate::staging::private_recovery_dir;

const START_INIT_TIMEOUT: Duration = Duration::from_secs(12);

/// Exceeds in-flight frame capture + `AVAssetWriter` finalization (`WRITER_FINISH_TIMEOUT`) so
/// normal stop/finalization is never prematurely abandoned while the writer is still flushing.
pub(crate) const TIMELAPSE_STOP_TIMEOUT: Duration =
    Duration::from_secs(WRITER_FINISH_TIMEOUT.as_secs() + FRAME_CAPTURE_TIMEOUT.as_secs() + 5);

/// Long-running interval timelapse capture session backed by `SCScreenshotManager` and
/// incremental `AVAssetWriter` encoding at 30 fps playback speed (0% idle CPU between ticks,
/// O(1) memory across multi-hour recordings, audio automatically OFF).
pub struct TimelapseSession {
    sync: Arc<SyncState>,
    frames_written: Arc<AtomicU64>,
    worker_done: Arc<AtomicBool>,
    done_rx: Option<mpsc::Receiver<Result<RecordingOutcome, String>>>,
    started_at: Instant,
}

pub struct PendingTimelapseStop {
    sync: Arc<SyncState>,
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

        let sync = SyncState::new();
        let sync_worker = Arc::clone(&sync);
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
                sync_worker,
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
                sync,
                frames_written,
                worker_done,
                done_rx: Some(done_rx),
                started_at,
            }),
            Ok(Err(err)) => {
                sync.signal_start_cancelled();
                Err(err)
            }
            Err(_) => {
                sync.signal_start_cancelled();
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
        self.sync.signal_stop();
        let done_rx = self
            .done_rx
            .take()
            .expect("TimelapseSession::done_rx present until stop");
        PendingTimelapseStop {
            sync: Arc::clone(&self.sync),
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
        self.sync.signal_stop();
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
        self.sync.signal_stop();
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
    sync: Arc<SyncState>,
    frames_written: Arc<AtomicU64>,
    started_at: Instant,
    init_tx: mpsc::SyncSender<Result<(), String>>,
) -> Result<RecordingOutcome, String> {
    if sync.is_start_cancelled() {
        return Err("Timelapse start cancelled before initialization".to_string());
    }
    // Drain autoreleased Cocoa intermediates created during setup on this non-main Rust thread.
    let setup_res = autoreleasepool(|_| {
        let snapshot = query_shareable_content_for_capture()?;
        let (filter, pixel_w, pixel_h, source_rect_opt) =
            build_filter_and_dimensions(&snapshot.content, &cfg)?;

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

        let writer = StreamingMp4Writer::new(
            pixel_w as u32,
            pixel_h as u32,
            TIMELAPSE_PLAYBACK_FPS,
            cfg.codec,
            staging.staged_file(),
        )?;
        Ok::<_, String>((filter, stream_cfg, pixel_w, pixel_h, writer))
    });
    let (filter, stream_cfg, pixel_w, pixel_h, mut writer) = match setup_res {
        Ok(v) => v,
        Err(e) => {
            let _ = init_tx.send(Err(e.clone()));
            return Err(e);
        }
    };

    let cancel_flag = Arc::clone(&sync.cancelled);
    let partial_error = match run_timelapse_loop(
        interval,
        &sync,
        &frames_written,
        init_tx,
        || {
            autoreleasepool(|_| {
                dispatch_screenshot_frame(&filter, &stream_cfg, pixel_w, pixel_h, &cancel_flag)
            })
        },
        |inflight| inflight.wait(),
        |frame| autoreleasepool(|_| writer.append_rgba_frame(frame)),
    ) {
        Ok(LoopExit::CancelledBeforeStart) => {
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
    if let Err(fin_err) = autoreleasepool(|_| writer.finish()) {
        record_timelapse_perf(cfg.codec, started_at.elapsed(), 0, false);
        return Err(match partial_error {
            Some(cap_err) => {
                format!("{cap_err}; additionally failed saving partial video: {fin_err}")
            }
            None => fin_err,
        });
    }
    // If the destination disconnected (e.g. external drive unmounted), rescue the finished local
    // MP4 into our private recovery folder before `ExclusiveStagingDir::drop()` cleans staging.
    let (final_path, size, final_warning) = match staging.commit() {
        Ok(s) => (output_path, s, partial_error),
        Err(com_err) => match private_recovery_dir()
            .and_then(|rec_dir| staging.preserve_to_recovery_dir(&rec_dir))
        {
            Ok((recovered_path, rec_size)) => {
                let rec_display = format_tilde_path(&recovered_path);
                let msg = match partial_error {
                    Some(cap_err) => format!(
                        "{cap_err}; also failed moving recording to destination ({com_err}). Saved locally instead: {rec_display}"
                    ),
                    None => format!(
                        "Destination became unavailable ({com_err}). Recording was saved locally instead: {rec_display}"
                    ),
                };
                (recovered_path, rec_size, Some(msg))
            }
            Err(rec_err) => {
                record_timelapse_perf(cfg.codec, started_at.elapsed(), 0, false);
                return Err(match partial_error {
                    Some(cap_err) => format!(
                        "{cap_err}; failed committing partial video ({com_err}) and local recovery also failed ({rec_err})"
                    ),
                    None => format!(
                        "Failed saving recording to destination ({com_err}) and local recovery also failed ({rec_err})"
                    ),
                });
            }
        },
    };

    let clean_ok = final_warning.is_none();
    record_timelapse_perf(cfg.codec, started_at.elapsed(), size, clean_ok);
    Ok(RecordingOutcome {
        output_path: final_path,
        partial_error: final_warning,
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
