use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::mpsc;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSApplication};
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer};

use crate::capture::audio_devices::{
    MicPermissionStatus, microphone_permission_status, request_microphone_permission_channel,
};
use crate::capture::recorder::StopPollState;
use crate::capture::shareable::query_shareable_content;
use crate::capture::{ActiveRecording, AudioCaptureMode, CaptureConfig};
use crate::config::Preferences;
use crate::permissions::{has_screen_capture_permission, open_microphone_settings};
use crate::ui::editor::{EditorOpenOptions, EditorWindow};
use crate::ui::folder_picker::show_error_alert;
use crate::ui::onboarding_visual::OnboardingStep;
use crate::ui::overlay::SelectorOverlay;
use crate::ui::{AppRuntime, apply_export_dir_change, open_onboarding};

pub(crate) fn toggle_select_or_stop(mtm: MainThreadMarker, runtime: &Rc<RefCell<AppRuntime>>) {
    let (busy_transition, has_session) = {
        let rt = runtime.borrow();
        (
            rt.pending_stop.is_some() || rt.mic_perm_timer.is_some(),
            rt.session.is_some(),
        )
    };
    if busy_transition {
        return;
    }
    if has_session {
        stop_active_recording(mtm, runtime);
    } else {
        show_selector(mtm, runtime);
    }
}

pub(crate) fn show_selector(mtm: MainThreadMarker, runtime: &Rc<RefCell<AppRuntime>>) {
    {
        let rt = runtime.borrow();
        if rt.session.is_some() || rt.pending_stop.is_some() || rt.mic_perm_timer.is_some() {
            return;
        }
    }
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

pub(crate) fn should_complete_tutorial_on_save(
    active_tutorial_gen: Option<u64>,
    editor_tutorial_gen: Option<u64>,
    recording_active: bool,
) -> bool {
    !recording_active && active_tutorial_gen.is_some() && active_tutorial_gen == editor_tutorial_gen
}

pub(crate) fn cancel_overlay(runtime: &Rc<RefCell<AppRuntime>>) {
    let resume_tutorial = {
        let mut rt = runtime.borrow_mut();
        if let Some(ov) = rt.overlay.take() {
            ov.close();
        }
        rt.tutorial_generation.is_some() && has_screen_capture_permission()
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
        rt.tutorial_generation.is_some()
    };

    if !cfg.audio_mode.captures_mic() {
        launch_recording_session(mtm, runtime, cfg, tutorial_active);
        return;
    }

    match microphone_permission_status() {
        MicPermissionStatus::Authorized => {
            launch_recording_session(mtm, runtime, cfg, tutorial_active);
        }
        MicPermissionStatus::Denied => {
            handle_mic_permission_denied(mtm, runtime, cfg, tutorial_active);
        }
        MicPermissionStatus::NotDetermined => {
            // Request macOS native mic prompt asynchronously without blocking the AppKit runloop.
            let rx = request_microphone_permission_channel();
            let pending_cfg = RefCell::new(Some(cfg));
            let rt_poll = Rc::clone(runtime);
            let poll_block = RcBlock::new(move |timer: NonNull<NSTimer>| {
                let granted = match rx.try_recv() {
                    Err(mpsc::TryRecvError::Empty) => return,
                    Ok(ok) => ok,
                    Err(mpsc::TryRecvError::Disconnected) => false,
                };
                unsafe { timer.as_ref().invalidate() };
                rt_poll.borrow_mut().mic_perm_timer = None;
                let Some(cfg) = pending_cfg.borrow_mut().take() else {
                    return;
                };
                let Some(m) = MainThreadMarker::new() else {
                    return;
                };
                if granted {
                    launch_recording_session(m, &rt_poll, cfg, tutorial_active);
                } else {
                    handle_mic_permission_denied(m, &rt_poll, cfg, tutorial_active);
                }
            });
            let timer = unsafe {
                let t =
                    NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.05, true, &poll_block);
                NSRunLoop::mainRunLoop().addTimer_forMode(&t, NSRunLoopCommonModes);
                t
            };
            runtime.borrow_mut().mic_perm_timer = Some(timer);
        }
    }
}

