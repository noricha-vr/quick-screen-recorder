use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::delegate::{DelegateSignal, ensure_latest_stop_generation};

/// Minimum finalization budget for single-audio or silent recordings.
pub(super) const STOP_SINGLE_AUDIO_MIN: Duration = Duration::from_secs(30);
/// Upper bound on finalization wait for single-audio or silent recordings.
pub(super) const STOP_SINGLE_AUDIO_MAX: Duration = Duration::from_secs(180);
/// Minimum finalization budget when BOTH system audio and microphone are enabled:
/// macOS `replayd` runs offline `RPAudioMixUtility` (`AVAssetExportSession`) after
/// `removeRecordingOutput`, which regularly takes 8+ seconds on high-resolution clips.
pub(super) const STOP_DUAL_AUDIO_MIN: Duration = Duration::from_secs(60);
/// Upper bound on finalization wait for dual-audio offline mixing.
pub(super) const STOP_DUAL_AUDIO_MAX: Duration = Duration::from_secs(600);
/// Confirmation grace period after `stopCaptureWithCompletionHandler:` is dispatched.
pub(super) const STOP_STREAM_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);
/// Grace window during startup abort while `recordingOutputDidStartRecording:` has not yet fired.
/// Upgrades automatically to the full `finalize_deadline` if `started` becomes `true` mid-abort.
pub(super) const STARTUP_UNSTARTED_GRACE: Duration = Duration::from_secs(1);

