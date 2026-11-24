use std::path::Path;
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

use crate::capture::recorder::{
    STOP_FALLBACK_AFTER, STOP_STREAM_CONFIRM_TIMEOUT, STOP_TIMEOUT, StopPollState,
};
use crate::export::avfoundation::{extract_rgba_frame, probe_video_metadata};

#[derive(Default)]
pub(super) struct DelegateSignal {
    pub(super) started: bool,
    pub(super) finished: bool,
    pub(super) error: Option<String>,
    pub(super) stream_stopped: bool,
    pub(super) stream_stop_error: Option<String>,
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

    pub(super) fn trigger_stop_capture_once(
        &self,
        stream: &SCStream,
        signal: &Arc<(Mutex<DelegateSignal>, Condvar)>,
    ) {
        if self.stop_capture_sent.swap(true, Ordering::AcqRel) {
            return;
        }
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
            let (lock, cvar) = &*sig;
            if let Ok(mut guard) = lock.lock() {
                guard.stream_stopped = true;
                if stop_err.is_some() {
                    guard.stream_stop_error = stop_err;
                }
                cvar.notify_all();
            }
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
    /// AFTER startup was already cancelled by an interrupt/timeout (earlier pre-start stop calls
    /// may have returned `-3808` before the stream transitioned to running).
    pub(super) fn stop_force_on_late_start(&self) {
        autoreleasepool(|_| {
            let _ = unsafe {
                self.stream
                    .removeRecordingOutput_error(&self.recording_output)
            };
            unsafe {
                self.stream.stopCaptureWithCompletionHandler(None);
            }
        });
    }
}

pub(super) fn wait_for_stream_stopped(
    signal: &Arc<(Mutex<DelegateSignal>, Condvar)>,
    timeout: Duration,
) -> bool {
    let (lock, cvar) = &**signal;
    let Ok(guard) = lock.lock() else {
        return false;
    };
    let Ok((guard, _)) = cvar.wait_timeout_while(guard, timeout, |s| !s.stream_stopped) else {
        return false;
    };
    guard.stream_stopped
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

/// Invoked inside the `startCaptureWithCompletionHandler` completion block. If startup was
/// already cancelled while this callback was delayed, runs `on_late_start_cleanup` so a newly
/// running stream is torn down immediately instead of remaining active.
pub(super) fn handle_start_callback_result<C>(
    res: Result<(), String>,
    cancelled_after_start: &AtomicBool,
    tx: &mpsc::SyncSender<Result<(), String>>,
    on_late_start_cleanup: C,
) where
    C: FnOnce(),
{
    if res.is_ok() && cancelled_after_start.load(Ordering::Acquire) {
        on_late_start_cleanup();
        return;
    }
    if tx.send(res).is_err() || cancelled_after_start.load(Ordering::Acquire) {
        on_late_start_cleanup();
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

/// Pure transition table for [`crate::capture::recorder::PendingRecordingStop::poll`].
/// Returns `(next_poll_state, should_send_stop_capture)`.
pub(super) fn evaluate_stop_poll(
    finished: bool,
    stream_stopped: bool,
    stop_sent_elapsed: Option<Duration>,
    total_elapsed: Duration,
) -> (StopPollState, bool) {
    if finished && stream_stopped {
        return (StopPollState::Finished, stop_sent_elapsed.is_none());
    }
    if stop_sent_elapsed.is_none() && (finished || total_elapsed >= STOP_FALLBACK_AFTER) {
        return (StopPollState::Pending, true);
    }
    let timed_out = match stop_sent_elapsed {
        Some(sent_for) if finished => sent_for >= STOP_STREAM_CONFIRM_TIMEOUT,
        Some(sent_for) => total_elapsed >= STOP_TIMEOUT && sent_for >= STOP_STREAM_CONFIRM_TIMEOUT,
        None => false,
    };
    let state = if timed_out {
        StopPollState::TimedOut
    } else {
        StopPollState::Pending
    };
    (state, false)
}

/// Strictly verifies that `path` is a non-empty playable video file with at least one playable
/// video track and a decodable initial frame (rejecting non-video files and truncated MP4s).
pub fn is_valid_salvageable_mp4(path: &Path) -> bool {
    let Ok(meta_fs) = std::fs::metadata(path) else {
        return false;
    };
    if !meta_fs.is_file() || meta_fs.len() == 0 {
        return false;
    }
    let Ok(meta) = probe_video_metadata(path) else {
        return false;
    };
    meta.has_video
        && meta.is_playable
        && meta.duration_sec >= 0.05
        && extract_rgba_frame(path, 0.0, Some(64)).is_ok()
}

pub(super) fn format_stop_error_with_preservation(
    stop_err: &str,
    staging: &mut crate::fs_safety::ExclusiveStagingDir,
) -> String {
    match crate::staging::preserve_staged_permanently(
        staging,
        crate::staging::private_recovery_dir(),
    ) {
        Ok(saved_path) => format!(
            "{stop_err}. Playable footage was preserved at {}",
            crate::fs_safety::format_tilde_path(&saved_path)
        ),
        Err((rec_err, kept_path)) => format!(
            "{stop_err} (recovery failed: {rec_err}). Playable footage was preserved in staging at {}",
            crate::fs_safety::format_tilde_path(&kept_path)
        ),
    }
}
