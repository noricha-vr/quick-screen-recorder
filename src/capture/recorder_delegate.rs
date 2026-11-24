use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCRecordingOutput, SCRecordingOutputDelegate, SCStream, SCStreamErrorCode,
};

use super::stop::{StopControl, StopMode};

#[derive(Default)]
pub(super) struct DelegateSignal {
    pub(super) start_completed: bool,
    pub(super) started: bool,
    pub(super) finished: bool,
    pub(super) error: Option<String>,
    pub(super) stop_gen: u64,
    pub(super) stream_stopped: bool,
    pub(super) stream_stop_error: Option<String>,
}

/// Returns `true` ONLY when `startCapture` has settled, `stopCapture` has completed, AND
/// `stopCapture` succeeded without error (rejecting stop errors and pre-start `-3808` races).
pub(super) fn is_cleanly_stopped(s: &DelegateSignal) -> bool {
    s.start_completed && s.stream_stopped && s.stream_stop_error.is_none()
}

/// Allocates a fresh monotonic stop generation and clears prior stop completion state so any
/// older in-flight stop callback can never mark this newer stop request as completed.
#[cfg(test)]
pub(super) fn begin_stop_generation(signal: &Arc<(Mutex<DelegateSignal>, Condvar)>) -> u64 {
    let (lock, _) = &**signal;
    if let Ok(mut guard) = lock.lock() {
        guard.stop_gen = guard.stop_gen.wrapping_add(1);
        guard.stream_stopped = false;
        guard.stream_stop_error = None;
        guard.stop_gen
    } else {
        0
    }
}

/// Ensures the dispatched stop generation is never behind `signal.stop_gen`. If an earlier
/// pre-start worker called `begin_stop_generation` after a delayed start bumped `stop_gen` to
/// `preferred_gen`, allocates a fresh strictly newer generation under the same `signal` mutex
/// so the post-start stop callback can never be dropped as stale.
pub(super) fn ensure_latest_stop_generation(
    signal: &Arc<(Mutex<DelegateSignal>, Condvar)>,
    preferred_gen: Option<u64>,
) -> u64 {
    let (lock, _) = &**signal;
    if let Ok(mut guard) = lock.lock() {
        if let Some(g) = preferred_gen {
            if guard.stop_gen == g {
                guard.stream_stopped = false;
                guard.stream_stop_error = None;
                return g;
            }
        }
        guard.stop_gen = guard.stop_gen.wrapping_add(1);
        guard.stream_stopped = false;
        guard.stream_stop_error = None;
        guard.stop_gen
    } else {
        0
    }
}

/// Records a `stopCapture` completion result ONLY if its `expected_gen` still matches the latest
/// stop generation on `signal`. Returns `false` when an obsolete pre-start callback arrives late.
pub(super) fn record_stop_callback_for_gen(
    signal: &Arc<(Mutex<DelegateSignal>, Condvar)>,
    expected_gen: u64,
    stop_err: Option<String>,
) -> bool {
    let (lock, cvar) = &**signal;
    if let Ok(mut guard) = lock.lock() {
        if guard.stop_gen != expected_gen {
            return false;
        }
        guard.stream_stopped = true;
        guard.stream_stop_error = stop_err;
        cvar.notify_all();
        return true;
    }
    false
}

/// Thread-safe handle routing ALL `removeRecordingOutput` and `stopCaptureWithCompletionHandler`
/// dispatches through a single serialized background worker so:
/// - The AppKit main thread never blocks on `replayd` IPC or audio-mix waiting.
/// - `stopCapture` never fires prematurely while `RPAudioMixUtility` is mixing dual-audio tracks.
/// - Delayed `startCapture` callbacks after cancellation re-enter the same serialized state machine.
pub(super) struct SendableStreamStop {
    stream: Retained<SCStream>,
    recording_output: Retained<SCRecordingOutput>,
    signal: Arc<(Mutex<DelegateSignal>, Condvar)>,
    control: Arc<StopControl>,
}

