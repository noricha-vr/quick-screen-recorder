use std::ffi::c_void;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSWorkspace, NSWorkspaceScreensDidSleepNotification,
    NSWorkspaceSessionDidResignActiveNotification, NSWorkspaceWillSleepNotification,
};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFString, CFType};
use objc2_core_graphics::CGSessionCopyCurrentDictionary;
use objc2_foundation::{
    NSDistributedNotificationCenter, NSNotificationName, NSObjectProtocol, NSOperationQueue,
    NSString,
};

#[path = "system_resume.rs"]
mod resume;

#[cfg(test)]
pub(crate) use resume::{
    ResumeLatch, handle_distributed_lock_notification, handle_distributed_unlock_notification,
};
pub use resume::{UnlockMonitor, pump_current_runloop};

const DISTRIBUTED_SCREEN_LOCKED_NAME: &str = "com.apple.screenIsLocked";
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(100);

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

#[derive(Default)]
struct CoordinatorState {
    reason: Option<SystemInterruptReason>,
    resumed_after_interrupt: bool,
    hook: Option<StopHook>,
    hook_invoked: bool,
}

/// Thread-safe first-wins interrupt latch & at-most-once stop-hook coordinator.
///
/// Decouples recording the first interrupt reason from stop-hook invocation so regardless of
/// whether the UI observer, background notification observer, or independent watchdog thread
/// triggers first—or whether `attach_stop_hook` runs before or after an interrupt—the attached
/// stop hook is guaranteed to run exactly once.
#[derive(Clone, Default)]
pub struct InterruptLatch {
    inner: Arc<Mutex<CoordinatorState>>,
}

impl InterruptLatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `reason` (first-wins) and invokes any attached stop hook if not yet invoked.
    /// Returns `true` if this call recorded the first interrupt reason.
    pub fn trigger(&self, reason: SystemInterruptReason) -> bool {
        let (first, hook_to_run) = match self.inner.lock() {
            Ok(mut guard) => {
                let first = if guard.reason.is_none() {
                    guard.reason = Some(reason);
                    guard.resumed_after_interrupt = false;
                    true
                } else {
                    false
                };
                let hook = if !guard.hook_invoked {
                    if let Some(h) = guard.hook.clone() {
                        guard.hook_invoked = true;
                        Some(h)
                    } else {
                        None
                    }
                } else {
                    None
                };
                (first, hook)
            }
            Err(_) => (false, None),
        };
        if let Some(hook) = hook_to_run {
            hook();
        }
        first
    }

    /// Attaches a session's immediate stop hook. If an interrupt already fired before attachment,
    /// invokes the hook immediately outside the mutex.
    pub fn attach_stop_hook(&self, hook: StopHook) {
        let hook_to_run = match self.inner.lock() {
            Ok(mut guard) => {
                guard.hook = Some(Arc::clone(&hook));
                if guard.reason.is_some() && !guard.hook_invoked {
                    guard.hook_invoked = true;
                    Some(hook)
                } else {
                    None
                }
            }
            Err(_) => None,
        };
        if let Some(h) = hook_to_run {
            h();
        }
    }

    pub fn reason(&self) -> Option<SystemInterruptReason> {
        self.inner.lock().ok().and_then(|g| g.reason)
    }

    pub fn is_interrupted(&self) -> bool {
        self.reason().is_some()
    }

    /// Records that macOS unlocked/woke AFTER an interrupt was latched (no-op before any interrupt).
    pub fn note_resumed(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            if guard.reason.is_some() {
                guard.resumed_after_interrupt = true;
            }
        }
    }

    /// Returns `true` if a wake/unlock event has already been observed after this latch triggered.
    pub fn has_resumed_after_interrupt(&self) -> bool {
        self.inner
            .lock()
            .map(|g| g.resumed_after_interrupt)
            .unwrap_or(false)
    }
}

