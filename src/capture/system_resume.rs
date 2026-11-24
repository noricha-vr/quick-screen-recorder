use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceScreensDidWakeNotification,
    NSWorkspaceSessionDidBecomeActiveNotification,
};
use objc2_foundation::{
    NSDistributedNotificationCenter, NSNotification, NSNotificationName, NSObjectProtocol,
    NSOperationQueue, NSString,
};

use super::{InterruptLatch, SystemInterruptReason, current_session_interrupt};

const DISTRIBUTED_SCREEN_UNLOCKED_NAME: &str = "com.apple.screenIsUnlocked";

type ObserverToken = Retained<ProtocolObject<dyn NSObjectProtocol>>;

/// Verifies an untrusted distributed `com.apple.screenIsLocked` notification against
/// WindowServer's `CGSessionCopyCurrentDictionary` before latching an interrupt.
pub(crate) fn handle_distributed_lock_notification<P>(
    latch: &InterruptLatch,
    mut session_probe: P,
) -> Option<SystemInterruptReason>
where
    P: FnMut() -> Option<SystemInterruptReason>,
{
    let verified = session_probe()?;
    let _ = latch.trigger(verified);
    latch.reason()
}

/// Verifies an untrusted distributed `com.apple.screenIsUnlocked` notification against
/// WindowServer before recording resume state.
pub(crate) fn handle_distributed_unlock_notification<P>(
    latch: &InterruptLatch,
    mut session_probe: P,
) -> bool
where
    P: FnMut() -> Option<SystemInterruptReason>,
{
    if session_probe().is_some() {
        return false;
    }
    latch.note_resumed();
    latch.has_resumed_after_interrupt()
}

pub(super) fn make_bg_observer_block(
    latch: InterruptLatch,
    reason: SystemInterruptReason,
) -> RcBlock<dyn Fn(NonNull<NSNotification>)> {
    RcBlock::new(move |_note: NonNull<NSNotification>| {
        autoreleasepool(|_| {
            let _ = latch.trigger(reason);
        });
    })
}

pub(super) fn make_ui_observer_block(
    latch: InterruptLatch,
    ui_fired: Arc<AtomicBool>,
    on_main: Arc<dyn Fn(SystemInterruptReason) + 'static>,
    reason: SystemInterruptReason,
) -> RcBlock<dyn Fn(NonNull<NSNotification>)> {
    RcBlock::new(move |_note: NonNull<NSNotification>| {
        autoreleasepool(|_| {
            let _ = latch.trigger(reason);
            let effective = latch.reason().unwrap_or(reason);
            if !ui_fired.swap(true, Ordering::AcqRel) {
                on_main(effective);
            }
        });
    })
}

pub(super) fn make_verified_bg_lock_block(
    latch: InterruptLatch,
) -> RcBlock<dyn Fn(NonNull<NSNotification>)> {
    RcBlock::new(move |_note: NonNull<NSNotification>| {
        autoreleasepool(|_| {
            let _ = handle_distributed_lock_notification(&latch, current_session_interrupt);
        });
    })
}

pub(super) fn make_verified_ui_lock_block(
    latch: InterruptLatch,
    ui_fired: Arc<AtomicBool>,
    on_main: Arc<dyn Fn(SystemInterruptReason) + 'static>,
) -> RcBlock<dyn Fn(NonNull<NSNotification>)> {
    RcBlock::new(move |_note: NonNull<NSNotification>| {
        autoreleasepool(|_| {
            if let Some(effective) =
                handle_distributed_lock_notification(&latch, current_session_interrupt)
            {
                if !ui_fired.swap(true, Ordering::AcqRel) {
                    on_main(effective);
                }
            }
        });
    })
}

