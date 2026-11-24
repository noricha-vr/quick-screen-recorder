use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSWorkspace, NSWorkspaceScreensDidSleepNotification,
    NSWorkspaceSessionDidResignActiveNotification, NSWorkspaceWillSleepNotification,
};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFString, CFType};
use objc2_core_graphics::CGSessionCopyCurrentDictionary;
use objc2_foundation::{
    NSDate, NSDefaultRunLoopMode, NSDistributedNotificationCenter, NSNotification,
    NSNotificationName, NSObjectProtocol, NSOperationQueue, NSRunLoop, NSString,
};

const DISTRIBUTED_SCREEN_LOCKED_NAME: &str = "com.apple.screenIsLocked";

/// System-level session or power transition that invalidates screen capture on macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemInterruptReason {
    ScreenLocked,
    SessionResigned,
    DisplaySlept,
    SystemWillSleep,
}

impl SystemInterruptReason {
    fn to_code(self) -> u8 {
        match self {
            Self::ScreenLocked => 1,
            Self::SessionResigned => 2,
            Self::DisplaySlept => 3,
            Self::SystemWillSleep => 4,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::ScreenLocked),
            2 => Some(Self::SessionResigned),
            3 => Some(Self::DisplaySlept),
            4 => Some(Self::SystemWillSleep),
            _ => None,
        }
    }

    pub fn clause(self) -> &'static str {
        match self {
            Self::ScreenLocked => "the screen was locked",
            Self::SessionResigned => "the user session switched away",
            Self::DisplaySlept => "the display went to sleep",
            Self::SystemWillSleep => "the Mac went to sleep",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::ScreenLocked => "screen locked",
            Self::SessionResigned => "session switched",
            Self::DisplaySlept => "display slept",
            Self::SystemWillSleep => "system sleep",
        }
    }
}

/// Lock-free first-wins latch recording the first system interrupt seen by an active recording.
#[derive(Clone, Default)]
pub struct InterruptLatch {
    code: Arc<AtomicU8>,
}

impl InterruptLatch {
    pub fn new() -> Self {
        Self {
            code: Arc::new(AtomicU8::new(0)),
        }
    }

