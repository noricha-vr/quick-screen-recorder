pub mod capture_flow;
pub mod clipboard;
pub mod demo;
pub mod editor;
pub mod editor_controls;
pub mod editor_worker;
pub mod folder_picker;
pub mod hotkey;
pub mod onboarding;
pub mod onboarding_layout;
pub mod onboarding_scenes;
pub mod onboarding_visual;
pub mod overlay;
pub mod overlay_actions;
pub mod overlay_canvas;
pub mod overlay_toolbar;
pub mod overlay_window;
pub mod status_bar;
pub mod trim_math;
pub mod trim_view;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::NSTimer;

use crate::capture::{ActiveRecording, PendingStop, RecordingCodec};
use crate::cli::DemoMode;
use crate::config::Preferences;
use crate::permissions::has_screen_capture_permission;
use crate::ui::capture_flow::{
    cancel_overlay, open_editor_for_file, show_selector, stop_active_recording,
    toggle_select_or_stop,
};
use crate::ui::demo::run_demo_flow;
use crate::ui::editor::EditorWindow;
use crate::ui::folder_picker::{choose_export_directory, choose_video_file, show_error_alert};
use crate::ui::hotkey::{
    install_standard_edit_menu, register_global_cmd_shift_6, register_local_escape_monitor,
};
use crate::ui::onboarding::{OnboardingCallbacks, OnboardingWindow};
use crate::ui::onboarding_visual::OnboardingStep;
use crate::ui::overlay::SelectorOverlay;
use crate::ui::status_bar::{StatusBarCallbacks, StatusBarController};

pub(crate) struct AppRuntime {
    pub(crate) prefs: Preferences,
    pub(crate) status_bar: Option<StatusBarController>,
    pub(crate) onboarding: Option<OnboardingWindow>,
    pub(crate) tutorial_generation: Option<u64>,
    pub(crate) next_tutorial_gen: u64,
    pub(crate) overlay: Option<SelectorOverlay>,
    pub(crate) session: Option<ActiveRecording>,
    pub(crate) pending_stop: Option<PendingStop>,
    pub(crate) active_capture_app: Option<String>,
    pub(crate) rec_timer: Option<Retained<NSTimer>>,
    pub(crate) stop_poll_timer: Option<Retained<NSTimer>>,
    pub(crate) mic_perm_timer: Option<Retained<NSTimer>>,
    pub(crate) editors: Vec<EditorWindow>,
    pub(crate) temp_files: Vec<PathBuf>,
    pub(crate) quit_after_stop: bool,
}

pub fn run_gui(
    open_selector: bool,
    open_editor: Option<PathBuf>,
    demo_mode: Option<DemoMode>,
) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("GUI must be started on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    let policy = if demo_mode.is_some() || open_editor.is_some() {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    };
    app.setActivationPolicy(policy);
    install_standard_edit_menu(mtm);
    crate::fs_safety::prune_stale_cache_files(std::time::Duration::from_secs(24 * 3600));

    if let Some(demo) = demo_mode {
        return run_demo_flow(mtm, &app, demo);
    }

    let prefs = Preferences::load();
    let initial_codec = prefs.recording_codec;
    let initial_export_dir = prefs.resolved_export_dir();
    let show_first_run = !prefs.onboarding_completed;
    let runtime = Rc::new(RefCell::new(AppRuntime {
        prefs,
        status_bar: None,
        onboarding: None,
        tutorial_generation: None,
        next_tutorial_gen: 1,
        overlay: None,
        session: None,
        pending_stop: None,
        active_capture_app: None,
        rec_timer: None,
        stop_poll_timer: None,
        mic_perm_timer: None,
        editors: Vec::new(),
        temp_files: Vec::new(),
        quit_after_stop: false,
    }));

    let status_bar = StatusBarController::new(
        mtm,
        initial_codec,
        initial_export_dir.as_deref(),
        build_status_bar_callbacks(&runtime),
    );
    runtime.borrow_mut().status_bar = Some(status_bar);
    install_shortcut_monitors(&runtime);

    if let Some(video_path) = open_editor {
        open_editor_for_file(mtm, &runtime, &video_path, None, None)?;
    } else if open_selector {
        show_selector(mtm, &runtime);
    } else if show_first_run {
        open_onboarding(mtm, &runtime, None, None, false);
    }

    app.run();
    Ok(())
}

