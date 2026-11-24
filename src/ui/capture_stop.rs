use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes, NSTimer};

use crate::capture::recorder::StopPollState;
use crate::capture::{SystemInterruptReason, UnlockMonitor, current_session_interrupt};
use crate::fs_safety::{format_tilde_path, sanitize_terminal_text};
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

/// Defer modal `NSAlert::runModal()` calls while the screen is actively locked or still asleep
/// (i.e. wake/unlock has not yet been observed since the auto-stop started). If macOS already
/// resumed before finalization completed, display the alert immediately instead of waiting for
/// another sleep/wake cycle.
pub(crate) fn should_defer_error_alert(
    auto_stop_reason: Option<SystemInterruptReason>,
    session_currently_interrupted: bool,
    already_resumed: bool,
) -> bool {
    if session_currently_interrupted {
        return true;
    }
    if already_resumed {
        return false;
    }
    matches!(
        auto_stop_reason,
        Some(SystemInterruptReason::DisplaySlept | SystemInterruptReason::SystemWillSleep)
    )
}

fn ensure_unlock_monitor(runtime: &Rc<RefCell<AppRuntime>>) {
    if runtime.borrow().unlock_monitor.is_some() {
        return;
    }
    let rt_unlock = Rc::clone(runtime);
    let mon = UnlockMonitor::start_gui(move || {
        if let Some(m) = MainThreadMarker::new() {
            if current_session_interrupt().is_none() {
                flush_deferred_alerts(m, &rt_unlock);
            }
        }
    });
    runtime.borrow_mut().unlock_monitor = Some(mon);
}

pub(crate) fn queue_or_show_error_alert(
    mtm: MainThreadMarker,
    runtime: &Rc<RefCell<AppRuntime>>,
    title: &str,
    message: &str,
    defer: bool,
) {
    if !defer {
        show_error_alert(mtm, title, message);
        return;
    }
    ensure_unlock_monitor(runtime);
    let already_unlocked = {
        let mut rt = runtime.borrow_mut();
        rt.deferred_alerts
            .push((title.to_string(), message.to_string()));
        current_session_interrupt().is_none()
            && rt
                .unlock_monitor
                .as_ref()
                .is_some_and(UnlockMonitor::has_resumed)
    };
    if already_unlocked {
        flush_deferred_alerts(mtm, runtime);
    }
}

pub(crate) fn flush_deferred_alerts(mtm: MainThreadMarker, runtime: &Rc<RefCell<AppRuntime>>) {
    let alerts: Vec<(String, String)> = {
        let mut rt = runtime.borrow_mut();
        if rt.deferred_alerts.is_empty() {
            return;
        }
        let _ = rt.unlock_monitor.take();
        std::mem::take(&mut rt.deferred_alerts)
    };
    for (title, msg) in alerts {
        show_error_alert(mtm, &title, &msg);
    }
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

    // Arm unlock/wake monitoring before finalization starts when auto-stopping so if macOS
    // wakes up while the MP4 is flushing, we detect `has_resumed()` and never leave alerts stuck.
    if auto_stop_reason.is_some() {
        ensure_unlock_monitor(runtime);
    }

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
        let (should_quit, already_resumed) = {
            let rt = rt_poll.borrow();
            (
                rt.quit_after_stop,
                rt.unlock_monitor
                    .as_ref()
                    .is_some_and(UnlockMonitor::has_resumed),
            )
        };
        let defer_modal = should_defer_error_alert(
            auto_stop_reason,
            current_session_interrupt().is_some(),
            already_resumed,
        );
        if !defer_modal {
            let mut rt = rt_poll.borrow_mut();
            if rt.deferred_alerts.is_empty() {
                let _ = rt.unlock_monitor.take();
            }
        }

        match pending.finish() {
            Ok(outcome) => {
                if should_quit {
                    let rescued = preserve_capture_to_recovery(
                        &outcome.output_path,
                        &mut rt_poll.borrow_mut().temp_files,
                        private_recovery_dir(),
                        "Quit-Saved-Recording",
                    );
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
                    if defer_modal {
                        eprintln!(
                            "QuickScreenRecorder: {}",
                            sanitize_terminal_text(&notice, false)
                        );
                    } else {
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
                    if let Err(err) = open_editor_for_file(
                        m,
                        &rt_poll,
                        &editor_path,
                        app_name,
                        tutorial_gen,
                        Some(notice),
                    ) {
                        let fallback_msg = format!(
                            "Recording auto-stopped ({}) and was saved at {}, but failed opening the editor: {err}",
                            reason.short_label(),
                            format_tilde_path(&editor_path)
                        );
                        queue_or_show_error_alert(
                            m,
                            &rt_poll,
                            "Recording Saved — Failed Opening Editor",
                            &fallback_msg,
                            defer_modal,
                        );
                    }
                    return;
                }

                let inline_notice = if defer_modal {
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
                    queue_or_show_error_alert(
                        m,
                        &rt_poll,
                        "Failed Opening Editor",
                        &err,
                        defer_modal,
                    );
                }
                if let Some(partial_err) = outcome.partial_error {
                    queue_or_show_error_alert(
                        m,
                        &rt_poll,
                        "Recording Saved With Warning",
                        &format!(
                            "The recording was saved and opened in the editor, with a warning: {partial_err}"
                        ),
                        defer_modal,
                    );
                }
            }
            Err(err) => {
                if should_quit {
                    if defer_modal {
                        eprintln!(
                            "QuickScreenRecorder recording error on quit: {}",
                            sanitize_terminal_text(&err, false)
                        );
                    } else {
                        show_error_alert(m, "Recording Error", &err);
                    }
                    crate::ui::finish_app_termination(m, &rt_poll);
                } else {
                    let msg = match auto_stop_reason {
                        Some(reason) => format!(
                            "Recording auto-stopped because {}, but saving encountered an error: {err}",
                            reason.clause()
                        ),
                        None => err,
                    };
                    queue_or_show_error_alert(m, &rt_poll, "Recording Error", &msg, defer_modal);
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
#[path = "capture_stop_tests.rs"]
mod tests;
