pub mod clipboard;
pub mod editor;
pub mod editor_worker;
pub mod hotkey;
pub mod overlay;
pub mod overlay_actions;
pub mod overlay_canvas;
pub mod overlay_toolbar;
pub mod status_bar;
pub mod trim_math;
pub mod trim_view;

use std::cell::RefCell;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventMask, NSModalResponseOK,
    NSOpenPanel,
};
use objc2_foundation::{NSString, NSTimer};

use crate::capture::recorder::RecordingSession;
use crate::capture::shareable::{DisplayInfo, query_shareable_content};
use crate::capture::{CaptureConfig, RecordingCodec};
use crate::cli::DemoMode;
use crate::config::Preferences;
use crate::permissions::{has_screen_capture_permission, request_screen_capture_permission};
use crate::ui::clipboard::capture_window_view_to_png;
use crate::ui::editor::EditorWindow;
use crate::ui::overlay::SelectorOverlay;
use crate::ui::status_bar::{StatusBarCallbacks, StatusBarController};

struct AppRuntime {
    prefs: Preferences,
    status_bar: Option<StatusBarController>,
    overlay: Option<SelectorOverlay>,
    session: Option<RecordingSession>,
    rec_timer: Option<Retained<NSTimer>>,
    editors: Vec<EditorWindow>,
    temp_files: Vec<PathBuf>,
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
    crate::fs_safety::prune_stale_cache_files(std::time::Duration::from_secs(24 * 3600));

    let prefs = Preferences::load();
    let initial_codec = prefs.recording_codec;
    let runtime = Rc::new(RefCell::new(AppRuntime {
        prefs,
        status_bar: None,
        overlay: None,
        session: None,
        rec_timer: None,
        editors: Vec::new(),
        temp_files: Vec::new(),
    }));

    let rt_primary = Rc::clone(&runtime);
    let rt_open = Rc::clone(&runtime);
    let rt_codec = Rc::clone(&runtime);
    let rt_quit = Rc::clone(&runtime);

