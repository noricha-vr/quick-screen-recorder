use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCRecordingOutput, SCRecordingOutputDelegate, SCStream, SCStreamErrorCode,
};

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

/// Shared one-shot guard ensuring `removeRecordingOutput` and `stopCaptureWithCompletionHandler`
/// each fire at most once across background interrupt hooks and main-thread `PendingRecordingStop`.
#[derive(Default)]
pub(super) struct StreamStopControl {
    remove_sent: AtomicBool,
    stop_capture_sent: AtomicBool,
}

impl StreamStopControl {
    pub(super) fn trigger_remove_once(
        &self,
        stream: &SCStream,
        recording_output: &SCRecordingOutput,
    ) -> bool {
        if self.remove_sent.swap(true, Ordering::AcqRel) {
            return false;
        }
        unsafe {
            stream
                .removeRecordingOutput_error(recording_output)
                .is_err()
        }
    }

    pub(super) fn mark_stop_capture_sent(&self) {
        self.stop_capture_sent.store(true, Ordering::Release);
    }

    pub(super) fn trigger_stop_capture_once(
        &self,
        stream: &SCStream,
        signal: &Arc<(Mutex<DelegateSignal>, Condvar)>,
    ) {
        if self.stop_capture_sent.swap(true, Ordering::AcqRel) {
            return;
        }
        let expected_gen = begin_stop_generation(signal);
        let sig = Arc::clone(signal);
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
            stream.stopCaptureWithCompletionHandler(Some(&stop_block));
        }
    }
}

/// Thread-safe handle allowing the background system-interrupt queue to stop an active
/// `SCStream` + `SCRecordingOutput` immediately without waiting for the AppKit main thread.
pub(super) struct SendableStreamStop {
    stream: Retained<SCStream>,
    recording_output: Retained<SCRecordingOutput>,
    signal: Arc<(Mutex<DelegateSignal>, Condvar)>,
    control: Arc<StreamStopControl>,
}

// SAFETY: Apple ScreenCaptureKit documents `SCStream::removeRecordingOutput:error:` and
// `SCStream::stopCaptureWithCompletionHandler:` as thread-safe; `Retained` manages ObjC refcounts
// atomically, and `StreamStopControl` guarantees at-most-once execution for both operations.
unsafe impl Send for SendableStreamStop {}
unsafe impl Sync for SendableStreamStop {}

impl SendableStreamStop {
    pub(super) fn new(
        stream: Retained<SCStream>,
        recording_output: Retained<SCRecordingOutput>,
        signal: Arc<(Mutex<DelegateSignal>, Condvar)>,
        control: Arc<StreamStopControl>,
    ) -> Self {
        Self {
            stream,
            recording_output,
            signal,
            control,
        }
    }

    pub(super) fn stop_now(&self, fallback_wait: Duration) {
        autoreleasepool(|_| {
            let remove_failed = self
                .control
                .trigger_remove_once(&self.stream, &self.recording_output);
            if !remove_failed {
                let (lock, cvar) = &*self.signal;
                if let Ok(guard) = lock.lock() {
                    let _ = cvar.wait_timeout_while(guard, fallback_wait, |s| !s.finished);
                }
            }
            self.control
                .trigger_stop_capture_once(&self.stream, &self.signal);
        });
    }

    /// Initiates teardown and waits up to `confirm_timeout` for ScreenCaptureKit's `stopCapture`
    /// completion handler so callers never drop `staging` while the stream could still start/write.
    pub(super) fn stop_and_wait_stopped(&self, confirm_timeout: Duration) -> bool {
        self.stop_now(Duration::from_millis(250));
        wait_for_stream_stopped(&self.signal, confirm_timeout)
    }

    /// Unconditional teardown used when a delayed `startCapture` completion handler succeeds
    /// AFTER startup was already cancelled by an interrupt/timeout. Uses the fresh `expected_gen`
    /// allocated when `start_completed` flipped so earlier pre-start callbacks are ignored.
    pub(super) fn stop_force_on_late_start(&self, expected_gen: u64) {
        self.control.mark_stop_capture_sent();
        autoreleasepool(|_| {
            let _ = unsafe {
                self.stream
                    .removeRecordingOutput_error(&self.recording_output)
            };
            let sig = Arc::clone(&self.signal);
            let stop_block = RcBlock::new(move |err: *mut NSError| {
                let stop_err = if !err.is_null() {
                    let ns_err = unsafe { &*err };
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
/// delayed, clears any earlier pre-start `-3808` stop status before running `on_late_start_cleanup`
/// so callers wait for the post-start stop completion handler to confirm success or failure.
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