// SAFETY: Apple ScreenCaptureKit documents `SCStream::removeRecordingOutput:error:` and
// `SCStream::stopCaptureWithCompletionHandler:` as thread-safe; `Retained` manages ObjC refcounts
// atomically, and `StopControl` strictly serializes all operations through a single worker.
unsafe impl Send for SendableStreamStop {}
unsafe impl Sync for SendableStreamStop {}

impl SendableStreamStop {
    pub(super) fn new(
        stream: Retained<SCStream>,
        recording_output: Retained<SCRecordingOutput>,
        signal: Arc<(Mutex<DelegateSignal>, Condvar)>,
        control: Arc<StopControl>,
    ) -> Arc<Self> {
        Arc::new(Self {
            stream,
            recording_output,
            signal,
            control,
        })
    }

    pub(super) fn control(&self) -> &Arc<StopControl> {
        &self.control
    }

    /// Registers a stop request and spawns the serial background worker if not already running.
    /// Returns immediately without blocking the caller (safe to invoke from `mainQueue`).
    pub(super) fn request_stop_async(self: &Arc<Self>, mode: StopMode) {
        if self.control.register_stop_request(mode, Instant::now()) {
            self.spawn_serial_worker();
        }
    }

    /// Queues a post-start stop generation when a delayed `startCapture` succeeds AFTER startup
    /// cancellation, funneling through the same serial worker rather than bypassing ordering.
    pub(super) fn request_late_start_stop(self: &Arc<Self>, late_gen: u64) {
        if self
            .control
            .register_late_start_stop(late_gen, Instant::now(), &self.signal)
        {
            self.spawn_serial_worker();
        }
    }

    /// Initiates serialized startup-abort teardown and waits up to `confirm_timeout` for
    /// ScreenCaptureKit's `stopCapture` completion handler so callers never drop `staging` while
    /// the stream could still start/write.
    pub(super) fn stop_and_wait_stopped(self: &Arc<Self>, confirm_timeout: Duration) -> bool {
        self.request_stop_async(StopMode::StartupAbort);
        wait_for_stream_stopped(&self.signal, confirm_timeout)
    }

    fn spawn_serial_worker(self: &Arc<Self>) {
        let worker = Arc::clone(self);
        let spawned = thread::Builder::new()
            .name("qsr-stop-worker".to_string())
            .spawn(move || worker.run_worker_in_autoreleasepool());
        if spawned.is_err() {
            self.control.clear_worker_running_on_spawn_error();
        }
    }

    fn run_worker_in_autoreleasepool(&self) {
        autoreleasepool(|_| {
            self.control.run_serial_worker_loop(
                &self.signal,
                || {
                    autoreleasepool(|_| unsafe {
                        self.stream
                            .removeRecordingOutput_error(&self.recording_output)
                            .is_ok()
                    })
                },
                |expected_gen| {
                    autoreleasepool(|_| {
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
                            let _ = record_stop_callback_for_gen(&sig, expected_gen, stop_err);
                        });
                        unsafe {
                            self.stream
                                .stopCaptureWithCompletionHandler(Some(&stop_block));
                        }
                    });
                },
                Instant::now,
            );
        });
    }
}