    let status_bar = StatusBarController::new(
        mtm,
        initial_codec,
        StatusBarCallbacks {
            on_primary_click: Box::new(move |m| toggle_select_or_stop(m, &rt_primary)),
            on_open_video: Box::new(move |m| prompt_open_video_in_editor(m, &rt_open)),
            on_toggle_codec: Box::new(move |_m| {
                let mut rt = rt_codec.borrow_mut();
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
            on_quit: Box::new(move |m| cleanup_and_quit(m, &rt_quit)),
        },
    );
    runtime.borrow_mut().status_bar = Some(status_bar);

    install_shortcut_monitors(&runtime);

    if let Some(demo) = demo_mode {
        run_demo_flow(mtm, &app, &runtime, demo)?;
        return Ok(());
    }

    if let Some(video_path) = open_editor {
        open_editor_for_file(mtm, &runtime, &video_path)?;
    } else if open_selector {
        show_selector(mtm, &runtime);
    }

    app.run();
    Ok(())
}

fn toggle_select_or_stop(mtm: MainThreadMarker, runtime: &Rc<RefCell<AppRuntime>>) {
    let is_recording = runtime.borrow().session.is_some();
    if is_recording {
        stop_active_recording(mtm, runtime);
    } else {
        show_selector(mtm, runtime);
    }
}

fn show_selector(mtm: MainThreadMarker, runtime: &Rc<RefCell<AppRuntime>>) {
    if !has_screen_capture_permission() && !request_screen_capture_permission() {
        show_error_alert(
            mtm,
            "Screen Recording Permission Required",
            "Enable Quick Screen Recorder in System Settings > Privacy & Security > Screen & System Audio Recording, then try again.",
        );
        return;
    }

    if let Some(existing) = runtime.borrow_mut().overlay.take() {
        existing.close();
    }

    let (displays, windows) = match query_shareable_content() {
        Ok(snap) => (snap.displays, snap.windows),
        Err(err) => {
            show_error_alert(mtm, "Cannot Access Screen Content", &err);
            return;
        }
    };

    let prefs_snapshot = runtime.borrow().prefs.clone();
    let rt_start = Rc::clone(runtime);
    let rt_cancel = Rc::clone(runtime);

    let overlay_res = SelectorOverlay::show(
        mtm,
        &prefs_snapshot,
        displays,
        windows,
        false,
        Rc::new(move |start_res| {
            if let Some(m) = MainThreadMarker::new() {
                match start_res {
                    Ok((cfg, updated_prefs)) => {
                        start_recording_from_overlay(m, &rt_start, cfg, updated_prefs);
                    }
                    Err(err) => show_error_alert(m, "Cannot Start Recording", &err),
                }
            }
        }),
        Rc::new(move || {
            if let Some(ov) = rt_cancel.borrow_mut().overlay.take() {
                ov.close();
            }
        }),
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

fn start_recording_from_overlay(
    mtm: MainThreadMarker,
    runtime: &Rc<RefCell<AppRuntime>>,
    cfg: CaptureConfig,
    updated_prefs: Preferences,
) {
    {
        let mut rt = runtime.borrow_mut();
        rt.prefs = updated_prefs;
        let _ = rt.prefs.save();
        if let Some(ov) = rt.overlay.take() {
            ov.close();
        }
    }

    let raw_out = match crate::fs_safety::unique_private_file_path("capture", "mp4") {
        Ok(p) => p,
        Err(err) => {
            show_error_alert(mtm, "Cache Directory Error", &err);
            return;
        }
    };

    match RecordingSession::start(&cfg, &raw_out) {
        Ok(session) => {
            let mut rt = runtime.borrow_mut();
            rt.temp_files.push(raw_out);
            if let Some(sb) = &rt.status_bar {
                sb.set_recording_elapsed(mtm, 0);
            }
            rt.session = Some(session);

            let rt_tick = Rc::clone(runtime);
            let block = RcBlock::new(move |_t: NonNull<NSTimer>| {
                let Some(m) = MainThreadMarker::new() else {
                    return;
                };
                let rt_ref = rt_tick.borrow();
                if let (Some(sess), Some(sb)) = (&rt_ref.session, &rt_ref.status_bar) {
                    sb.set_recording_elapsed(m, sess.elapsed().as_secs());
                }
            });
            let timer =
                unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.5, true, &block) };
            rt.rec_timer = Some(timer);
        }
        Err(err) => show_error_alert(mtm, "Recording Failed to Start", &err),
    }
}

fn stop_active_recording(mtm: MainThreadMarker, runtime: &Rc<RefCell<AppRuntime>>) {
    let session = {
        let mut rt = runtime.borrow_mut();
        if let Some(timer) = rt.rec_timer.take() {
            timer.invalidate();
        }
        if let Some(sb) = &rt.status_bar {
            sb.set_idle(mtm);
        }
        rt.session.take()
    };

    let Some(sess) = session else {
        return;
    };

    match sess.stop() {
        Ok(saved_path) => {
            if let Err(err) = open_editor_for_file(mtm, runtime, &saved_path) {
                show_error_alert(mtm, "Failed Opening Editor", &err);
            }
        }
        Err(err) => show_error_alert(mtm, "Recording Error", &err),
    }
}

fn open_editor_for_file(
    mtm: MainThreadMarker,
    runtime: &Rc<RefCell<AppRuntime>>,
    video_path: &std::path::Path,
) -> Result<(), String> {
    let prefs = runtime.borrow().prefs.clone();
    let rt_dir = Rc::clone(runtime);
    let editor = EditorWindow::open(
        mtm,
        video_path,
        &prefs,
        false,
        Rc::new(move |new_dir| {
            let mut rt = rt_dir.borrow_mut();
            rt.prefs.export_dir = Some(new_dir);
            let _ = rt.prefs.save();
        }),
    )?;
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    runtime.borrow_mut().editors.push(editor);
    Ok(())
}

fn prompt_open_video_in_editor(mtm: MainThreadMarker, runtime: &Rc<RefCell<AppRuntime>>) {
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(false);
    if panel.runModal() != NSModalResponseOK {
        return;
    }
    if let Some(url) = panel.URL() {
        if let Some(p) = url.path() {
            let path = PathBuf::from(p.to_string());
            if let Err(err) = open_editor_for_file(mtm, runtime, &path) {
                show_error_alert(mtm, "Cannot Open Video", &err);
            }
        }
    }
}

fn cleanup_and_quit(mtm: MainThreadMarker, runtime: &Rc<RefCell<AppRuntime>>) {
    {
        let mut rt = runtime.borrow_mut();
        if let Some(t) = rt.rec_timer.take() {
            t.invalidate();
        }
        if let Some(sess) = rt.session.take() {
            let _ = sess.stop();
        }
        for tmp in rt.temp_files.drain(..) {
            let _ = std::fs::remove_file(tmp);
        }
    }
    let app = NSApplication::sharedApplication(mtm);
    app.terminate(None);
}

fn install_shortcut_monitors(runtime: &Rc<RefCell<AppRuntime>>) {
    // Global Cmd+Shift+6 via Carbon HotKey (works across all apps with zero Input Monitoring TCC)
    let rt_global = Rc::clone(runtime);
    crate::ui::hotkey::register_global_cmd_shift_6(move |m| {
        toggle_select_or_stop(m, &rt_global);
    });

    // Local Escape key cancels active selector overlay
    let rt_local = Rc::clone(runtime);
    let local_block = RcBlock::new(move |ev: NonNull<NSEvent>| -> *mut NSEvent {
        let event = unsafe { ev.as_ref() };
        if event.keyCode() == 53 {
            if let Some(ov) = rt_local.borrow_mut().overlay.take() {
                ov.close();
                return std::ptr::null_mut();
            }
        }
        ev.as_ptr()
    });
    unsafe {
        let _ = NSEvent::addLocalMonitorForEventsMatchingMask_handler(
            NSEventMask::KeyDown,
            &local_block,
        );
    }
}

fn run_demo_flow(
    mtm: MainThreadMarker,
    app: &NSApplication,
    runtime: &Rc<RefCell<AppRuntime>>,
    demo: DemoMode,
) -> Result<(), String> {
    match demo {
        DemoMode::Overlay { screenshot_out } => {
            let prefs = Preferences::default();
            let displays = vec![DisplayInfo {
                display_id: 1,
                width: 1920,
                height: 1080,
                frame: crate::geometry::Rect::new(0.0, 0.0, 1920.0, 1080.0),
            }];
            let ov = SelectorOverlay::show(
                mtm,
                &prefs,
                displays,
                Vec::new(),
                true,
                Rc::new(|_| {}),
                Rc::new(|| {}),
            )?;
            if let Some(shot_path) = screenshot_out {
                let win_clone = ov.canvas_window.clone();
                let block = RcBlock::new(move |t: NonNull<NSTimer>| {
                    unsafe { t.as_ref().invalidate() };
                    let _ = capture_window_view_to_png(&win_clone, &shot_path);
                    if let Some(m) = MainThreadMarker::new() {
                        NSApplication::sharedApplication(m).terminate(None);
                    }
                });
                unsafe {
                    let _ =
                        NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.35, false, &block);
                }
            }
            runtime.borrow_mut().overlay = Some(ov);
            app.run();
        }
        DemoMode::Editor {
            input,
            screenshot_out,
        } => {
            let prefs = Preferences::default();
            let ed = EditorWindow::open(mtm, &input, &prefs, true, Rc::new(|_| {}))?;
            if let Some(shot_path) = screenshot_out {
                let win_clone = ed.window.clone();
                let block = RcBlock::new(move |t: NonNull<NSTimer>| {
                    unsafe { t.as_ref().invalidate() };
                    let _ = capture_window_view_to_png(&win_clone, &shot_path);
                    if let Some(m) = MainThreadMarker::new() {
                        NSApplication::sharedApplication(m).terminate(None);
                    }
                });
                unsafe {
                    let _ =
                        NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.35, false, &block);
                }
            }
            runtime.borrow_mut().editors.push(ed);
            app.run();
        }
    }
    Ok(())
}

fn show_error_alert(mtm: MainThreadMarker, title: &str, message: &str) {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    alert.runModal();
}
