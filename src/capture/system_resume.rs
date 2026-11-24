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

const DISTRIBUTED_SCREEN_UNLOCKED_NAME: &str = "com.apple.screenIsUnlocked";

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
        let b_dist = make_block(Arc::clone(&cb), resumed.clone());
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