/// Result of probing WindowServer's `CGSessionCopyCurrentDictionary()`, explicitly distinguishing
/// lookup failures from a confirmed unlocked console session so untrusted distributed unlock
/// notifications can never fail open when the dictionary is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionProbeState {
    ActiveUnlocked,
    Interrupted(SystemInterruptReason),
    Unavailable,
}

/// Evaluates `CGSessionCopyCurrentDictionary` boolean flags into a 3-state probe result.
pub(crate) fn evaluate_session_state(
    dict_present: bool,
    screen_is_locked: Option<bool>,
    on_console: Option<bool>,
) -> SessionProbeState {
    if !dict_present {
        return SessionProbeState::Unavailable;
    }
    if screen_is_locked == Some(true) {
        return SessionProbeState::Interrupted(SystemInterruptReason::ScreenLocked);
    }
    if on_console == Some(false) {
        return SessionProbeState::Interrupted(SystemInterruptReason::SessionResigned);
    }
    // On macOS, `CGSSessionScreenIsLocked` is omitted until the session has been locked at least
    // once, while `kCGSSessionOnConsoleKey` is `1` on an active unlocked desktop console.
    if on_console == Some(true) {
        return SessionProbeState::ActiveUnlocked;
    }
    SessionProbeState::Unavailable
}

/// Synchronously queries macOS WindowServer session state for screen lock or console switch.
/// Completely independent of AppKit and the main thread's `NSRunLoop`.
pub fn current_session_state() -> SessionProbeState {
    autoreleasepool(|_| {
        let Some(dict) = CGSessionCopyCurrentDictionary() else {
            return SessionProbeState::Unavailable;
        };
        let locked = cf_dict_bool(&dict, "CGSSessionScreenIsLocked");
        // Apple's `kCGSessionOnConsoleKey` C constant resolves to `"kCGSSessionOnConsoleKey"`.
        let on_console = cf_dict_bool(&dict, "kCGSSessionOnConsoleKey");
        evaluate_session_state(true, locked, on_console)
    })
}

