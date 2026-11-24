use std::sync::{Arc, Condvar, Mutex};

use objc2::rc::Retained;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{SCRecordingOutput, SCRecordingOutputDelegate};

#[derive(Default)]
pub(super) struct DelegateSignal {
    pub(super) started: bool,
    pub(super) finished: bool,
    pub(super) error: Option<String>,
    pub(super) stream_stopped: bool,
    pub(super) stream_stop_error: Option<String>,
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