fn handle_mic_permission_denied(
    mtm: MainThreadMarker,
    runtime: &Rc<RefCell<AppRuntime>>,
    mut cfg: CaptureConfig,
    tutorial_active: bool,
) {
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);

    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Microphone Access Denied"));
    alert.setInformativeText(&NSString::from_str(
        "Microphone permission is turned off for Quick Screen Recorder. You can record without a microphone now, or open System Settings to enable it.",
    ));
    alert.addButtonWithTitle(&NSString::from_str("Record Without Mic"));
    alert.addButtonWithTitle(&NSString::from_str("Open Microphone Settings"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));

    let response = alert.runModal();
    if response == NSAlertFirstButtonReturn {
        cfg.audio_mode = AudioCaptureMode::from_split(cfg.audio_mode.captures_system(), false);
        cfg.microphone_device_id = None;
        launch_recording_session(mtm, runtime, cfg, tutorial_active);
    } else if response == NSAlertSecondButtonReturn {
        let _ = open_microphone_settings();
    }
}

fn launch_recording_session(
    mtm: MainThreadMarker,
    runtime: &Rc<RefCell<AppRuntime>>,
    cfg: CaptureConfig,
    tutorial_active: bool,
) {
    let raw_out = match crate::fs_safety::unique_private_file_path("capture", "mp4") {
        Ok(p) => p,
        Err(err) => {
            show_error_alert(mtm, "Cache Directory Error", &err);
            return;
        }
    };
    let source_app = cfg.source_app_name.clone();
    match ActiveRecording::start(&cfg, &raw_out) {
        Ok(session) => {
            {
                let mut rt = runtime.borrow_mut();
                rt.temp_files.push(raw_out);
                rt.active_capture_app = source_app;
                let init_tl_frames = session.timelapse_frames();
                if let Some(sb) = &rt.status_bar {
                    sb.set_recording_elapsed(mtm, 0, init_tl_frames);
                }
                rt.session = Some(session);
                let rt_tick = Rc::clone(runtime);
                let block = RcBlock::new(move |_t: NonNull<NSTimer>| {
                    let Some(m) = MainThreadMarker::new() else {
                        return;
                    };
                    let worker_aborted = {
                        let r = rt_tick.borrow();
                        if let (Some(sess), Some(sb)) = (&r.session, &r.status_bar) {
                            sb.set_recording_elapsed(
                                m,
                                sess.elapsed().as_secs(),
                                sess.timelapse_frames(),
                            );
                            sess.has_ended_unexpectedly()
                        } else {
                            false
                        }
                    };
                    if worker_aborted {
                        stop_active_recording(m, &rt_tick);
                    }
                });
                rt.rec_timer = Some(unsafe {
                    let t =
                        NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.5, true, &block);
                    NSRunLoop::mainRunLoop().addTimer_forMode(&t, NSRunLoopCommonModes);
                    t
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
    let (session, captured_app, tutorial_gen) = {
        let mut rt = runtime.borrow_mut();
        if rt.pending_stop.is_some() {
            return;
        }
        let Some(sess) = rt.session.take() else {
            return;
        };
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
        match pending.finish() {
            Ok(outcome) => {
                if should_quit {
                    if let Some(partial_err) = outcome.partial_error {
                        show_error_alert(
                            m,
                            "Recording Saved With Warning",
                            &format!(
                                "The recording finished saving before quit, with a warning: {partial_err}"
                            ),
                        );
                    }
                    crate::ui::finish_app_termination(m, &rt_poll);
                    return;
                }
                if let Err(err) =
                    open_editor_for_file(m, &rt_poll, &outcome.output_path, app_name, tutorial_gen)
                {
                    show_error_alert(m, "Failed Opening Editor", &err);
                }
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
            Err(err) => {
                show_error_alert(m, "Recording Error", &err);
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
}