    /// Records `reason` if no interrupt has been recorded yet. Returns `true` only on the first trigger.
    pub fn trigger(&self, reason: SystemInterruptReason) -> bool {
        self.code
            .compare_exchange(0, reason.to_code(), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn reason(&self) -> Option<SystemInterruptReason> {
        SystemInterruptReason::from_code(self.code.load(Ordering::Acquire))
    }
}

/// Evaluates `CGSessionCopyCurrentDictionary` boolean flags into a concrete interrupt reason.
pub(crate) fn evaluate_session_lock_flags(
    screen_is_locked: Option<bool>,
    on_console: Option<bool>,
) -> Option<SystemInterruptReason> {
    if screen_is_locked == Some(true) {
        return Some(SystemInterruptReason::ScreenLocked);
    }
    if on_console == Some(false) {
        return Some(SystemInterruptReason::SessionResigned);
    }
    None
}

/// Synchronously queries macOS WindowServer session state for screen lock or console switch.
/// Returns `None` when unlocked or when running without a WindowServer session.
pub fn current_session_interrupt() -> Option<SystemInterruptReason> {
    autoreleasepool(|_| {
        let dict = CGSessionCopyCurrentDictionary()?;
        let locked = cf_dict_bool(&dict, "CGSSessionScreenIsLocked");
        let on_console = cf_dict_bool(&dict, "kCGSessionOnConsoleKey");
        evaluate_session_lock_flags(locked, on_console)
    })
}

fn cf_dict_bool(dict: &CFDictionary, key: &str) -> Option<bool> {
    let cf_key = CFString::from_str(key);
    let raw_val = unsafe { dict.value((&*cf_key as *const CFString).cast::<c_void>()) };
    if raw_val.is_null() {
        return None;
    }
    let cf_val: &CFType = unsafe { &*raw_val.cast::<CFType>() };
    if let Some(b) = cf_val.downcast_ref::<CFBoolean>() {
        return Some(b.value());
    }
    if let Some(n) = cf_val.downcast_ref::<CFNumber>() {
        return n.as_i32().map(|v| v != 0);
    }
    None
}

/// Active macOS notification monitor watching screen lock, fast user switch, display sleep,
/// and system sleep. Removes all registered observers deterministically on `Drop`.
pub struct SystemInterruptMonitor {
    latch: InterruptLatch,
    immediate_stop: Option<Arc<dyn Fn() + Send + Sync>>,
    distributed_tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    workspace_tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl SystemInterruptMonitor {
    /// Registers observers delivering onto `NSOperationQueue::mainQueue()` for GUI sessions.
    pub fn start_gui<F>(
        immediate_stop: Option<Arc<dyn Fn() + Send + Sync>>,
        on_main_interrupt: F,
    ) -> Self
    where
        F: Fn(SystemInterruptReason) + 'static,
    {
        let main_queue = NSOperationQueue::mainQueue();
        Self::start_inner(
            immediate_stop,
            Some(&main_queue),
            Some(Arc::new(on_main_interrupt)),
        )
    }

    /// Registers observers without a main-queue hop for headless CLI sessions pumped via `pump_current_runloop`.
    pub fn start_headless(immediate_stop: Option<Arc<dyn Fn() + Send + Sync>>) -> Self {
        Self::start_inner(immediate_stop, None, None)
    }

    fn start_inner(
        immediate_stop: Option<Arc<dyn Fn() + Send + Sync>>,
        queue: Option<&NSOperationQueue>,
        on_interrupt: Option<Arc<dyn Fn(SystemInterruptReason) + 'static>>,
    ) -> Self {
        let latch = InterruptLatch::new();
        let mut distributed_tokens = Vec::with_capacity(1);
        let mut workspace_tokens = Vec::with_capacity(3);

        let dist_center = NSDistributedNotificationCenter::defaultCenter();
        let lock_name = NSString::from_str(DISTRIBUTED_SCREEN_LOCKED_NAME);
        let lock_block = make_observer_block(
            latch.clone(),
            immediate_stop.clone(),
            on_interrupt.clone(),
            SystemInterruptReason::ScreenLocked,
        );
        let lock_token = unsafe {
            dist_center.addObserverForName_object_queue_usingBlock(
                Some(&lock_name),
                None,
                queue,
                &lock_block,
            )
        };
        distributed_tokens.push(lock_token);

        let ws_center = NSWorkspace::sharedWorkspace().notificationCenter();
        let workspace_events: [(&NSNotificationName, SystemInterruptReason); 3] = unsafe {
            [
                (
                    NSWorkspaceSessionDidResignActiveNotification,
                    SystemInterruptReason::SessionResigned,
                ),
                (
                    NSWorkspaceScreensDidSleepNotification,
                    SystemInterruptReason::DisplaySlept,
                ),
                (
                    NSWorkspaceWillSleepNotification,
                    SystemInterruptReason::SystemWillSleep,
                ),
            ]
        };
        for (name, reason) in workspace_events {
            let block = make_observer_block(
                latch.clone(),
                immediate_stop.clone(),
                on_interrupt.clone(),
                reason,
            );
            let token = unsafe {
                ws_center.addObserverForName_object_queue_usingBlock(
                    Some(name),
                    None,
                    queue,
                    &block,
                )
            };
            workspace_tokens.push(token);
        }

        Self {
            latch,
            immediate_stop,
            distributed_tokens,
            workspace_tokens,
        }
    }

    /// Returns the recorded interrupt reason, falling back to a synchronous `CGSession` check
    /// so missed or delayed distributed notifications are still caught promptly.
    pub fn poll_interrupt(&self) -> Option<SystemInterruptReason> {
        if let Some(reason) = self.latch.reason() {
            return Some(reason);
        }
        if let Some(reason) = current_session_interrupt() {
            if self.latch.trigger(reason) {
                if let Some(stop_fn) = &self.immediate_stop {
                    stop_fn();
                }
            }
            return self.latch.reason();
        }
        None
    }
}

impl Drop for SystemInterruptMonitor {
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

fn make_observer_block(
    latch: InterruptLatch,
    immediate_stop: Option<Arc<dyn Fn() + Send + Sync>>,
    on_interrupt: Option<Arc<dyn Fn(SystemInterruptReason) + 'static>>,
    reason: SystemInterruptReason,
) -> RcBlock<dyn Fn(NonNull<NSNotification>)> {
    RcBlock::new(move |_note: NonNull<NSNotification>| {
        autoreleasepool(|_| {
            if latch.trigger(reason) {
                if let Some(stop_fn) = &immediate_stop {
                    stop_fn();
                }
                if let Some(cb) = &on_interrupt {
                    cb(reason);
                }
            }
        });
    })
}

/// Pumps the current thread's `NSRunLoop` up to `duration` so CLI sessions receive macOS
/// workspace/distributed notifications without busy-spinning when no sources are pending.
pub fn pump_current_runloop(duration: Duration) {
    if duration.is_zero() {
        return;
    }
    let started = Instant::now();
    autoreleasepool(|_| {
        let limit = NSDate::dateWithTimeIntervalSinceNow(duration.as_secs_f64());
        let rl = NSRunLoop::currentRunLoop();
        let _ = rl.runMode_beforeDate(unsafe { NSDefaultRunLoopMode }, &limit);
    });
    let elapsed = started.elapsed();
    if let Some(rem) = duration.checked_sub(elapsed) {
        if !rem.is_zero() {
            thread::sleep(rem);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_latch_preserves_first_reason_and_fires_once() {
        let latch = InterruptLatch::new();
        assert_eq!(latch.reason(), None);
        assert!(latch.trigger(SystemInterruptReason::ScreenLocked));
        assert!(!latch.trigger(SystemInterruptReason::DisplaySlept));
        assert_eq!(latch.reason(), Some(SystemInterruptReason::ScreenLocked));
    }

    #[test]
    fn evaluate_session_lock_flags_detects_lock_and_console_resign() {
        assert_eq!(evaluate_session_lock_flags(None, None), None);
        assert_eq!(evaluate_session_lock_flags(Some(false), Some(true)), None);
        assert_eq!(
            evaluate_session_lock_flags(Some(true), Some(true)),
            Some(SystemInterruptReason::ScreenLocked)
        );
        assert_eq!(
            evaluate_session_lock_flags(Some(false), Some(false)),
            Some(SystemInterruptReason::SessionResigned)
        );
        assert_eq!(
            evaluate_session_lock_flags(Some(true), Some(false)),
            Some(SystemInterruptReason::ScreenLocked)
        );
    }

    #[test]
    fn interrupt_reason_round_trips_and_formats_messages() {
        for reason in [
            SystemInterruptReason::ScreenLocked,
            SystemInterruptReason::SessionResigned,
            SystemInterruptReason::DisplaySlept,
            SystemInterruptReason::SystemWillSleep,
        ] {
            assert_eq!(
                SystemInterruptReason::from_code(reason.to_code()),
                Some(reason)
            );
            assert!(!reason.clause().is_empty());
            assert!(!reason.short_label().is_empty());
        }
        assert_eq!(SystemInterruptReason::from_code(0), None);
        assert_eq!(SystemInterruptReason::from_code(99), None);
    }

    #[test]
    fn headless_monitor_registers_and_drops_cleanly() {
        let monitor = SystemInterruptMonitor::start_headless(None);
        pump_current_runloop(Duration::from_millis(5));
        drop(monitor);
    }
}
