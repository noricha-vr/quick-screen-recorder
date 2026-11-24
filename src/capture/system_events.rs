use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceScreensDidSleepNotification,
    NSWorkspaceScreensDidWakeNotification, NSWorkspaceSessionDidBecomeActiveNotification,
    NSWorkspaceSessionDidResignActiveNotification, NSWorkspaceWillSleepNotification,
};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFString, CFType};
use objc2_core_graphics::CGSessionCopyCurrentDictionary;
use objc2_foundation::{
    NSDate, NSDefaultRunLoopMode, NSDistributedNotificationCenter, NSNotification,
    NSNotificationName, NSObjectProtocol, NSOperationQueue, NSRunLoop, NSString,
};

const DISTRIBUTED_SCREEN_LOCKED_NAME: &str = "com.apple.screenIsLocked";
const DISTRIBUTED_SCREEN_UNLOCKED_NAME: &str = "com.apple.screenIsUnlocked";

pub type StopHook = Arc<dyn Fn() + Send + Sync>;

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
/// and system sleep.
///
/// Immediate stop signaling runs on a dedicated serial background `NSOperationQueue` completely
/// independent of the AppKit main thread, so even a busy/blocked GUI runloop can never delay
/// stopping capture or allow post-lock frames. A separate main-queue observer notifies GUI state.
pub struct SystemInterruptMonitor {
    latch: InterruptLatch,
    stop_hook: Arc<Mutex<Option<StopHook>>>,
    _bg_queue: Retained<NSOperationQueue>,
    distributed_tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    workspace_tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl SystemInterruptMonitor {
    /// Arms monitoring BEFORE recording startup so lock/sleep events during initialization are
    /// never missed. `on_main_interrupt` runs strictly on `NSOperationQueue::mainQueue()`, while
    /// the latch and attached stop hook execute immediately on an independent background queue.
    pub fn start_gui<F>(on_main_interrupt: F) -> Self
    where
        F: Fn(SystemInterruptReason) + 'static,
    {
        Self::start_inner(Some(Arc::new(on_main_interrupt)))
    }

    /// Arms headless monitoring for CLI sessions (background serial queue + `pump_current_runloop`).
    pub fn start_headless() -> Self {
        Self::start_inner(None)
    }

    fn start_inner(
        on_main_interrupt: Option<Arc<dyn Fn(SystemInterruptReason) + 'static>>,
    ) -> Self {
        let latch = InterruptLatch::new();
        let stop_hook: Arc<Mutex<Option<StopHook>>> = Arc::new(Mutex::new(None));
        let bg_queue = NSOperationQueue::new();
        bg_queue.setMaxConcurrentOperationCount(1);

        let mut distributed_tokens = Vec::with_capacity(2);
        let mut workspace_tokens = Vec::with_capacity(6);

        let dist_center = NSDistributedNotificationCenter::defaultCenter();
        let lock_name = NSString::from_str(DISTRIBUTED_SCREEN_LOCKED_NAME);
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

        // Layer 1: Independent background queue for zero-latency latch + immediate stop hook.
        let bg_lock_block = make_bg_observer_block(
            latch.clone(),
            Arc::clone(&stop_hook),
            SystemInterruptReason::ScreenLocked,
        );
        distributed_tokens.push(unsafe {
            dist_center.addObserverForName_object_queue_usingBlock(
                Some(&lock_name),
                None,
                Some(&bg_queue),
                &bg_lock_block,
            )
        });
        for (name, reason) in workspace_events {
            let block = make_bg_observer_block(latch.clone(), Arc::clone(&stop_hook), reason);
            workspace_tokens.push(unsafe {
                ws_center.addObserverForName_object_queue_usingBlock(
                    Some(name),
                    None,
                    Some(&bg_queue),
                    &block,
                )
            });
        }

        // Layer 2 (GUI only): Main-queue observer wakes AppKit UI state without touching Rc on bg thread.
        if let Some(cb) = on_main_interrupt {
            let main_queue = NSOperationQueue::mainQueue();
            let ui_fired = Arc::new(AtomicBool::new(false));
            let ui_lock_block = make_ui_observer_block(
                latch.clone(),
                Arc::clone(&ui_fired),
                Arc::clone(&cb),
                SystemInterruptReason::ScreenLocked,
            );
            distributed_tokens.push(unsafe {
                dist_center.addObserverForName_object_queue_usingBlock(
                    Some(&lock_name),
                    None,
                    Some(&main_queue),
                    &ui_lock_block,
                )
            });
            for (name, reason) in workspace_events {
                let block = make_ui_observer_block(
                    latch.clone(),
                    Arc::clone(&ui_fired),
                    Arc::clone(&cb),
                    reason,
                );
                workspace_tokens.push(unsafe {
                    ws_center.addObserverForName_object_queue_usingBlock(
                        Some(name),
                        None,
                        Some(&main_queue),
                        &block,
                    )
                });
            }
        }

        Self {
            latch,
            stop_hook,
            _bg_queue: bg_queue,
            distributed_tokens,
            workspace_tokens,
        }
    }

    pub fn latch(&self) -> InterruptLatch {
        self.latch.clone()
    }

    /// Attaches the newly started session's thread-safe immediate stop hook. If an interrupt
    /// already fired while the session was initializing, invokes the stop hook immediately.
    pub fn attach_stop_hook(&self, hook: StopHook) {
        if let Ok(mut guard) = self.stop_hook.lock() {
            *guard = Some(Arc::clone(&hook));
        }
        if self.latch.reason().is_some() {
            hook();
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
                invoke_attached_stop_hook(&self.stop_hook);
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

fn invoke_attached_stop_hook(slot: &Arc<Mutex<Option<StopHook>>>) {
    let hook = slot.lock().ok().and_then(|g| g.clone());
    if let Some(stop_fn) = hook {
        stop_fn();
    }
}

fn make_bg_observer_block(
    latch: InterruptLatch,
    stop_hook: Arc<Mutex<Option<StopHook>>>,
    reason: SystemInterruptReason,
) -> RcBlock<dyn Fn(NonNull<NSNotification>)> {
    RcBlock::new(move |_note: NonNull<NSNotification>| {
        autoreleasepool(|_| {
            if latch.trigger(reason) {
                invoke_attached_stop_hook(&stop_hook);
            }
        });
    })
}

fn make_ui_observer_block(
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

/// Main-thread observer used to present deferred error alerts once macOS unlocks or wakes.
pub struct UnlockMonitor {
    distributed_tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    workspace_tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl UnlockMonitor {
    pub fn start_gui<F>(on_unlock: F) -> Self
    where
        F: Fn() + 'static,
    {
        let main_queue = NSOperationQueue::mainQueue();
        let cb: Arc<dyn Fn() + 'static> = Arc::new(on_unlock);
        let dist_center = NSDistributedNotificationCenter::defaultCenter();
        let unlock_name = NSString::from_str(DISTRIBUTED_SCREEN_UNLOCKED_NAME);
        let make_block = |f: Arc<dyn Fn() + 'static>| {
            RcBlock::new(move |_note: NonNull<NSNotification>| {
                autoreleasepool(|_| f());
            })
        };
        let mut distributed_tokens = Vec::with_capacity(1);
        let b_dist = make_block(Arc::clone(&cb));
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
            let b = make_block(Arc::clone(&cb));
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
            distributed_tokens,
            workspace_tokens,
        }
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
#[path = "system_events_tests.rs"]
mod tests;
