use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCRecordingOutput, SCRecordingOutputDelegate, SCStream, SCStreamErrorCode,
};

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