/// Registers background wake/unlock observers attached to `latch` from recording start so a
/// sleep->wake completing before the AppKit main thread runs stop logic is never missed.
pub(super) fn register_bg_resume_observers(
    resume_queue: &NSOperationQueue,
    latch: InterruptLatch,
) -> (Vec<ObserverToken>, Vec<ObserverToken>) {
    let dist_center = NSDistributedNotificationCenter::defaultCenter();
    let unlock_name = NSString::from_str(DISTRIBUTED_SCREEN_UNLOCKED_NAME);
    let latch_unlock = latch.clone();
    let dist_block = RcBlock::new(move |_note: NonNull<NSNotification>| {
        autoreleasepool(|_| {
            let _ =
                handle_distributed_unlock_notification(&latch_unlock, current_session_interrupt);
        });
    });
    let dist_tokens = vec![unsafe {
        dist_center.addObserverForName_object_queue_usingBlock(
            Some(&unlock_name),
            None,
            Some(resume_queue),
            &dist_block,
        )
    }];

    let ws_center = NSWorkspace::sharedWorkspace().notificationCenter();
    let wake_names: [&NSNotificationName; 3] = unsafe {
        [
            NSWorkspaceSessionDidBecomeActiveNotification,
            NSWorkspaceScreensDidWakeNotification,
            NSWorkspaceDidWakeNotification,
        ]
    };
    let mut ws_tokens = Vec::with_capacity(3);
    for name in wake_names {
        let latch_wake = latch.clone();
        let b = RcBlock::new(move |_note: NonNull<NSNotification>| {
            autoreleasepool(|_| {
                latch_wake.note_resumed();
            });
        });
        ws_tokens.push(unsafe {
            ws_center.addObserverForName_object_queue_usingBlock(
                Some(name),
                None,
                Some(resume_queue),
                &b,
            )
        });
    }
    (dist_tokens, ws_tokens)
}

#[derive(Clone, Default)]
pub(crate) struct ResumeLatch(Arc<AtomicBool>);

impl ResumeLatch {
    pub(crate) fn mark_resumed(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub(crate) fn has_resumed(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn reset(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Tracks whether macOS has unlocked or woken since an auto-stop began, and optionally notifies
/// the AppKit main thread so deferred error alerts are neither dropped nor stuck after wake.
pub struct UnlockMonitor {
    resumed: ResumeLatch,
    distributed_tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    workspace_tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl UnlockMonitor {
    pub fn start_gui<F>(on_unlock: F) -> Self
    where
        F: Fn() + 'static,
    {
        let main_queue = NSOperationQueue::mainQueue();
        let resumed = ResumeLatch::default();
        let cb: Arc<dyn Fn() + 'static> = Arc::new(on_unlock);
        let dist_center = NSDistributedNotificationCenter::defaultCenter();
        let unlock_name = NSString::from_str(DISTRIBUTED_SCREEN_UNLOCKED_NAME);

        let make_block = |f: Arc<dyn Fn() + 'static>, flag: ResumeLatch| {
            RcBlock::new(move |_note: NonNull<NSNotification>| {
                autoreleasepool(|_| {
                    flag.mark_resumed();
                    f();
                });
            })
        };

        let mut distributed_tokens = Vec::with_capacity(1);
        let f_dist = Arc::clone(&cb);
        let flag_dist = resumed.clone();
        let b_dist = RcBlock::new(move |_note: NonNull<NSNotification>| {
            autoreleasepool(|_| {
                // Verify untrusted distributed unlock against live `CGSession` state before
                // marking resumed or flushing deferred alerts.
                if current_session_interrupt().is_none() {
                    flag_dist.mark_resumed();
                    f_dist();
                }
            });
        });
        distributed_tokens.push(unsafe {
            dist_center.addObserverForName_object_queue_usingBlock(
                Some(&unlock_name),
                None,
                Some(&main_queue),
                &b_dist,
            )
        });

        let ws_center = NSWorkspace::sharedWorkspace().notificationCenter();
        let wake_names: [&NSNotificationName; 3] = unsafe {
            [
                NSWorkspaceSessionDidBecomeActiveNotification,
                NSWorkspaceScreensDidWakeNotification,
                NSWorkspaceDidWakeNotification,
            ]
        };
        let mut workspace_tokens = Vec::with_capacity(3);
        for name in wake_names {
            let b = make_block(Arc::clone(&cb), resumed.clone());
            workspace_tokens.push(unsafe {
                ws_center.addObserverForName_object_queue_usingBlock(
                    Some(name),
                    None,
                    Some(&main_queue),
                    &b,
                )
            });
        }
        Self {
            resumed,
            distributed_tokens,
            workspace_tokens,
        }
    }

    pub fn has_resumed(&self) -> bool {
        self.resumed.has_resumed()
    }

    pub fn reset_resumed(&self) {
        self.resumed.reset();
    }
}

impl Drop for UnlockMonitor {
    fn drop(&mut self) {
        let dist_center = NSDistributedNotificationCenter::defaultCenter();
        for token in self.distributed_tokens.drain(..) {
            unsafe {
                dist_center.removeObserver(token.as_ref());
            }
        }
        let ws_center = NSWorkspace::sharedWorkspace().notificationCenter();
        for token in self.workspace_tokens.drain(..) {
            unsafe {
                ws_center.removeObserver(token.as_ref());
            }
        }
    }
}