/// Convenience projection returning `Some(reason)` only when WindowServer confirms lock/resign.
pub fn current_session_interrupt() -> Option<SystemInterruptReason> {
    match current_session_state() {
        SessionProbeState::Interrupted(reason) => Some(reason),
        SessionProbeState::ActiveUnlocked | SessionProbeState::Unavailable => None,
    }
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

/// Dedicated OS watchdog thread that polls WindowServer lock/console state completely
/// independently of the AppKit main runloop (since `NSDistributedNotificationCenter` delivers via
/// the main runloop and can stall whenever the GUI main thread is blocked).
struct SessionWatchdog {
    stop: Arc<(Mutex<bool>, Condvar)>,
    handle: Option<JoinHandle<()>>,
}

impl SessionWatchdog {
    fn start<P>(interval: Duration, latch: InterruptLatch, mut probe: P) -> Self
    where
        P: FnMut() -> Option<SystemInterruptReason> + Send + 'static,
    {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let stop_bg = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let (lock, cvar) = &*stop_bg;
            let mut guard = match lock.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            while !*guard {
                if latch.is_interrupted() {
                    return;
                }
                drop(guard);
                if let Some(reason) = probe() {
                    let _ = latch.trigger(reason);
                    return;
                }
                guard = match lock.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                if *guard {
                    break;
                }
                let (next_guard, _) = cvar
                    .wait_timeout(guard, interval)
                    .unwrap_or_else(|p| p.into_inner());
                guard = next_guard;
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for SessionWatchdog {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.stop;
        if let Ok(mut guard) = lock.lock() {
            *guard = true;
            cvar.notify_all();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Active macOS monitor watching screen lock, user switch, display sleep, and system sleep.
///
/// Combines three redundant layers:
/// 1. An independent OS watchdog thread polling `CGSessionCopyCurrentDictionary()` every 100ms
///    (works even when the AppKit main thread is 100% blocked).
/// 2. Background serial `NSOperationQueue` observers for instant push on workspace sleep/lock.
/// 3. Optional main-queue GUI wake observers (strictly on `mainQueue()` so `Rc` stays main-only).
pub struct SystemInterruptMonitor {
    latch: InterruptLatch,
    _bg_queue: Retained<NSOperationQueue>,
    _resume_queue: Retained<NSOperationQueue>,
    _watchdog: SessionWatchdog,
    distributed_tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    workspace_tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl SystemInterruptMonitor {
    /// Arms monitoring BEFORE recording startup so lock/sleep events during initialization are
    /// captured immediately.
    pub fn start_gui<F>(on_main_interrupt: F) -> Self
    where
        F: Fn(SystemInterruptReason) + 'static,
    {
        Self::start_inner(Some(Arc::new(on_main_interrupt)))
    }

    /// Arms headless monitoring for CLI sessions (watchdog + notification centers).
    pub fn start_headless() -> Self {
        Self::start_inner(None)
    }

    fn start_inner(
        on_main_interrupt: Option<Arc<dyn Fn(SystemInterruptReason) + 'static>>,
    ) -> Self {
        let latch = InterruptLatch::new();
        let bg_queue = NSOperationQueue::new();
        bg_queue.setMaxConcurrentOperationCount(1);
        let resume_queue = NSOperationQueue::new();
        resume_queue.setMaxConcurrentOperationCount(1);

        let mut distributed_tokens = Vec::with_capacity(3);
        let mut workspace_tokens = Vec::with_capacity(9);

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

        // Verify untrusted distributed `com.apple.screenIsLocked` against live `CGSession` state
        // so another local process cannot spoof a fake screen-lock stop.
        let bg_lock_block = resume::make_verified_bg_lock_block(latch.clone());
        distributed_tokens.push(unsafe {
            dist_center.addObserverForName_object_queue_usingBlock(
                Some(&lock_name),
                None,
                Some(&bg_queue),
                &bg_lock_block,
            )
        });
        for (name, reason) in workspace_events {
            let block = resume::make_bg_observer_block(latch.clone(), reason);
            workspace_tokens.push(unsafe {
                ws_center.addObserverForName_object_queue_usingBlock(
                    Some(name),
                    None,
                    Some(&bg_queue),
                    &block,
                )
            });
        }

        if let Some(cb) = on_main_interrupt {
            let main_queue = NSOperationQueue::mainQueue();
            let ui_fired = Arc::new(AtomicBool::new(false));
            let ui_lock_block = resume::make_verified_ui_lock_block(
                latch.clone(),
                Arc::clone(&ui_fired),
                Arc::clone(&cb),
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
                let block = resume::make_ui_observer_block(
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

        // Observe wake/unlock on a dedicated background queue from recording start so a fast
        // sleep->wake while the AppKit main thread is delayed can never be missed.
        let (res_dist, res_ws) = resume::register_bg_resume_observers(&resume_queue, latch.clone());
        distributed_tokens.extend(res_dist);
        workspace_tokens.extend(res_ws);

        let watchdog = SessionWatchdog::start(
            WATCHDOG_POLL_INTERVAL,
            latch.clone(),
            current_session_interrupt,
        );

        Self {
            latch,
            _bg_queue: bg_queue,
            _resume_queue: resume_queue,
            _watchdog: watchdog,
            distributed_tokens,
            workspace_tokens,
        }
    }

    pub fn latch(&self) -> InterruptLatch {
        self.latch.clone()
    }

    /// Attaches the session's immediate stop hook (invoked immediately if already interrupted).
    pub fn attach_stop_hook(&self, hook: StopHook) {
        self.latch.attach_stop_hook(hook);
    }

    /// Returns the recorded interrupt reason, falling back to a synchronous `CGSession` check.
    pub fn poll_interrupt(&self) -> Option<SystemInterruptReason> {
        if let Some(reason) = self.latch.reason() {
            return Some(reason);
        }
        if let Some(reason) = current_session_interrupt() {
            let _ = self.latch.trigger(reason);
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

#[cfg(test)]
#[path = "system_events_tests.rs"]
mod tests;
