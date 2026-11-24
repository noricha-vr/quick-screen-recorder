use std::cell::RefCell;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSEvent, NSEventModifierFlags, NSEventType, NSScreen};
use objc2_foundation::{NSPoint, NSString, NSTimer};

use crate::capture::shareable::{DisplayInfo, WindowInfo};
use crate::cli::DemoMode;
use crate::config::Preferences;
use crate::geometry::Rect;
use crate::ui::clipboard::capture_window_view_to_png;
use crate::ui::editor::{EditorOpenOptions, EditorWindow};
use crate::ui::onboarding::{OnboardingCallbacks, OnboardingWindow};
use crate::ui::onboarding_visual::OnboardingStep;
use crate::ui::overlay::{
    SelectorOverlay, commit_toolbar_text_editing, extract_nsscreen_display_id, nsrect_to_rect,
};

pub fn run_demo_flow(
    mtm: MainThreadMarker,
    app: &NSApplication,
    demo: DemoMode,
) -> Result<(), String> {
    match demo {
        DemoMode::Overlay { screenshot_out } => {
            let prefs = Preferences::default();
            let displays = vec![DisplayInfo {
                display_id: 1,
                width: 1920,
                height: 1080,
                frame: Rect::new(0.0, 0.0, 1920.0, 1080.0),
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
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
            ov.canvas_window.makeKeyAndOrderFront(None);
            if let Some(shot_path) = screenshot_out {
                schedule_screenshot_and_quit(ov.canvas_window.clone(), shot_path);
            }
            app.run();
            drop(ov);
        }
        DemoMode::OverlayWindow { screenshot_out } => {
            let prefs = Preferences::default();
            let displays = vec![DisplayInfo {
                display_id: 1,
                width: 1080,
                height: 680,
                frame: Rect::new(0.0, 0.0, 1080.0, 680.0),
            }];
            // Two overlapping mock windows (AppKit top-left coordinates for SCWindow frame):
            // Window 0 is selected; Window 1 sits behind the floating toolbar to verify clean hover.
            let windows = vec![
                WindowInfo {
                    window_id: 101,
                    title: "Project Editor".to_string(),
                    app_name: "Demo Studio".to_string(),
                    frame: Rect::new(140.0, 120.0, 760.0, 390.0),
                },
                WindowInfo {
                    window_id: 102,
                    title: "Background Browser".to_string(),
                    app_name: "Demo Web".to_string(),
                    frame: Rect::new(80.0, 60.0, 920.0, 580.0),
                },
            ];
            let ov = SelectorOverlay::show(
                mtm,
                &prefs,
                displays,
                windows,
                true,
                Rc::new(|_| {}),
                Rc::new(|| {}),
            )?;
            ov.select_mode_segment(1);
            // Simulate pointer directly over the toolbar (where Window 1 also exists behind it):
            // hover highlight MUST be suppressed while controls render in active key state.
            let tb_frame = ov.canvas_view.ivars_toolbar_rect_demo();
            let over_tb = NSPoint::new(
                tb_frame.x + tb_frame.width * 0.5,
                tb_frame.y + tb_frame.height * 0.5,
            );
            ov.canvas_view.update_hover_for_local_point(
                over_tb,
                ov.canvas_view.is_local_point_over_toolbar(over_tb),
            );
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
            ov.canvas_window.makeKeyAndOrderFront(None);
            if let Some(shot_path) = screenshot_out {
                schedule_screenshot_and_quit(ov.canvas_window.clone(), shot_path);
            }
            app.run();
            drop(ov);
        }
        DemoMode::VerifyOverlayLive => {
            return run_live_two_window_overlay_verification(mtm, app);
        }
        DemoMode::Editor {
            input,
            screenshot_out,
        } => {
            let prefs = Preferences::default();
            let ed = EditorWindow::open(
                mtm,
                &input,
                &prefs,
                EditorOpenOptions {
                    captured_app_name: None,
                    demo_showcase: true,
                    tutorial_hint: false,
                    initial_notice: None,
                    on_export_dir_change: Rc::new(|_| {}),
                    on_export_saved: Rc::new(|_| {}),
                },
            )?;
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
            ed.window.makeKeyAndOrderFront(None);
            if let Some(shot_path) = screenshot_out {
                schedule_screenshot_and_quit(ed.window.clone(), shot_path);
            }
            app.run();
            drop(ed);
        }
        DemoMode::Onboarding {
            granted,
            completed,
            screenshot_out,
        } => {
            let (step, freeze_phase, sample_saved) = if completed {
                (
                    OnboardingStep::Completed,
                    Some(0.0),
                    Some(PathBuf::from(
                        "~/Movies/Quick Screen Recorder/2026-11-24_09-30-00_Quick-Screen-Recorder.mp4",
                    )),
                )
            } else if granted {
                (OnboardingStep::StartRecording, Some(0.80), None)
            } else {
                (OnboardingStep::Permission, Some(0.46), None)
            };
            let ob = OnboardingWindow::open(
                mtm,
                step,
                granted || completed,
                sample_saved,
                freeze_phase,
                OnboardingCallbacks {
                    on_start_recording: Box::new(|_| {}),
                    on_stop_recording: Box::new(|_| {}),
                    on_skip: Box::new(|_| {}),
                    on_done: Box::new(|_| {}),
                },
            );
            if let Some(shot_path) = screenshot_out {
                schedule_screenshot_and_quit(ob.window.clone(), shot_path);
            }
            app.run();
            drop(ob);
        }
    }
    Ok(())
}

/// Real two-window AppKit self-test exercising the live (`demo_backdrop = false`) overlay
/// windows, key-window ownership, toolbar hover suppression over overlapping background windows,
/// and non-reentrant text-field commit without requiring screen recording TCC.
fn run_live_two_window_overlay_verification(
    mtm: MainThreadMarker,
    app: &NSApplication,
) -> Result<(), String> {
    let screen = NSScreen::mainScreen(mtm).ok_or("No active NSScreen for live verification")?;
    let sf = nsrect_to_rect(screen.frame());
    let display_id = extract_nsscreen_display_id(&screen).unwrap_or(1);
    let scale = screen.backingScaleFactor().max(1.0);
    let displays = vec![DisplayInfo {
        display_id,
        width: (sf.width * scale).round() as u32,
        height: (sf.height * scale).round() as u32,
        frame: sf,
    }];
    // Window 0 (smaller center) is selected; Window 1 spans nearly the full display (including
    // directly behind the floating toolbar!) so hovering the toolbar tests suppression cleanly.
    let windows = vec![
        WindowInfo {
            window_id: 201,
            title: "Selected Front Window".to_string(),
            app_name: "VerifyApp".to_string(),
            frame: Rect::new(
                sf.x + sf.width * 0.25,
                sf.y + sf.height * 0.22,
                sf.width * 0.45,
                sf.height * 0.42,
            ),
        },
        WindowInfo {
            window_id: 202,
            title: "Full Behind-Toolbar Window".to_string(),
            app_name: "VerifyBackdrop".to_string(),
            frame: Rect::new(sf.x + 20.0, sf.y + 20.0, sf.width - 40.0, sf.height - 40.0),
        },
    ];
    let prefs = Preferences::default();
    let ov = SelectorOverlay::show(
        mtm,
        &prefs,
        displays,
        windows,
        false,
        Rc::new(|_| {}),
        Rc::new(|| {}),
    )?;
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    ov.canvas_window.orderFrontRegardless();
    ov.toolbar_window.makeKeyAndOrderFront(None);

    let ov_cell = Rc::new(RefCell::new(Some(ov)));
    let err_cell = Rc::new(RefCell::new(Option::<String>::None));
    let ov_chk = Rc::clone(&ov_cell);
    let err_chk = Rc::clone(&err_cell);

    let block = RcBlock::new(move |t: NonNull<NSTimer>| {
        unsafe { t.as_ref().invalidate() };
        let res = (|| -> Result<(), String> {
            let guard = ov_chk.borrow();
            let ov = guard.as_ref().ok_or("Overlay dropped early")?;

            if !ov.toolbar_window.isKeyWindow() {
                return Err("toolbar_window must be key window on open".to_string());
            }
            if ov.canvas_window.isKeyWindow() {
                return Err("canvas_window must NOT become key window in live mode".to_string());
            }
            if ov.toolbar_window.level() <= ov.canvas_window.level() {
                return Err("toolbar_window level must stay above canvas_window".to_string());
            }

            // Switch to Area width editing using the REAL AppKit field editor, modify text to
            // "800", commit via `commit_toolbar_text_editing`, and verify both non-reentrancy
            // AND that `OverlayState.selection.width` actually updated to `800 / scale`.
            let _ = ov.toolbar_window.makeFirstResponder(Some(&ov.w_field));
            let editor = ov
                .w_field
                .currentEditor()
                .ok_or("w_field did not activate an AppKit field editor")?;
            editor.setString(&NSString::from_str("800"));
            commit_toolbar_text_editing(&ov.toolbar_window);
            let actual_px_w = (ov.state.borrow().selection.width * scale).round() as i32;
            if (actual_px_w - 800).abs() > 2 {
                return Err(format!(
                    "expected width commit to update selection to ~800px, got {actual_px_w}px"
                ));
            }

            // Switch to Window mode: toolbar MUST stay key window (never dimmed!).
            ov.select_mode_segment(1);
            if !ov.toolbar_window.isKeyWindow() {
                return Err(
                    "toolbar_window lost key status after switching to Window mode".to_string(),
                );
            }
            if ov.canvas_window.isKeyWindow() {
                return Err("canvas_window stole key status in Window mode".to_string());
            }

            // Pick a point directly inside `toolbar_window.frame()` (which also sits over Window 1).
            let tb_screen = nsrect_to_rect(ov.toolbar_window.frame());
            let tb_center_screen = NSPoint::new(
                tb_screen.x + tb_screen.width * 0.5,
                tb_screen.y + tb_screen.height * 0.5,
            );
            let tb_center_local = ov.canvas_window.convertPointFromScreen(tb_center_screen);
            if !ov.canvas_view.is_local_point_over_toolbar(tb_center_local) {
                return Err("center of toolbar_window must be detected as over toolbar".to_string());
            }
            ov.canvas_view
                .update_hover_for_local_point(tb_center_local, true);
            if ov.state.borrow().hovered_window_idx.is_some() {
                return Err("hovered_window_idx must be None while cursor is over toolbar".into());
            }

            // Pick a point outside both the selected Window 0 and toolbar_window, inside Window 1.
            let outside_local = NSPoint::new(40.0, 40.0);
            if ov.canvas_view.is_local_point_over_toolbar(outside_local) {
                return Err("corner point (40,40) unexpectedly fell inside toolbar".to_string());
            }
            ov.canvas_view
                .update_hover_for_local_point(outside_local, false);
            if ov.state.borrow().hovered_window_idx != Some(1) {
                return Err(format!(
                    "expected hovered_window_idx=Some(1) outside toolbar, got {:?}",
                    ov.state.borrow().hovered_window_idx
                ));
            }

            // Move back over toolbar -> background window highlight MUST clear immediately!
            ov.canvas_view
                .update_hover_for_local_point(tb_center_local, true);
            if ov.state.borrow().hovered_window_idx.is_some() {
                return Err(
                    "hovered_window_idx failed to clear when returning cursor over toolbar".into(),
                );
            }
            Ok(())
        })();
        if let Err(msg) = res {
            *err_chk.borrow_mut() = Some(msg);
        }
        if let Some(ov) = ov_chk.borrow_mut().take() {
            ov.close();
        }
        if let Some(m) = MainThreadMarker::new() {
            let app = NSApplication::sharedApplication(m);
            app.stop(None);
            if let Some(wake_ev) = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
                NSEventType::ApplicationDefined,
                NSPoint::new(0.0, 0.0),
                NSEventModifierFlags::empty(),
                0.0,
                0,
                None,
                0,
                0,
                0,
            ) {
                app.postEvent_atStart(&wake_ev, true);
            }
        }
    });
    unsafe {
        let _ = NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.25, false, &block);
    }
    app.run();
    if let Some(err) = err_cell.borrow_mut().take() {
        return Err(err);
    }
    println!("OVERLAY_LIVE_VERIFY_OK");
    Ok(())
}

fn schedule_screenshot_and_quit(
    win: objc2::rc::Retained<objc2_app_kit::NSWindow>,
    shot_path: PathBuf,
) {
    let block = RcBlock::new(move |t: NonNull<NSTimer>| {
        unsafe { t.as_ref().invalidate() };
        let _ = capture_window_view_to_png(&win, &shot_path);
        if let Some(m) = MainThreadMarker::new() {
            NSApplication::sharedApplication(m).terminate(None);
        }
    });
    unsafe {
        let _ = NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.35, false, &block);
    }
}