/// Waits up to `timeout` for BOTH `startCapture` and `stopCapture` to settle, and returns `true`
/// ONLY when `is_cleanly_stopped` holds (`start_completed && stream_stopped && no stop error`).
pub(super) fn wait_for_stream_stopped(
    signal: &Arc<(Mutex<DelegateSignal>, Condvar)>,
    timeout: Duration,
) -> bool {
    let (lock, cvar) = &**signal;
    let Ok(guard) = lock.lock() else {
        return false;
    };
    let Ok((guard, _)) =
        cvar.wait_timeout_while(guard, timeout, |s| !(s.start_completed && s.stream_stopped))
    else {
        return false;
    };
    is_cleanly_stopped(&guard)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StartWaitOutcome {
    Started,
    HandOffToStop,
}

fn record_startup_abort_error(signal: &Arc<(Mutex<DelegateSignal>, Condvar)>, msg: &str) {
    if let Ok(mut guard) = signal.0.lock() {
        guard.error.get_or_insert_with(|| msg.to_string());
    }
}

/// Coordinates a potentially delayed `startCaptureWithCompletionHandler` result with concurrent
/// system-interrupt cancellation so startup never blocks 8 seconds on lock/sleep and late start
/// completions can never leave a stream running. Only returns `Err` (allowing `staging` to be
/// dropped) after `trigger_stop_and_confirm()` confirms `stream_stopped == true` AND
/// `writer_started()` is still `false`; otherwise returns `Ok(StartWaitOutcome::HandOffToStop)`
/// so `RecordingSession` keeps `staging` alive through normal stop/recovery.
pub(super) fn wait_for_capture_start_with_interrupt<F, W, S>(
    rx: &mpsc::Receiver<Result<(), String>>,
    signal: &Arc<(Mutex<DelegateSignal>, Condvar)>,
    timeout: Duration,
    cancelled_after_start: &AtomicBool,
    mut check_interrupted: F,
    mut writer_started: W,
    mut trigger_stop_and_confirm: S,
) -> Result<StartWaitOutcome, String>
where
    F: FnMut() -> Option<String>,
    W: FnMut() -> bool,
    S: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    let abort_or_handoff = |msg: String,
                            is_interrupt: bool,
                            writer_started: &mut W,
                            trigger_stop_and_confirm: &mut S|
     -> Result<StartWaitOutcome, String> {
        if !writer_started() {
            cancelled_after_start.store(true, Ordering::Release);
        }
        let stop_confirmed = trigger_stop_and_confirm();
        if writer_started() || !stop_confirmed {
            if !is_interrupt {
                record_startup_abort_error(signal, &msg);
            }
            return Ok(StartWaitOutcome::HandOffToStop);
        }
        Err(msg)
    };
    loop {
        if let Some(msg) = check_interrupted() {
            return abort_or_handoff(
                msg,
                true,
                &mut writer_started,
                &mut trigger_stop_and_confirm,
            );
        }
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            return abort_or_handoff(
                "Timed out starting ScreenCaptureKit stream".to_string(),
                false,
                &mut writer_started,
                &mut trigger_stop_and_confirm,
            );
        }
        match rx.recv_timeout(rem.min(Duration::from_millis(25))) {
            Ok(Ok(())) => {
                if let Some(msg) = check_interrupted() {
                    return abort_or_handoff(
                        msg,
                        true,
                        &mut writer_started,
                        &mut trigger_stop_and_confirm,
                    );
                }
                if let Ok(mut guard) = signal.0.lock() {
                    guard.start_completed = true;
                    signal.1.notify_all();
                }
                return Ok(StartWaitOutcome::Started);
            }
            Ok(Err(err)) => return Err(err),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return abort_or_handoff(
                    "ScreenCaptureKit start handler disconnected".to_string(),
                    false,
                    &mut writer_started,
                    &mut trigger_stop_and_confirm,
                );
            }
        }
    }
}

