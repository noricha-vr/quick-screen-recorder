use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use objc2_foundation::NSTimer;

use crate::capture::shareable::DisplayInfo;
use crate::cli::DemoMode;
use crate::config::Preferences;
use crate::ui::clipboard::capture_window_view_to_png;
use crate::ui::editor::{EditorOpenOptions, EditorWindow};
use crate::ui::onboarding::{OnboardingCallbacks, OnboardingWindow};
use crate::ui::onboarding_visual::OnboardingStep;
use crate::ui::overlay::SelectorOverlay;

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
                schedule_screenshot_and_quit(ov.canvas_window.clone(), shot_path);
            }
            app.run();
            drop(ov);
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