fn build_status_bar_callbacks(runtime: &Rc<RefCell<AppRuntime>>) -> StatusBarCallbacks {
    let (rt_p, rt_o, rt_s, rt_c, rt_k, rt_g, rt_q) = (
        Rc::clone(runtime),
        Rc::clone(runtime),
        Rc::clone(runtime),
        Rc::clone(runtime),
        Rc::clone(runtime),
        Rc::clone(runtime),
        Rc::clone(runtime),
    );
    StatusBarCallbacks {
        on_primary_click: Box::new(move |m| toggle_select_or_stop(m, &rt_p)),
        on_open_video: Box::new(move |m| {
            if let Some(path) = choose_video_file(m) {
                if let Err(err) = open_editor_for_file(m, &rt_o, &path, None, None) {
                    show_error_alert(m, "Cannot Open Video", &err);
                }
            }
        }),
        on_select_export_dir: Box::new(move |m, dir| {
            if !crate::fs_safety::is_usable_export_dir(&dir) {
                show_error_alert(
                    m,
                    "Invalid Export Folder",
                    "Selected destination is unavailable or unsafe. Please choose another folder.",
                );
                let cur = rt_s.borrow().prefs.resolved_export_dir();
                if let Some(sb) = &rt_s.borrow().status_bar {
                    sb.set_export_dir(m, cur.as_deref());
                }
                return;
            }
            apply_export_dir_change(m, &rt_s, dir);
        }),
        on_choose_export_folder: Box::new(move |m| {
            let cur = rt_c.borrow().prefs.resolved_export_dir();
            match choose_export_directory(m, cur.as_deref()) {
                None => {}
                Some(Err(msg)) => show_error_alert(m, "Invalid Export Folder", msg),
                Some(Ok(picked)) => apply_export_dir_change(m, &rt_c, picked),
            }
        }),
        on_toggle_codec: Box::new(move |_m| {
            let mut rt = rt_k.borrow_mut();
            rt.prefs.recording_codec = match rt.prefs.recording_codec {
                RecordingCodec::H264 => RecordingCodec::Hevc,
                RecordingCodec::Hevc => RecordingCodec::H264,
            };
            let next = rt.prefs.recording_codec;
            let _ = rt.prefs.save();
            if let Some(sb) = &rt.status_bar {
                sb.set_codec_label(next);
            }
        }),
        on_open_guide: Box::new(move |m| {
            let is_rec = {
                let mut rt = rt_g.borrow_mut();
                let rec = rt.session.is_some() || rt.pending_stop.is_some();
                if !rec {
                    // Start a fresh tutorial generation so any previously open editor cannot complete it.
                    rt.tutorial_generation = None;
                }
                rec
            };
            let step = is_rec.then_some(OnboardingStep::RecordingInProgress);
            open_onboarding(m, &rt_g, step, None, is_rec);
        }),
        on_quit: Box::new(move |m| cleanup_and_quit(m, &rt_q)),
    }
}