/// Invoked inside the `startCaptureWithCompletionHandler` completion block. Updates
/// `start_completed` on `signal`, and if startup was already cancelled while this callback was
/// delayed, clears any earlier pre-start `-3808` stop status before routing `late_gen` through
/// `on_late_start_cleanup` into the shared serial stop coordinator.
pub(super) fn handle_start_callback_result<C>(
    res: Result<(), String>,
    signal: &Arc<(Mutex<DelegateSignal>, Condvar)>,
    cancelled_after_start: &AtomicBool,
    tx: &mpsc::SyncSender<Result<(), String>>,
    on_late_start_cleanup: C,
) where
    C: FnOnce(u64),
{
    let reset_for_late_stop = || -> u64 {
        let (lock, cvar) = &**signal;
        if let Ok(mut guard) = lock.lock() {
            guard.start_completed = true;
            guard.stop_gen = guard.stop_gen.wrapping_add(1);
            guard.stream_stopped = false;
            guard.stream_stop_error = None;
            let next_gen = guard.stop_gen;
            cvar.notify_all();
            next_gen
        } else {
            0
        }
    };
    if res.is_err() {
        let (lock, cvar) = &**signal;
        if let Ok(mut guard) = lock.lock() {
            guard.start_completed = true;
            guard.stop_gen = guard.stop_gen.wrapping_add(1);
            guard.stream_stopped = true;
            guard.stream_stop_error = None;
            cvar.notify_all();
        }
        let _ = tx.send(res);
        return;
    }
    if cancelled_after_start.load(Ordering::Acquire) {
        let late_gen = reset_for_late_stop();
        on_late_start_cleanup(late_gen);
        return;
    }
    if tx.send(res).is_err() || cancelled_after_start.load(Ordering::Acquire) {
        let late_gen = reset_for_late_stop();
        on_late_start_cleanup(late_gen);
        return;
    }
    let (lock, cvar) = &**signal;
    if let Ok(mut guard) = lock.lock() {
        guard.start_completed = true;
        cvar.notify_all();
    }
}

pub(super) struct DelegateIvars {
    signal: Arc<(Mutex<DelegateSignal>, Condvar)>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "QSRRecordingOutputDelegate"]
    #[ivars = DelegateIvars]
    pub(super) struct RecordingDelegate;

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
    pub(super) fn new(signal: Arc<(Mutex<DelegateSignal>, Condvar)>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { signal });
        // SAFETY: Calling `[super init]` on freshly allocated NSObject subclass.
        unsafe { msg_send![super(this), init] }
    }
}

/// Waits up to `timeout` for `recordingOutputDidStartRecording:` while polling `check_interrupted`
/// every 50ms. Only returns `Err` (dropping `staging`) when `trigger_stop_and_confirm()` confirms
/// `stream_stopped == true` AND `guard.started` is still `false` afterward; otherwise returns
/// `Ok(())` so `RecordingSession` preserves `staging` through normal stop/recovery.
pub(super) fn wait_for_recording_started<F, S>(
    signal: &Arc<(Mutex<DelegateSignal>, Condvar)>,
    timeout: Duration,
    mut check_interrupted: F,
    mut trigger_stop_and_confirm: S,
) -> Result<(), String>
where
    F: FnMut() -> Option<String>,
    S: FnMut() -> bool,
{
    let (lock, cvar) = &**signal;
    let deadline = Instant::now() + timeout;
    let mut guard = lock.lock().map_err(|_| "Mutex poisoned")?;
    while !guard.started && guard.error.is_none() {
        if let Some(msg) = check_interrupted() {
            drop(guard);
            let stop_confirmed = trigger_stop_and_confirm();
            let started = lock.lock().map(|g| g.started).unwrap_or(true);
            if started || !stop_confirmed {
                return Ok(());
            }
            return Err(msg);
        }
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            break;
        }
        let (next_guard, _) = cvar
            .wait_timeout(guard, rem.min(Duration::from_millis(50)))
            .map_err(|_| "Mutex poisoned")?;
        guard = next_guard;
    }
    if guard.started {
        drop(guard);
        if check_interrupted().is_some() {
            let _ = trigger_stop_and_confirm();
        }
        return Ok(());
    }
    if let Some(err) = guard.error.clone() {
        drop(guard);
        let stop_confirmed = trigger_stop_and_confirm();
        let started = lock.lock().map(|g| g.started).unwrap_or(true);
        if started || !stop_confirmed {
            return Ok(());
        }
        return Err(format!("Recording output failed to start: {err}"));
    }
    drop(guard);
    let stop_confirmed = trigger_stop_and_confirm();
    let started = lock.lock().map(|g| g.started).unwrap_or(true);
    let timeout_msg = "Timed out waiting for hardware recording writer to start".to_string();
    if started || !stop_confirmed {
        record_startup_abort_error(signal, &timeout_msg);
        return Ok(());
    }
    Err(timeout_msg)
}
