use std::cell::RefCell;
use std::path::Path;
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use objc2_foundation::NSTimer;

use crate::capture::CaptureConfig;
use crate::capture::recorder::RecordingSession;
use crate::capture::shareable::query_shareable_content;
use crate::config::Preferences;
use crate::permissions::has_screen_capture_permission;
use crate::ui::editor::{EditorOpenOptions, EditorWindow};
use crate::ui::folder_picker::show_error_alert;
use crate::ui::onboarding_visual::OnboardingStep;
use crate::ui::overlay::SelectorOverlay;
use crate::ui::{AppRuntime, apply_export_dir_change, open_onboarding};

pub(crate) fn toggle_select_or_stop(mtm: MainThreadMarker, runtime: &Rc<RefCell<AppRuntime>>) {
    if runtime.borrow().session.is_some() {
        stop_active_recording(mtm, runtime);
    } else {
        show_selector(mtm, runtime);
    }
}

pub(crate) fn show_selector(mtm: MainThreadMarker, runtime: &Rc<RefCell<AppRuntime>>) {
    if !has_screen_capture_permission() {
        open_onboarding(mtm, runtime, Some(OnboardingStep::Permission), None, false);
        return;
    }
    {
        let mut rt = runtime.borrow_mut();
        if let Some(ob) = &rt.onboarding {
            ob.hide_temporarily();
        }
        if let Some(existing) = rt.overlay.take() {
            existing.close();
        }
    }
    let (displays, windows) = match query_shareable_content() {
        Ok(snap) => (snap.displays, snap.windows),
        Err(err) => {
            show_error_alert(mtm, "Cannot Access Screen Content", &err);
            return;
        }
    };
    let prefs_snapshot = runtime.borrow().prefs.clone();
    let (rt_start, rt_cancel) = (Rc::clone(runtime), Rc::clone(runtime));
    let overlay_res = SelectorOverlay::show(
        mtm,
        &prefs_snapshot,
        displays,
        windows,
        false,
        Rc::new(move |res| {
            if let Some(m) = MainThreadMarker::new() {
                match res {
                    Ok((cfg, updated)) => start_recording_from_overlay(m, &rt_start, cfg, updated),
                    Err(err) => show_error_alert(m, "Cannot Start Recording", &err),
                }
            }
        }),
        Rc::new(move || cancel_overlay(&rt_cancel)),
    );
    match overlay_res {
        Ok(ov) => {
            let app = NSApplication::sharedApplication(mtm);
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
            ov.canvas_window.makeKeyAndOrderFront(None);
            ov.toolbar_window.orderFrontRegardless();
            runtime.borrow_mut().overlay = Some(ov);
        }
        Err(err) => show_error_alert(mtm, "Overlay Error", &err),
    }
}

pub(crate) fn cancel_overlay(runtime: &Rc<RefCell<AppRuntime>>) {
    let resume_tutorial = {
        let mut rt = runtime.borrow_mut();
        if let Some(ov) = rt.overlay.take() {
            ov.close();
        }
        rt.tutorial_in_progress && has_screen_capture_permission()
    };
    if resume_tutorial {
        if let Some(m) = MainThreadMarker::new() {
            open_onboarding(
                m,
                runtime,
                Some(OnboardingStep::StartRecording),
                None,
                false,
            );
        }
    }
}

