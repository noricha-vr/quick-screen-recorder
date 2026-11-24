use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes, NSTimer};

use crate::capture::recorder::StopPollState;
use crate::capture::{SystemInterruptReason, current_session_interrupt};
use crate::fs_safety::format_tilde_path;
use crate::staging::{preserve_capture_to_recovery, private_recovery_dir};
use crate::ui::editor::{EditorOpenOptions, EditorWindow};
use crate::ui::folder_picker::show_error_alert;
use crate::ui::onboarding_visual::OnboardingStep;
use crate::ui::{AppRuntime, apply_export_dir_change, open_onboarding};

pub(crate) fn should_complete_tutorial_on_save(
    active_tutorial_gen: Option<u64>,
    editor_tutorial_gen: Option<u64>,
    recording_active: bool,
) -> bool {
    !recording_active && active_tutorial_gen.is_some() && active_tutorial_gen == editor_tutorial_gen
}

/// Modal `NSAlert::runModal()` calls must be skipped while the screen is locked or sleeping so
/// logout, sleep, and app termination never hang behind an invisible modal dialog.
pub(crate) fn should_suppress_modal_alert(
    auto_stop_reason: Option<SystemInterruptReason>,
    session_currently_interrupted: bool,
) -> bool {
    auto_stop_reason.is_some() || session_currently_interrupted
}

pub(crate) fn build_auto_stop_notice(
    reason: SystemInterruptReason,
    saved_path: &Path,
    partial_error: Option<&str>,
    recovery_fallback_err: Option<&str>,
) -> String {
    let saved_display = format_tilde_path(saved_path);
    match (partial_error, recovery_fallback_err) {
        (Some(warn), _) => format!(
            "Auto-stopped ({}) · Saved at {saved_display} ({warn})",
            reason.short_label()
        ),
        (None, Some(rec_err)) => format!(
            "Auto-stopped ({}) · Preserved at {saved_display} (recovery fallback: {rec_err})",
            reason.short_label()
        ),
        (None, None) => format!(
            "Auto-stopped ({}) · Saved at {saved_display}",
            reason.short_label()
        ),
    }
}

pub(crate) fn stop_active_recording(mtm: MainThreadMarker, runtime: &Rc<RefCell<AppRuntime>>) {
    stop_active_recording_with_reason(mtm, runtime, None);
}

pub(crate) fn stop_active_recording_with_reason(
    mtm: MainThreadMarker,
    runtime: &Rc<RefCell<AppRuntime>>,
    auto_stop_reason: Option<SystemInterruptReason>,
) {
    let (session, captured_app, tutorial_gen) = {
        let mut rt = runtime.borrow_mut();
        if rt.pending_stop.is_some() {
            return;
        }
        let Some(sess) = rt.session.take() else {
            return;
        };
        let _ = rt.interrupt_monitor.take();
        if let Some(timer) = rt.rec_timer.take() {
            timer.invalidate();
        }
        if let Some(sb) = &rt.status_bar {
            sb.set_stopping(mtm);
        }
        if let Some(ob) = &rt.onboarding {
            ob.hide_temporarily();
        }
        (sess, rt.active_capture_app.take(), rt.tutorial_generation)
    };

    // Initiate teardown without blocking the AppKit main runloop; CoreAudio / ScreenCaptureKit
    // can deliver final audio + video buffers cleanly while NSTimer polls completion.
    let pending = session.begin_stop();
    runtime.borrow_mut().pending_stop = Some(pending);

    let captured_cell = RefCell::new(captured_app);
    let rt_poll = Rc::clone(runtime);
    let poll_block = RcBlock::new(move |timer: NonNull<NSTimer>| {
        let poll_state = {
            let mut rt = rt_poll.borrow_mut();
            match rt.pending_stop.as_mut() {
                Some(p) => p.poll(),
                None => {
                    unsafe { timer.as_ref().invalidate() };
                    rt.stop_poll_timer = None;
                    return;
                }
            }
        };
        if poll_state == StopPollState::Pending {
            return;
        }

        unsafe { timer.as_ref().invalidate() };
        let Some(m) = MainThreadMarker::new() else {
            return;
        };
        let finished_pending = {
            let mut rt = rt_poll.borrow_mut();
            rt.stop_poll_timer = None;
            if let Some(sb) = &rt.status_bar {
                sb.set_idle(m);
            }
            rt.pending_stop.take()
        };
        let Some(pending) = finished_pending else {
            return;
        };
        let app_name = captured_cell.borrow_mut().take();
        let should_quit = rt_poll.borrow().quit_after_stop;
        let suppress_modal =
            should_suppress_modal_alert(auto_stop_reason, current_session_interrupt().is_some());

        match pending.finish() {
            Ok(outcome) => {
                if should_quit {
                    let rescued = preserve_capture_to_recovery(
                        &outcome.output_path,
                        &mut rt_poll.borrow_mut().temp_files,
                        private_recovery_dir(),
                        "Quit-Saved-Recording",
                    );
                    if !suppress_modal {
                        let notice = match (rescued, outcome.partial_error) {
                            (Ok(saved), Some(warn)) => format!(
                                "Your recording was preserved at {} before quitting (warning: {warn}).",
                                format_tilde_path(&saved)
                            ),
                            (Ok(saved), None) => format!(
                                "Your recording was preserved at {} before quitting.",
                                format_tilde_path(&saved)
                            ),
                            (Err((rec_err, kept)), _) => format!(
                                "Could not move recording to recovery ({rec_err}); file was preserved at {}.",
                                format_tilde_path(&kept)
                            ),
                        };
                        show_error_alert(m, "Recording Saved Before Quit", &notice);
                    }
                    crate::ui::finish_app_termination(m, &rt_poll);
                    return;
                }

                if let Some(reason) = auto_stop_reason {
                    // Persist auto-stopped clips into private Recovery so they survive overnight
                    // OS reboots, quit without export, and 24-hour cache pruning.
                    let (editor_path, rec_err_opt) = match preserve_capture_to_recovery(
                        &outcome.output_path,
                        &mut rt_poll.borrow_mut().temp_files,
                        private_recovery_dir(),
                        "AutoSaved-Recording",
                    ) {
                        Ok(saved) => (saved, None),
                        Err((err, kept)) => (kept, Some(err)),
                    };
                    let notice = build_auto_stop_notice(
                        reason,
                        &editor_path,
                        outcome.partial_error.as_deref(),
                        rec_err_opt.as_deref(),
                    );
                    let _ = open_editor_for_file(
                        m,
                        &rt_poll,
                        &editor_path,
                        app_name,
                        tutorial_gen,
                        Some(notice),
                    );
                    return;
                }

                let inline_notice = if suppress_modal {
                    outcome
                        .partial_error
                        .as_ref()
                        .map(|w| format!("Recording saved with warning: {w}"))
                } else {
                    None
                };
                if let Err(err) = open_editor_for_file(
                    m,
                    &rt_poll,
                    &outcome.output_path,
                    app_name,
                    tutorial_gen,
                    inline_notice,
                ) {
                    if !suppress_modal {
                        show_error_alert(m, "Failed Opening Editor", &err);
                    }
                }
                if !suppress_modal {
                    if let Some(partial_err) = outcome.partial_error {
                        show_error_alert(
                            m,
                            "Recording Saved With Warning",
                            &format!(
                                "The recording was saved and opened in the editor, with a warning: {partial_err}"
                            ),
                        );
                    }
                }
            }
            Err(err) => {
                if !suppress_modal {
                    show_error_alert(m, "Recording Error", &err);
                }
                if should_quit {
                    crate::ui::finish_app_termination(m, &rt_poll);
                }
            }
        }
    });

    let timer = unsafe {
        let t = NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.035, true, &poll_block);
        // Register in common modes so menu tracking / modal sheets never pause stop finalization.
        NSRunLoop::mainRunLoop().addTimer_forMode(&t, NSRunLoopCommonModes);
        t
    };
    runtime.borrow_mut().stop_poll_timer = Some(timer);
}