pub(crate) fn open_onboarding(
    mtm: MainThreadMarker,
    runtime: &Rc<RefCell<AppRuntime>>,
    forced_step: Option<OnboardingStep>,
    saved_path: Option<PathBuf>,
    dock_top_right: bool,
) {
    let granted = has_screen_capture_permission();
    let step = forced_step.unwrap_or_else(|| OnboardingStep::initial(granted));
    let perm_display = granted || step == OnboardingStep::Completed;
    {
        let mut rt = runtime.borrow_mut();
        if matches!(step, OnboardingStep::Completed) {
            rt.tutorial_generation = None;
        } else if rt.tutorial_generation.is_none() {
            let gen_id = rt.next_tutorial_gen;
            rt.next_tutorial_gen = rt.next_tutorial_gen.wrapping_add(1).max(1);
            rt.tutorial_generation = Some(gen_id);
        }
        if let Some(ob) = &rt.onboarding {
            ob.show_step(mtm, step, perm_display, saved_path, dock_top_right);
            return;
        }
    }
    let (rt_st, rt_sp, rt_sk, rt_dn) = (
        Rc::clone(runtime),
        Rc::clone(runtime),
        Rc::clone(runtime),
        Rc::clone(runtime),
    );
    let ob = OnboardingWindow::open(
        mtm,
        step,
        perm_display,
        saved_path,
        None,
        OnboardingCallbacks {
            on_start_recording: Box::new(move |m| show_selector(m, &rt_st)),
            on_stop_recording: Box::new(move |m| stop_active_recording(m, &rt_sp)),
            on_skip: Box::new(move |_m| complete_and_close_onboarding(&rt_sk)),
            on_done: Box::new(move |_m| complete_and_close_onboarding(&rt_dn)),
        },
    );
    if dock_top_right {
        ob.show_step(mtm, step, perm_display, None, true);
    }
    runtime.borrow_mut().onboarding = Some(ob);
}

fn complete_and_close_onboarding(runtime: &Rc<RefCell<AppRuntime>>) {
    let mut rt = runtime.borrow_mut();
    rt.prefs.onboarding_completed = true;
    let _ = rt.prefs.save();
    rt.tutorial_generation = None;
    if let Some(ob) = &rt.onboarding {
        ob.close();
    }
}

pub(crate) fn apply_export_dir_change(
    mtm: MainThreadMarker,
    runtime: &Rc<RefCell<AppRuntime>>,
    dir: PathBuf,
) {
    {
        let mut rt = runtime.borrow_mut();
        rt.prefs.export_dir = Some(dir.clone());
        let _ = rt.prefs.save();
    }
    let rt = runtime.borrow();
    if let Some(sb) = &rt.status_bar {
        sb.set_export_dir(mtm, Some(&dir));
    }
    for ed in &rt.editors {
        ed.set_export_dir(mtm, dir.clone());
    }
}

fn cleanup_and_quit(mtm: MainThreadMarker, runtime: &Rc<RefCell<AppRuntime>>) {
    let has_active_session = {
        let mut rt = runtime.borrow_mut();
        if let Some(ob) = rt.onboarding.take() {
            ob.close();
        }
        if let Some(ov) = rt.overlay.take() {
            ov.close();
        }
        if let Some(t) = rt.mic_perm_timer.take() {
            t.invalidate();
        }
        // Defer termination while an asynchronous stop/commit is in flight so cross-volume
        // saves and local recovery always run to completion before the process exits.
        if rt.pending_stop.is_some() {
            rt.quit_after_stop = true;
            return;
        }
        if rt.session.is_some() {
            rt.quit_after_stop = true;
            true
        } else {
            false
        }
    };
    if has_active_session {
        stop_active_recording(mtm, runtime);
        return;
    }
    finish_app_termination(mtm, runtime);
}

pub(crate) fn finish_app_termination(mtm: MainThreadMarker, runtime: &Rc<RefCell<AppRuntime>>) {
    {
        let mut rt = runtime.borrow_mut();
        if let Some(t) = rt.rec_timer.take() {
            t.invalidate();
        }
        if let Some(t) = rt.stop_poll_timer.take() {
            t.invalidate();
        }
        for tmp in rt.temp_files.drain(..) {
            let _ = std::fs::remove_file(tmp);
        }
    }
    NSApplication::sharedApplication(mtm).terminate(None);
}

fn install_shortcut_monitors(runtime: &Rc<RefCell<AppRuntime>>) {
    let rt_global = Rc::clone(runtime);
    register_global_cmd_shift_6(move |m| toggle_select_or_stop(m, &rt_global));
    let rt_local = Rc::clone(runtime);
    register_local_escape_monitor(move || {
        if rt_local.borrow().overlay.is_some() {
            cancel_overlay(&rt_local);
            true
        } else {
            false
        }
    });
}
