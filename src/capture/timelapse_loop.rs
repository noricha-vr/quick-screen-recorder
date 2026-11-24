use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::export::avfoundation::RgbaFrame;

pub(crate) const MAX_CONSECUTIVE_CAPTURE_FAILURES: u32 = 3;

#[derive(Default)]
pub(crate) struct TimelapseControl {
    pub(crate) start_cancelled: bool,
    pub(crate) stop_requested: bool,
}

pub(crate) struct SyncState {
    pub(crate) control: (Mutex<TimelapseControl>, Condvar),
    /// Fast lock-free mirror of `start_cancelled || stop_requested` checked by ObjC callbacks.
    pub(crate) cancelled: Arc<AtomicBool>,
}

impl SyncState {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            control: (Mutex::new(TimelapseControl::default()), Condvar::new()),
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn signal_start_cancelled(&self) {
        let (lock, cvar) = &self.control;
        if let Ok(mut guard) = lock.lock() {
            guard.start_cancelled = true;
            guard.stop_requested = true;
            self.cancelled.store(true, Ordering::Release);
            cvar.notify_all();
        } else {
            self.cancelled.store(true, Ordering::Release);
        }
    }

    pub(crate) fn signal_stop(&self) {
        let (lock, cvar) = &self.control;
        if let Ok(mut guard) = lock.lock() {
            guard.stop_requested = true;
            self.cancelled.store(true, Ordering::Release);
            cvar.notify_all();
        } else {
            self.cancelled.store(true, Ordering::Release);
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn is_start_cancelled(&self) -> bool {
        self.control
            .0
            .lock()
            .map(|g| g.start_cancelled)
            .unwrap_or(true)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoopExit {
    CancelledBeforeStart,
    Stopped,
    /// Unrecoverable capture error occurred after >= 1 frame was written; caller should still
    /// finalize and commit already-written frames and surface the error message to the user.
    AbortedAfterPartialCapture(String),
}

/// Pure, testable two-phase capture loop.
/// `dispatch_capture` is invoked ONLY while holding `sync.control`'s mutex after verifying
/// stop/cancel and `is_interrupted()` are false, so no capture request can ever start after
/// stop/cancel or macOS screen lock/sleep (even if OS notifications are delayed). Any in-flight
/// capture completing after stop/cancel or lock/sleep is discarded without calling `write_frame`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_timelapse_loop<T, I, D, A, W>(
    interval: Duration,
    sync: &SyncState,
    frames_written: &AtomicU64,
    init_tx: mpsc::SyncSender<Result<(), String>>,
    mut is_interrupted: I,
    mut dispatch_capture: D,
    mut await_capture: A,
    mut write_frame: W,
) -> Result<LoopExit, String>
where
    I: FnMut() -> bool,
    D: FnMut() -> T,
    A: FnMut(T) -> Result<RgbaFrame, String>,
    W: FnMut(&RgbaFrame) -> Result<(), String>,
{
    // Atomically check start cancellation and session lock state before dispatching Frame 0.
    let first_inflight = {
        let guard = match sync.control.0.lock() {
            Ok(g) => g,
            Err(_) => return Ok(LoopExit::CancelledBeforeStart),
        };
        if guard.start_cancelled || guard.stop_requested || is_interrupted() {
            return Ok(LoopExit::CancelledBeforeStart);
        }
        dispatch_capture()
    };

    let first_frame = match await_capture(first_inflight) {
        Ok(f) => f,
        Err(e) => {
            if sync.is_start_cancelled() || is_interrupted() {
                return Ok(LoopExit::CancelledBeforeStart);
            }
            let _ = init_tx.send(Err(e.clone()));
            return Err(e);
        }
    };
    if sync.is_cancelled() || is_interrupted() {
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
    // is handled as a normal stop, never `CancelledBeforeStart`.
    if init_tx.send(Ok(())).is_err() {
        return Ok(LoopExit::CancelledBeforeStart);
    }

    let mut consecutive_failures: u32 = 0;
    let mut next_tick = Instant::now() + interval;
    loop {
        // Wait until `next_tick` OR `stop_requested`, then dispatch the next capture while STILL
        // holding the control mutex so `signal_stop()` and capture dispatch are strictly serialized.
        let inflight_opt: Option<T> = {
            let (lock, cvar) = &sync.control;
            let mut guard = match lock.lock() {
                Ok(g) => g,
                Err(_) => {
                    return Ok(finalize_stopped_loop(
                        frames_written,
                        &mut held_first_frame,
                        &mut write_frame,
                    ));
                }
            };
            while !guard.stop_requested {
                let wait_for = next_tick.saturating_duration_since(Instant::now());
                if wait_for.is_zero() {
                    break;
                }
                let (next_guard, _) = cvar
                    .wait_timeout(guard, wait_for)
                    .unwrap_or_else(|p| p.into_inner());
                guard = next_guard;
            }
            if guard.stop_requested || is_interrupted() {
                None
            } else {
                Some(dispatch_capture())
            }
        };

        let Some(inflight) = inflight_opt else {
            return Ok(finalize_stopped_loop(
                frames_written,
                &mut held_first_frame,
                &mut write_frame,
            ));
        };

        let capture_res = await_capture(inflight);
        // If stop or screen lock/sleep occurred while capture was in flight, discard the frame
        // without writing it to disk so post-stop/post-lock screen content can never enter the MP4.
        if sync.is_cancelled() || is_interrupted() {
            return Ok(finalize_stopped_loop(
                frames_written,
                &mut held_first_frame,
                &mut write_frame,
            ));
        }

        match capture_res {
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
                    let closing_note = match duplicate_single_frame_if_needed(
                        frames_written,
                        &mut held_first_frame,
                        &mut write_frame,
                    ) {
                        Ok(()) => String::new(),
                        Err(w_err) => format!("; closing frame write also failed: {w_err}"),
                    };
                    return Ok(LoopExit::AbortedAfterPartialCapture(format!(
                        "Timelapse stopped after {consecutive_failures} consecutive capture failures ({captured} frames captured): {err}{closing_note}"
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

fn finalize_stopped_loop<W>(
    frames_written: &AtomicU64,
    held_first_frame: &mut Option<RgbaFrame>,
    write_frame: &mut W,
) -> LoopExit
where
    W: FnMut(&RgbaFrame) -> Result<(), String>,
{
    match duplicate_single_frame_if_needed(frames_written, held_first_frame, write_frame) {
        Ok(()) => LoopExit::Stopped,
        Err(w_err) => LoopExit::AbortedAfterPartialCapture(format!(
            "Failed writing closing frame after {} frame(s): {w_err}",
            frames_written.load(Ordering::Relaxed)
        )),
    }
}

fn duplicate_single_frame_if_needed<W>(
    frames_written: &AtomicU64,
    held_first_frame: &mut Option<RgbaFrame>,
    write_frame: &mut W,
) -> Result<(), String>
where
    W: FnMut(&RgbaFrame) -> Result<(), String>,
{
    if frames_written.load(Ordering::Relaxed) == 1 {
        if let Some(frame) = held_first_frame.take() {
            write_frame(&frame)?;
            frames_written.fetch_add(1, Ordering::Relaxed);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    fn dummy(tag: u8) -> RgbaFrame {
        RgbaFrame {
            width: 4,
            height: 4,
            pixels: vec![tag; 4 * 4 * 4],
        }
    }

    #[test]
    fn delayed_notification_session_lock_blocks_dispatch_and_discards_inflight_frames() {
        let sync = SyncState::new();
        let sync_w = Arc::clone(&sync);
        let frames = Arc::new(AtomicU64::new(0));
        let frames_w = Arc::clone(&frames);
        let locked = Arc::new(AtomicBool::new(false));
        let locked_w = Arc::clone(&locked);
        let dispatches = Arc::new(AtomicU64::new(0));
        let dispatches_w = Arc::clone(&dispatches);
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(0);

        let handle = thread::spawn(move || {
            run_timelapse_loop(
                Duration::from_millis(25),
                &sync_w,
                &frames_w,
                init_tx,
                || locked_w.load(Ordering::Acquire),
                || dispatches_w.fetch_add(1, Ordering::SeqCst) + 1,
                |n| Ok(dummy(n as u8)),
                |_| Ok(()),
            )
        });
        assert_eq!(init_rx.recv_timeout(Duration::from_secs(2)), Ok(Ok(())));
        locked.store(true, Ordering::Release);
        assert_eq!(handle.join().unwrap(), Ok(LoopExit::Stopped));
        assert_eq!(dispatches.load(Ordering::SeqCst), 1);

        let sync2 = SyncState::new();
        let sync2_w = Arc::clone(&sync2);
        let frames2 = Arc::new(AtomicU64::new(0));
        let frames2_w = Arc::clone(&frames2);
        let locked2 = Arc::new(AtomicBool::new(false));
        let locked2_w = Arc::clone(&locked2);
        let tags = Arc::new(Mutex::new(Vec::<u8>::new()));
        let tags_w = Arc::clone(&tags);
        let (d2_tx, d2_rx) = mpsc::sync_channel::<()>(1);
        let (rel_tx, rel_rx) = mpsc::sync_channel::<()>(1);
        let (init_tx2, init_rx2) = mpsc::sync_channel::<Result<(), String>>(0);

        let h2 = thread::spawn(move || {
            let mut seq = 0_u8;
            run_timelapse_loop(
                Duration::from_millis(5),
                &sync2_w,
                &frames2_w,
                init_tx2,
                || locked2_w.load(Ordering::Acquire),
                || {
                    seq += 1;
                    if seq == 2 {
                        let _ = d2_tx.send(());
                    }
                    seq
                },
                |n| {
                    if n == 2 {
                        let _ = rel_rx.recv();
                    }
                    Ok(dummy(n))
                },
                |f| {
                    tags_w.lock().unwrap().push(f.pixels[0]);
                    Ok(())
                },
            )
        });
        assert_eq!(init_rx2.recv_timeout(Duration::from_secs(2)), Ok(Ok(())));
        d2_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        locked2.store(true, Ordering::Release);
        rel_tx.send(()).unwrap();
        assert_eq!(h2.join().unwrap(), Ok(LoopExit::Stopped));
        assert_eq!(*tags.lock().unwrap(), vec![1, 1]);
    }
}