fn start_recording_from_overlay(
    mtm: MainThreadMarker,
    runtime: &Rc<RefCell<AppRuntime>>,
    mut cfg: CaptureConfig,
    updated_prefs: Preferences,
) {
    let tutorial_active = {
        let mut rt = runtime.borrow_mut();
        rt.prefs.merge_capture_selection(&updated_prefs);
        cfg.codec = rt.prefs.recording_codec;
        if let Some(ob) = &rt.onboarding {
            cfg.excluded_window_ids.push(ob.window_number());
        }
        let _ = rt.prefs.save();
        if let Some(ov) = rt.overlay.take() {
            ov.close();
        }
        rt.tutorial_in_progress
    };
    let raw_out = match crate::fs_safety::unique_private_file_path("capture", "mp4") {
        Ok(p) => p,
        Err(err) => {
            show_error_alert(mtm, "Cache Directory Error", &err);
            return;
        }
    };
    let source_app = cfg.source_app_name.clone();
    match RecordingSession::start(&cfg, &raw_out) {
        Ok(session) => {
            {
                let mut rt = runtime.borrow_mut();
                rt.temp_files.push(raw_out);
                rt.active_capture_app = source_app;
                if let Some(sb) = &rt.status_bar {
                    sb.set_recording_elapsed(mtm, 0);
                }
                rt.session = Some(session);
                let rt_tick = Rc::clone(runtime);
                let block = RcBlock::new(move |_t: NonNull<NSTimer>| {
                    let Some(m) = MainThreadMarker::new() else {
                        return;
                    };
                    let r = rt_tick.borrow();
                    if let (Some(sess), Some(sb)) = (&r.session, &r.status_bar) {
                        sb.set_recording_elapsed(m, sess.elapsed().as_secs());
                    }
                });
                rt.rec_timer = Some(unsafe {
                    NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.5, true, &block)
                });
            }
            if tutorial_active {
                open_onboarding(
                    mtm,
                    runtime,
                    Some(OnboardingStep::RecordingInProgress),
                    None,
                    true,
                );
            }
        }
        Err(err) => show_error_alert(mtm, "Recording Failed to Start", &err),
    }
}

pub(crate) fn stop_active_recording(mtm: MainThreadMarker, runtime: &Rc<RefCell<AppRuntime>>) {
    let (session, captured_app, tutorial_hint) = {
        let mut rt = runtime.borrow_mut();
        if let Some(timer) = rt.rec_timer.take() {
            timer.invalidate();
        }
        if let Some(sb) = &rt.status_bar {
            sb.set_idle(mtm);
        }
        if let Some(ob) = &rt.onboarding {
            ob.hide_temporarily();
        }
        (
            rt.session.take(),
            rt.active_capture_app.take(),
            rt.tutorial_in_progress,
        )
    };
    let Some(sess) = session else {
        return;
    };
    match sess.stop() {
        Ok(saved_path) => {
            if let Err(err) =
                open_editor_for_file(mtm, runtime, &saved_path, captured_app, tutorial_hint)
            {
                show_error_alert(mtm, "Failed Opening Editor", &err);
            }
        }
        Err(err) => show_error_alert(mtm, "Recording Error", &err),
    }
}

pub(crate) fn open_editor_for_file(
    mtm: MainThreadMarker,
    runtime: &Rc<RefCell<AppRuntime>>,
    video_path: &Path,
    captured_app_name: Option<String>,
    tutorial_hint: bool,
) -> Result<(), String> {
    let prefs = runtime.borrow().prefs.clone();
    let (rt_dir, rt_saved) = (Rc::clone(runtime), Rc::clone(runtime));
    let editor = EditorWindow::open(
        mtm,
        video_path,
        &prefs,
        EditorOpenOptions {
            captured_app_name,
            demo_showcase: false,
            tutorial_hint,
            on_export_dir_change: Rc::new(move |new_dir| {
                if let Some(m) = MainThreadMarker::new() {
                    apply_export_dir_change(m, &rt_dir, new_dir);
                }
            }),
            on_export_saved: Rc::new(move |saved_path| {
                let celebrate = {
                    let mut rt = rt_saved.borrow_mut();
                    if rt.tutorial_in_progress {
                        rt.prefs.onboarding_completed = true;
                        let _ = rt.prefs.save();
                        rt.tutorial_in_progress = false;
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
            }),
        },
    )?;
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    runtime.borrow_mut().editors.push(editor);
    Ok(())
}
