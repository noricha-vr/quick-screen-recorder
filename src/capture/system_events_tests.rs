use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;

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
fn ui_trigger_before_bg_still_invokes_stop_hook_exactly_once() {
    let latch = InterruptLatch::new();
    let invocations = Arc::new(AtomicUsize::new(0));
    let inv_clone = Arc::clone(&invocations);
    latch.attach_stop_hook(Arc::new(move || {
        inv_clone.fetch_add(1, Ordering::SeqCst);
    }));

    // Simulate UI observer triggering first, followed by BG observer triggering second.
    assert!(latch.trigger(SystemInterruptReason::ScreenLocked));
    assert!(!latch.trigger(SystemInterruptReason::ScreenLocked));
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "stop hook must execute exactly once even when UI observer wins the first trigger"
    );
}

#[test]
fn attaching_stop_hook_after_early_interrupt_invokes_immediately() {
    let monitor = SystemInterruptMonitor::start_headless();
    assert!(
        monitor
            .latch()
            .trigger(SystemInterruptReason::SystemWillSleep)
    );
    let called = Arc::new(AtomicBool::new(false));
    let called_clone = Arc::clone(&called);
    monitor.attach_stop_hook(Arc::new(move || {
        called_clone.store(true, Ordering::Release);
    }));
    assert!(called.load(Ordering::Acquire));
}

#[test]
fn watchdog_triggers_stop_hook_while_main_thread_is_blocked() {
    let latch = InterruptLatch::new();
    let (hook_tx, hook_rx) = mpsc::sync_channel::<()>(1);
    latch.attach_stop_hook(Arc::new(move || {
        let _ = hook_tx.send(());
    }));

    let locked = Arc::new(AtomicBool::new(false));
    let locked_probe = Arc::clone(&locked);
    let watchdog = SessionWatchdog::start(Duration::from_millis(10), latch.clone(), move || {
        locked_probe
            .load(Ordering::Acquire)
            .then_some(SystemInterruptReason::ScreenLocked)
    });

    // Flip lock from another thread while THIS (main) thread blocks synchronously on hook_rx
    // without pumping any runloop or timer.
    let flipper = thread::spawn(move || {
        thread::sleep(Duration::from_millis(25));
        locked.store(true, Ordering::Release);
    });

    assert_eq!(
        hook_rx.recv_timeout(Duration::from_secs(2)),
        Ok(()),
        "independent watchdog must invoke stop hook even while main thread is completely blocked"
    );
    assert_eq!(latch.reason(), Some(SystemInterruptReason::ScreenLocked));
    flipper.join().expect("join flipper");
    drop(watchdog);
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
fn interrupt_reason_formats_messages() {
    for reason in [
        SystemInterruptReason::ScreenLocked,
        SystemInterruptReason::SessionResigned,
        SystemInterruptReason::DisplaySlept,
        SystemInterruptReason::SystemWillSleep,
    ] {
        assert!(!reason.clause().is_empty());
        assert!(!reason.short_label().is_empty());
    }
}

#[test]
fn headless_monitor_registers_and_drops_cleanly() {
    let monitor = SystemInterruptMonitor::start_headless();
    pump_current_runloop(Duration::from_millis(5));
    drop(monitor);
}