pub(crate) fn open_editor_for_file(
    mtm: MainThreadMarker,
    runtime: &Rc<RefCell<AppRuntime>>,
    video_path: &Path,
    captured_app_name: Option<String>,
    tutorial_gen: Option<u64>,
    initial_notice: Option<String>,
) -> Result<(), String> {
    let prefs = runtime.borrow().prefs.clone();
    let (rt_dir, rt_saved) = (Rc::clone(runtime), Rc::clone(runtime));
    let on_export_saved: Rc<dyn Fn(PathBuf)> = if let Some(editor_gen) = tutorial_gen {
        Rc::new(move |saved_path| {
            let celebrate = {
                let mut rt = rt_saved.borrow_mut();
                if should_complete_tutorial_on_save(
                    rt.tutorial_generation,
                    Some(editor_gen),
                    rt.session.is_some() || rt.pending_stop.is_some(),
                ) {
                    rt.prefs.onboarding_completed = true;
                    let _ = rt.prefs.save();
                    rt.tutorial_generation = None;
                    true
                } else {
                    false
                }
            };
            if celebrate {
                if let Some(m) = MainThreadMarker::new() {
                    open_onboarding(
                        m,
                        &rt_saved,
                        Some(OnboardingStep::Completed),
                        Some(saved_path),
                        false,
                    );
                }
            }
        })
    } else {
        Rc::new(|_| {})
    };
    let editor = EditorWindow::open(
        mtm,
        video_path,
        &prefs,
        EditorOpenOptions {
            captured_app_name,
            demo_showcase: false,
            tutorial_hint: tutorial_gen.is_some(),
            initial_notice,
            on_export_dir_change: Rc::new(move |new_dir| {
                if let Some(m) = MainThreadMarker::new() {
                    apply_export_dir_change(m, &rt_dir, new_dir);
                }
            }),
            on_export_saved,
        },
    )?;
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    runtime.borrow_mut().editors.push(editor);
    Ok(())
}

#[cfg(test)]
mod tests {
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
    fn modal_alerts_are_suppressed_on_screen_lock_and_sleep() {
        assert!(!should_suppress_modal_alert(None, false));
        assert!(should_suppress_modal_alert(None, true));
        assert!(should_suppress_modal_alert(
            Some(SystemInterruptReason::ScreenLocked),
            false
        ));
        assert!(should_suppress_modal_alert(
            Some(SystemInterruptReason::SystemWillSleep),
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
}
