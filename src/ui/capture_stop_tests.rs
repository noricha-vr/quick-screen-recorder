use super::*;

#[test]
fn unrelated_or_stale_editor_save_never_completes_tutorial() {
    assert!(!should_complete_tutorial_on_save(Some(1), None, false));
    assert!(!should_complete_tutorial_on_save(Some(1), None, true));
    assert!(!should_complete_tutorial_on_save(Some(2), Some(1), false));
    assert!(!should_complete_tutorial_on_save(Some(2), Some(2), true));
    assert!(should_complete_tutorial_on_save(Some(2), Some(2), false));
    assert!(!should_complete_tutorial_on_save(None, Some(2), false));
}

#[test]
fn modal_alerts_defer_when_locked_or_asleep_and_show_once_resumed() {
    assert!(!should_defer_error_alert(None, false, false));
    assert!(should_defer_error_alert(None, true, false));
    assert!(!should_defer_error_alert(
        Some(SystemInterruptReason::ScreenLocked),
        false,
        false
    ));
    assert!(should_defer_error_alert(
        Some(SystemInterruptReason::ScreenLocked),
        true,
        false
    ));
    assert!(should_defer_error_alert(
        Some(SystemInterruptReason::DisplaySlept),
        false,
        false
    ));
    assert!(!should_defer_error_alert(
        Some(SystemInterruptReason::DisplaySlept),
        false,
        true
    ));
    assert!(should_defer_error_alert(
        Some(SystemInterruptReason::SystemWillSleep),
        false,
        false
    ));
    assert!(!should_defer_error_alert(
        Some(SystemInterruptReason::SystemWillSleep),
        false,
        true
    ));
}

#[test]
fn auto_stop_notice_includes_reason_and_saved_location() {
    let path = Path::new("/tmp/AutoSaved-Recording-recovered-abc123.mp4");
    let clean = build_auto_stop_notice(SystemInterruptReason::ScreenLocked, path, None, None);
    assert!(clean.contains("screen locked"));
    assert!(clean.contains("AutoSaved-Recording"));

    let with_warn = build_auto_stop_notice(
        SystemInterruptReason::DisplaySlept,
        path,
        Some("window closed"),
        None,
    );
    assert!(with_warn.contains("display slept"));
    assert!(with_warn.contains("window closed"));
}
