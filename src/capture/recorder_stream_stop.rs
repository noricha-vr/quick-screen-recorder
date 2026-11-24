use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{SCRecordingOutput, SCStream, SCStreamErrorCode};

use super::{DelegateSignal, record_stop_callback_for_gen, wait_for_stream_stopped};
use crate::capture::recorder::stop::{StopControl, StopMode};

/// Thread-safe handle routing ALL `removeRecordingOutput` and `stopCaptureWithCompletionHandler`
/// dispatches through a single serialized background worker so:
/// - The AppKit main thread never blocks on `replayd` IPC or audio-mix waiting.
/// - `stopCapture` never fires prematurely while `RPAudioMixUtility` is mixing dual-audio tracks.
/// - Delayed `startCapture` callbacks after cancellation re-enter the same serialized state machine.
pub(in crate::capture::recorder) struct SendableStreamStop {
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
    pub(in crate::capture::recorder) fn new(
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

    pub(in crate::capture::recorder) fn control(&self) -> &Arc<StopControl> {
        &self.control
    }

    /// Registers a stop request and spawns the serial background worker if not already running.
    /// Returns immediately without blocking the caller (safe to invoke from `mainQueue`).
    pub(in crate::capture::recorder) fn request_stop_async(self: &Arc<Self>, mode: StopMode) {
        if self.control.register_stop_request(mode, Instant::now()) {
            self.spawn_serial_worker();
        }
    }

    /// Queues a post-start stop generation when a delayed `startCapture` succeeds AFTER startup
    /// cancellation, funneling through the same serial worker rather than bypassing ordering.
    pub(in crate::capture::recorder) fn request_late_start_stop(self: &Arc<Self>, late_gen: u64) {
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
    pub(in crate::capture::recorder) fn stop_and_wait_stopped(
        self: &Arc<Self>,
        confirm_timeout: Duration,
    ) -> bool {
        self.request_stop_async(StopMode::StartupAbort);
        wait_for_stream_stopped(&self.signal, confirm_timeout)
    }

    /// If `removeRecordingOutput` itself has hung past `finalize_deadline`, dispatches an
    /// emergency `stopCaptureWithCompletionHandler:` on a dedicated thread so screen/mic capture
    /// actually stops even when `qsr-stop-worker` is wedged in `removeRecordingOutput_error`.
    pub(in crate::capture::recorder) fn trigger_emergency_stop_if_remove_hung(
        self: &Arc<Self>,
        now: Instant,
    ) {
        if let Some(expected_gen) = self
            .control
            .claim_emergency_stop_if_remove_hung(&self.signal, now)
        {
            let stopper = Arc::clone(self);
            if thread::Builder::new()
                .name("qsr-emergency-stop".to_string())
                .spawn(move || stopper.dispatch_stop_capture(expected_gen))
                .is_err()
            {
                self.dispatch_stop_capture(expected_gen);
            }
        }
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

    fn dispatch_stop_capture(&self, expected_gen: u64) {
        autoreleasepool(|_| {
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
                |expected_gen| self.dispatch_stop_capture(expected_gen),
                Instant::now,
            );
        });
    }
}