/// Computes the finalization timeout for `SCRecordingOutput` before forcing `stopCapture`.
///
/// Uses the effective audio mode (after timelapse/cadence normalization) and the recording elapsed
/// time measured at the first stop request. Because `Instant` on macOS uses `CLOCK_UPTIME_RAW`,
/// system sleep pauses the monotonic clock so `RPAudioMixUtility` keeps its full budget on wake.
pub(crate) fn compute_finalize_timeout(elapsed_ms: u64, dual_audio: bool) -> Duration {
    if dual_audio {
        Duration::from_millis(elapsed_ms.saturating_mul(2))
            .clamp(STOP_DUAL_AUDIO_MIN, STOP_DUAL_AUDIO_MAX)
    } else {
        Duration::from_millis(elapsed_ms / 2).clamp(STOP_SINGLE_AUDIO_MIN, STOP_SINGLE_AUDIO_MAX)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemovePhase {
    Idle,
    InFlight,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StopMode {
    Normal,
    StartupAbort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StopSnapshot {
    pub(super) remove_phase: RemovePhase,
    pub(super) stop_requested_at: Option<Instant>,
    pub(super) finalize_deadline: Option<Instant>,
    pub(super) forced_stop_on_timeout: bool,
    pub(super) stop_capture_sent_at: Option<Instant>,
    pub(super) worker_running: bool,
}

#[derive(Debug)]
struct StopCoordinatorState {
    recording_started_at: Instant,
    dual_audio: bool,
    remove_phase: RemovePhase,
    stop_requested_at: Option<Instant>,
    finalize_deadline: Option<Instant>,
    mode: StopMode,
    forced_stop_on_timeout: bool,
    stop_capture_sent_at: Option<Instant>,
    stop_capture_gen: Option<u64>,
    worker_running: bool,
    pending_late_start_gen: Option<u64>,
    late_start_remove_retried_gen: Option<u64>,
}

/// Shared mutex-backed coordinator unifying stop state, finalization deadline, serial worker
/// ownership, and delayed-start re-teardown across GUI, CLI, background lock hooks, and startup.
pub(super) struct StopControl {
    state: Mutex<StopCoordinatorState>,
    cvar: Condvar,
}

impl StopControl {
    pub(super) fn new(recording_started_at: Instant, dual_audio: bool) -> Self {
        Self {
            state: Mutex::new(StopCoordinatorState {
                recording_started_at,
                dual_audio,
                remove_phase: RemovePhase::Idle,
                stop_requested_at: None,
                finalize_deadline: None,
                mode: StopMode::StartupAbort,
                forced_stop_on_timeout: false,
                stop_capture_sent_at: None,
                stop_capture_gen: None,
                worker_running: false,
                pending_late_start_gen: None,
                late_start_remove_retried_gen: None,
            }),
            cvar: Condvar::new(),
        }
    }

    /// Registers a stop request at `now`. Captures `stop_requested_at` and `finalize_deadline`
    /// ONCE on the first request so UI, CLI, and background hooks always share the exact same
    /// deadline. Returns `true` when the caller must spawn the serial stop worker.
    pub(super) fn register_stop_request(&self, mode: StopMode, now: Instant) -> bool {
        let Ok(mut st) = self.state.lock() else {
            return false;
        };
        if st.stop_requested_at.is_none() {
            let elapsed_ms = now
                .saturating_duration_since(st.recording_started_at)
                .as_millis() as u64;
            let budget = compute_finalize_timeout(elapsed_ms, st.dual_audio);
            st.stop_requested_at = Some(now);
            st.finalize_deadline = Some(now + budget);
            st.mode = mode;
        } else if mode == StopMode::Normal {
            st.mode = StopMode::Normal;
        }
        if st.stop_capture_sent_at.is_some() && st.pending_late_start_gen.is_none() {
            return false;
        }
        if !st.worker_running {
            st.worker_running = true;
            return true;
        }
        false
    }

    /// Queues a post-start teardown generation after a delayed `startCapture` callback completes
    /// post-cancellation, routing through the SAME serial worker instead of bypassing ordering.
    pub(super) fn register_late_start_stop(
        &self,
        late_gen: u64,
        now: Instant,
        signal: &Arc<(Mutex<DelegateSignal>, Condvar)>,
    ) -> bool {
        let should_spawn = {
            let Ok(mut st) = self.state.lock() else {
                return false;
            };
            if st.stop_requested_at.is_none() {
                let elapsed_ms = now
                    .saturating_duration_since(st.recording_started_at)
                    .as_millis() as u64;
                let budget = compute_finalize_timeout(elapsed_ms, st.dual_audio);
                st.stop_requested_at = Some(now);
                st.finalize_deadline = Some(now + budget);
                st.mode = StopMode::StartupAbort;
            }
            st.pending_late_start_gen = Some(late_gen);
            if !st.worker_running {
                st.worker_running = true;
                true
            } else {
                false
            }
        };
        signal.1.notify_all();
        self.cvar.notify_all();
        should_spawn
    }

    /// Releases worker ownership if `thread::Builder::spawn` fails so later polls can retry.
    pub(super) fn clear_worker_running_on_spawn_error(&self) {
        if let Ok(mut st) = self.state.lock() {
            st.worker_running = false;
            self.cvar.notify_all();
        }
    }

    /// Emergency fallback ONLY when `removeRecordingOutput_error` itself has hung past the full
    /// `finalize_deadline` (`RemovePhase::InFlight`): atomically marks `forced_stop_on_timeout`,
    /// records `stop_capture_sent_at`, and returns a fresh generation so the caller can dispatch
    /// `stopCaptureWithCompletionHandler:` off the stuck worker thread to halt screen/mic capture.
    pub(super) fn claim_emergency_stop_if_remove_hung(
        &self,
        signal: &Arc<(Mutex<DelegateSignal>, Condvar)>,
        now: Instant,
    ) -> Option<u64> {
        let Ok(mut st) = self.state.lock() else {
            return None;
        };
        let deadline = st.finalize_deadline?;
        if st.remove_phase != RemovePhase::InFlight
            || st.stop_capture_sent_at.is_some()
            || now < deadline
        {
            return None;
        }
        st.forced_stop_on_timeout = true;
        let expected_gen = ensure_latest_stop_generation(signal, st.pending_late_start_gen.take());
        st.stop_capture_sent_at = Some(now);
        st.stop_capture_gen = Some(expected_gen);
        self.cvar.notify_all();
        Some(expected_gen)
    }

    pub(super) fn snapshot(&self) -> StopSnapshot {
        self.state
            .lock()
            .map(|st| StopSnapshot {
                remove_phase: st.remove_phase,
                stop_requested_at: st.stop_requested_at,
                finalize_deadline: st.finalize_deadline,
                forced_stop_on_timeout: st.forced_stop_on_timeout,
                stop_capture_sent_at: st.stop_capture_sent_at,
                worker_running: st.worker_running,
            })
            .unwrap_or(StopSnapshot {
                remove_phase: RemovePhase::Failed,
                stop_requested_at: None,
                finalize_deadline: None,
                forced_stop_on_timeout: false,
                stop_capture_sent_at: None,
                worker_running: false,
            })
    }

    pub(super) fn recording_elapsed_ms_at_stop(&self, fallback_now: Instant) -> u64 {
        self.state
            .lock()
            .map(|st| {
                let stop_t = st.stop_requested_at.unwrap_or(fallback_now);
                stop_t
                    .saturating_duration_since(st.recording_started_at)
                    .as_millis() as u64
            })
            .unwrap_or(0)
    }

    pub(super) fn finalization_elapsed_ms(&self, finished_at: Instant) -> Option<u64> {
        self.state
            .lock()
            .ok()
            .and_then(|st| st.stop_requested_at)
            .map(|t0| finished_at.saturating_duration_since(t0).as_millis() as u64)
    }

    pub(super) fn wait_until_worker_idle(&self, timeout: Duration) {
        let Ok(guard) = self.state.lock() else {
            return;
        };
        let _ = self
            .cvar
            .wait_timeout_while(guard, timeout, |st| st.worker_running);
    }

    /// Executes the strictly serialized stop protocol on the single background worker thread:
    /// 1. Calls `do_remove()` at most once (never re-invoking once `RemovePhase::Succeeded`).
    /// 2. When `do_remove()` succeeded, waits on `signal` until `finished == true` OR the
    ///    effective finalization deadline expires (upgrading `StartupAbort` to full
    ///    `finalize_deadline` immediately if `started` flips `true`).
    /// 3. Dispatches `do_stop_capture(gen)` ONLY AFTER Step 1+2 settle (never concurrently with
    ///    an in-flight remove or prematurely during dual-audio mixing).
    /// 4. If a delayed `startCapture` callback queued `pending_late_start_gen` after a pre-start
    ///    `stopCapture`, loops cleanly for that generation without double-calling `do_remove()`.
    pub(super) fn run_serial_worker_loop<R, S, N>(
        &self,
        signal: &Arc<(Mutex<DelegateSignal>, Condvar)>,
        mut do_remove: R,
        mut do_stop_capture: S,
        mut now_fn: N,
    ) where
        R: FnMut() -> bool,
        S: FnMut(u64),
        N: FnMut() -> Instant,
    {
        loop {
            // Prepare any post-start generation queued after a previous pre-start `stopCapture`.
            // Never re-run `do_remove` if it already succeeded; retry only when pre-start remove failed.
            {
                let Ok(mut st) = self.state.lock() else {
                    return;
                };
                if st.pending_late_start_gen.is_some() && st.stop_capture_sent_at.is_some() {
                    if st.remove_phase == RemovePhase::Failed {
                        st.remove_phase = RemovePhase::Idle;
                    }
                    st.stop_capture_sent_at = None;
                    st.stop_capture_gen = None;
                    st.forced_stop_on_timeout = false;
                }
            }

            // Step 1: Perform `removeRecordingOutput` at most once per active attachment.
            let should_remove = {
                let Ok(mut st) = self.state.lock() else {
                    return;
                };
                if st.remove_phase == RemovePhase::Idle {
                    st.remove_phase = RemovePhase::InFlight;
                    true
                } else {
                    false
                }
            };
            if should_remove {
                let remove_ok = do_remove();
                if let Ok(mut st) = self.state.lock() {
                    st.remove_phase = if remove_ok {
                        RemovePhase::Succeeded
                    } else {
                        RemovePhase::Failed
                    };
                    self.cvar.notify_all();
                }
            }

            // Step 2: If `removeRecordingOutput` succeeded, wait for `recordingOutputDidFinishRecording:`
            // (or `didFailWithError:`) up to the shared deadline before calling `stopCapture`.
            let remove_succeeded = self
                .state
                .lock()
                .map(|st| st.remove_phase == RemovePhase::Succeeded)
                .unwrap_or(false);
            if remove_succeeded {
                self.wait_for_recording_output_finish(signal, &mut now_fn);
            }

            // Step 3: Dispatch `stopCaptureWithCompletionHandler:` strictly after Step 1+2.
            // If a delayed `startCapture` callback arrived AFTER an initial pre-start remove
            // failed and BEFORE the first `stopCapture` was dispatched, loop back to retry
            // `removeRecordingOutput` + wait for `finished` on the newly started stream first.
            enum Step3Action {
                RetryRemove,
                SendStop(u64),
                None,
            }
            let step3 = {
                let Ok(mut st) = self.state.lock() else {
                    return;
                };
                let should_retry_remove = match st.pending_late_start_gen {
                    Some(late_gen)
                        if st.remove_phase == RemovePhase::Failed
                            && st.stop_capture_sent_at.is_none()
                            && st.late_start_remove_retried_gen != Some(late_gen) =>
                    {
                        st.late_start_remove_retried_gen = Some(late_gen);
                        st.remove_phase = RemovePhase::Idle;
                        st.forced_stop_on_timeout = false;
                        true
                    }
                    _ => false,
                };
                if should_retry_remove {
                    Step3Action::RetryRemove
                } else if st.stop_capture_sent_at.is_none() {
                    let expected_gen =
                        ensure_latest_stop_generation(signal, st.pending_late_start_gen.take());
                    st.stop_capture_sent_at = Some(now_fn());
                    st.stop_capture_gen = Some(expected_gen);
                    self.cvar.notify_all();
                    Step3Action::SendStop(expected_gen)
                } else {
                    Step3Action::None
                }
            };
            match step3 {
                Step3Action::RetryRemove => continue,
                Step3Action::SendStop(expected_gen) => do_stop_capture(expected_gen),
                Step3Action::None => {}
            }

            // Step 4: If a delayed start callback queued a post-start generation AFTER we sent a
            // pre-start `stopCapture`, loop cleanly so top-of-loop prepares that generation.
            let Ok(mut st) = self.state.lock() else {
                return;
            };
            if st.pending_late_start_gen.is_some() {
                continue;
            }
            st.worker_running = false;
            self.cvar.notify_all();
            break;
        }
    }

    fn wait_for_recording_output_finish<N>(
        &self,
        signal: &Arc<(Mutex<DelegateSignal>, Condvar)>,
        now_fn: &mut N,
    ) where
        N: FnMut() -> Instant,
    {
        let (sig_lock, sig_cvar) = &**signal;
        let Ok(mut sig_guard) = sig_lock.lock() else {
            return;
        };
        loop {
            if sig_guard.finished {
                return;
            }
            let (mode, stop_req, full_deadline) = match self.state.lock() {
                Ok(st) => {
                    let req = st.stop_requested_at.unwrap_or_else(&mut *now_fn);
                    let dl = st.finalize_deadline.unwrap_or(req + STOP_SINGLE_AUDIO_MIN);
                    (st.mode, req, dl)
                }
                Err(_) => return,
            };
            let effective_deadline = if mode == StopMode::Normal || sig_guard.started {
                full_deadline
            } else {
                full_deadline.min(stop_req + STARTUP_UNSTARTED_GRACE)
            };
            let now = now_fn();
            if now >= effective_deadline {
                if let Ok(mut st) = self.state.lock() {
                    st.forced_stop_on_timeout = true;
                    self.cvar.notify_all();
                }
                return;
            }
            let rem = effective_deadline.saturating_duration_since(now);
            let wait_chunk = rem.min(Duration::from_millis(50));
            match sig_cvar.wait_timeout(sig_guard, wait_chunk) {
                Ok((next_guard, _)) => sig_guard = next_guard,
                Err(_) => return,
            }
        }
    }
}

#[cfg(test)]
#[path = "recorder_stop_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "recorder_stop_recovery_tests.rs"]
mod recovery_tests;
