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
    let monitor = SystemInterruptMonitor::start_headless();
    pump_current_runloop(Duration::from_millis(5));
    drop(monitor);
}
