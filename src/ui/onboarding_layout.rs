use std::path::Path;

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSBackingStoreType, NSBox,
    NSBoxType, NSButton, NSColor, NSFont, NSScreen, NSTextAlignment, NSTextField, NSView, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use crate::fs_safety::{format_tilde_path, sanitize_terminal_text};
use crate::ui::onboarding_visual::{OnboardingStep, TutorialVisualView};

pub const ONBOARDING_WIN_W: f64 = 580.0;
pub const ONBOARDING_WIN_H: f64 = 486.0;

pub struct OnboardingUi {
    pub window: Retained<NSWindow>,
    pub title_label: Retained<NSTextField>,
    pub step_labels: [Retained<NSTextField>; 3],
    pub perm_card: Retained<NSBox>,
    pub perm_dot: Retained<NSBox>,
    pub perm_title: Retained<NSTextField>,
    pub perm_hint: Retained<NSTextField>,
    pub perm_open_btn: Retained<NSButton>,
    pub visual: Retained<TutorialVisualView>,
    pub body_label: Retained<NSTextField>,
    pub secondary_btn: Retained<NSButton>,
    pub primary_btn: Retained<NSButton>,
    pub waiting_label: Retained<NSTextField>,
}

pub fn build_onboarding_ui(mtm: MainThreadMarker, initial_step: OnboardingStep) -> OnboardingUi {
    let style =
        NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable;
    let content_rect = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(ONBOARDING_WIN_W, ONBOARDING_WIN_H),
    );
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            content_rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    if let Some(dark) = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }) {
        window.setAppearance(Some(&dark));
    }
    window.setTitle(&NSString::from_str("Quick Screen Recorder — Quick Start"));
    window.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.09, 0.10, 0.14, 1.0,
    )));
    position_onboarding_window(mtm, &window, false);

    let root = NSView::initWithFrame(NSView::alloc(mtm), content_rect);
    window.setContentView(Some(&root));

    let full_bg = NSBox::initWithFrame(NSBox::alloc(mtm), content_rect);
    full_bg.setBoxType(NSBoxType::Custom);
    full_bg.setBorderWidth(0.0);
    full_bg.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.09, 0.10, 0.14, 1.0,
    ));
    root.addSubview(&full_bg);

    let title_label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    title_label.setFont(Some(&NSFont::systemFontOfSize_weight(19.0, 0.35)));
    title_label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.95, 0.97, 1.0, 1.0,
    )));
    title_label.setFrame(NSRect::new(
        NSPoint::new(24.0, 434.0),
        NSSize::new(276.0, 28.0),
    ));
    root.addSubview(&title_label);

    let step_specs = [
        ("1 Permission", 304.0, 92.0),
        ("2 Record", 402.0, 74.0),
        ("3 Save", 482.0, 74.0),
    ];
    let step_labels = std::array::from_fn(|i| {
        let (name, x, w) = step_specs[i];
        let lbl = NSTextField::labelWithString(&NSString::from_str(name), mtm);
        lbl.setFont(Some(&NSFont::systemFontOfSize_weight(11.0, 0.25)));
        lbl.setAlignment(NSTextAlignment::Right);
        lbl.setFrame(NSRect::new(NSPoint::new(x, 439.0), NSSize::new(w, 18.0)));
        root.addSubview(&lbl);
        lbl
    });

    let perm_card = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        NSRect::new(
            NSPoint::new(24.0, 374.0),
            NSSize::new(ONBOARDING_WIN_W - 48.0, 48.0),
        ),
    );
    perm_card.setBoxType(NSBoxType::Custom);
    perm_card.setCornerRadius(10.0);
    perm_card.setBorderWidth(1.0);
    root.addSubview(&perm_card);

    let perm_dot = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        NSRect::new(NSPoint::new(38.0, 392.0), NSSize::new(12.0, 12.0)),
    );
    perm_dot.setBoxType(NSBoxType::Custom);
    perm_dot.setCornerRadius(6.0);
    perm_dot.setBorderWidth(0.0);
    root.addSubview(&perm_dot);

    let perm_title = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    perm_title.setFont(Some(&NSFont::systemFontOfSize_weight(13.0, 0.3)));
    perm_title.setFrame(NSRect::new(
        NSPoint::new(58.0, 388.0),
        NSSize::new(326.0, 20.0),
    ));
    root.addSubview(&perm_title);

    let perm_hint = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    perm_hint.setFont(Some(&NSFont::systemFontOfSize_weight(11.5, 0.15)));
    perm_hint.setAlignment(NSTextAlignment::Right);
    perm_hint.setFrame(NSRect::new(
        NSPoint::new(340.0, 389.0),
        NSSize::new(202.0, 18.0),
    ));
    root.addSubview(&perm_hint);

    let perm_open_btn = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Open Settings →"),
            None,
            None,
            mtm,
        )
    };
    perm_open_btn.setBezelColor(Some(&NSColor::systemBlueColor()));
    perm_open_btn.setFont(Some(&NSFont::boldSystemFontOfSize(12.5)));
    perm_open_btn.setFrame(NSRect::new(
        NSPoint::new(398.0, 384.0),
        NSSize::new(144.0, 28.0),
    ));
    perm_open_btn.setHidden(true);
    root.addSubview(&perm_open_btn);

    let visual = TutorialVisualView::new(
        mtm,
        NSRect::new(
            NSPoint::new(24.0, 136.0),
            NSSize::new(ONBOARDING_WIN_W - 48.0, 226.0),
        ),
        initial_step,
    );
    root.addSubview(&visual);

    let body_label = NSTextField::wrappingLabelWithString(&NSString::from_str(""), mtm);
    body_label.setFont(Some(&NSFont::systemFontOfSize_weight(12.5, 0.05)));
    body_label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.78, 0.84, 0.93, 1.0,
    )));
    body_label.setFrame(NSRect::new(
        NSPoint::new(24.0, 74.0),
        NSSize::new(ONBOARDING_WIN_W - 48.0, 48.0),
    ));
    root.addSubview(&body_label);

    let secondary_btn = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Skip Tutorial"),
            None,
            None,
            mtm,
        )
    };
    secondary_btn.setFrame(NSRect::new(
        NSPoint::new(24.0, 20.0),
        NSSize::new(136.0, 34.0),
    ));
    root.addSubview(&secondary_btn);

    let primary_btn = unsafe {
        NSButton::buttonWithTitle_target_action(&NSString::from_str(""), None, None, mtm)
    };
    primary_btn.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
    primary_btn.setFrame(NSRect::new(
        NSPoint::new(ONBOARDING_WIN_W - 272.0, 20.0),
        NSSize::new(248.0, 34.0),
    ));
    root.addSubview(&primary_btn);

    let waiting_label = NSTextField::labelWithString(
        &NSString::from_str("Waiting for Screen Recording permission..."),
        mtm,
    );
    waiting_label.setFont(Some(&NSFont::systemFontOfSize_weight(12.0, 0.1)));
    waiting_label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.72, 0.78, 0.88, 0.90,
    )));
    waiting_label.setAlignment(NSTextAlignment::Right);
    waiting_label.setFrame(NSRect::new(
        NSPoint::new(176.0, 27.0),
        NSSize::new(ONBOARDING_WIN_W - 200.0, 20.0),
    ));
    waiting_label.setHidden(true);
    root.addSubview(&waiting_label);

    OnboardingUi {
        window,
        title_label,
        step_labels,
        perm_card,
        perm_dot,
        perm_title,
        perm_hint,
        perm_open_btn,
        visual,
        body_label,
        secondary_btn,
        primary_btn,
        waiting_label,
    }
}

pub fn position_onboarding_window(mtm: MainThreadMarker, window: &NSWindow, dock_top_right: bool) {
    let screen = NSScreen::mainScreen(mtm)
        .map(|s| s.visibleFrame())
        .unwrap_or_else(|| NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1440.0, 900.0)));
    let frame_size = window.frame().size;
    let origin = if dock_top_right {
        NSPoint::new(
            (screen.origin.x + screen.size.width - frame_size.width - 20.0)
                .max(screen.origin.x + 16.0),
            (screen.origin.y + screen.size.height - frame_size.height - 16.0)
                .max(screen.origin.y + 16.0),
        )
    } else {
        NSPoint::new(
            screen.origin.x + (screen.size.width - frame_size.width) * 0.5,
            screen.origin.y + (screen.size.height - frame_size.height) * 0.56,
        )
    };
    window.setFrameOrigin(origin);
}

pub fn apply_step_ui(
    ui: &OnboardingUi,
    step: OnboardingStep,
    permission_granted: bool,
    saved_path: Option<&Path>,
    phase: f64,
) {
    let title = match step {
        OnboardingStep::Permission => "Enable Screen Recording",
        OnboardingStep::StartRecording => "Try Your First Recording",
        OnboardingStep::RecordingInProgress => "Recording in Progress",
        OnboardingStep::Completed => "You're All Set!",
    };
    ui.title_label.setStringValue(&NSString::from_str(title));

    let cur_idx = step.progress_index();
    let names = ["Permission", "Record", "Save"];
    for (i, lbl) in ui.step_labels.iter().enumerate() {
        let (text, color) = if i < cur_idx {
            (
                format!("✓ {}", names[i]),
                NSColor::colorWithSRGBRed_green_blue_alpha(0.32, 0.88, 0.48, 1.0),
            )
        } else if i == cur_idx {
            (
                format!("{} {}", i + 1, names[i]),
                NSColor::colorWithSRGBRed_green_blue_alpha(0.46, 0.76, 1.0, 1.0),
            )
        } else {
            (
                format!("{} {}", i + 1, names[i]),
                NSColor::colorWithSRGBRed_green_blue_alpha(0.46, 0.52, 0.62, 0.9),
            )
        };
        lbl.setStringValue(&NSString::from_str(&text));
        lbl.setTextColor(Some(&color));
    }

    if permission_granted {
        ui.perm_card
            .setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
                0.06, 0.21, 0.12, 1.0,
            ));
        ui.perm_card
            .setBorderColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
                0.19, 0.82, 0.35, 0.55,
            ));
        ui.perm_dot
            .setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
                0.19, 0.86, 0.38, 1.0,
            ));
        ui.perm_title
            .setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
                0.76, 0.98, 0.82, 1.0,
            )));
        ui.perm_title
            .setStringValue(&NSString::from_str("Screen Recording: Enabled"));
        ui.perm_hint
            .setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
                0.52, 0.86, 0.62, 0.95,
            )));
        ui.perm_hint
            .setStringValue(&NSString::from_str("Ready to capture"));
        ui.perm_hint.setHidden(false);
        ui.perm_open_btn.setHidden(true);
        ui.perm_open_btn.setKeyEquivalent(&NSString::from_str(""));
    } else {
        ui.perm_card
            .setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
                0.22, 0.14, 0.05, 1.0,
            ));
        ui.perm_card
            .setBorderColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
                0.96, 0.66, 0.18, 0.58,
            ));
        ui.perm_dot
            .setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
                0.98, 0.68, 0.18, 1.0,
            ));
        ui.perm_title
            .setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
                1.0, 0.88, 0.64, 1.0,
            )));
        ui.perm_title
            .setStringValue(&NSString::from_str("Screen Recording: Not Enabled"));
        ui.perm_hint.setHidden(true);
        ui.perm_open_btn.setHidden(false);
        ui.perm_open_btn.setKeyEquivalent(&NSString::from_str("\r"));
    }

    ui.visual.set_step(step, phase);
    if let Some(path) = saved_path {
        let fname = path
            .file_name()
            .map(|n| sanitize_terminal_text(&n.to_string_lossy(), false))
            .unwrap_or_default();
        let folder = path
            .parent()
            .map(|p| format!("Saved in {}", format_tilde_path(p)))
            .unwrap_or_default();
        ui.visual.set_saved_summary(fname, folder);
    }

    match step {
        OnboardingStep::Permission => {
            ui.body_label.setStringValue(&NSString::from_str(
                "Click \"Open Settings →\" in the banner above, toggle Quick Screen Recorder ON, then click \"Quit & Reopen\" when macOS prompts.",
            ));
            ui.secondary_btn.setHidden(false);
            ui.secondary_btn
                .setTitle(&NSString::from_str("Skip Tutorial"));
            ui.secondary_btn.setKeyEquivalent(&NSString::from_str(""));
            ui.primary_btn.setKeyEquivalent(&NSString::from_str(""));
            ui.primary_btn.setHidden(!permission_granted);
            ui.waiting_label.setHidden(permission_granted);
            if permission_granted {
                ui.primary_btn
                    .setTitle(&NSString::from_str("Open Selector (⇧⌘6)"));
                ui.primary_btn.setBezelColor(None);
            }
        }
        OnboardingStep::StartRecording => {
            ui.body_label.setStringValue(&NSString::from_str(
                "Click ◉ Rec in the top-right menu bar → New Recording... (or press ⇧⌘6) to try a short recording and save your first clip.",
            ));
            ui.secondary_btn.setHidden(false);
            ui.secondary_btn
                .setTitle(&NSString::from_str("Skip Tutorial"));
            ui.secondary_btn.setKeyEquivalent(&NSString::from_str(""));
            ui.primary_btn.setHidden(false);
            ui.waiting_label.setHidden(true);
            ui.primary_btn
                .setTitle(&NSString::from_str("Open Selector (⇧⌘6)"));
            ui.primary_btn.setBezelColor(None);
            ui.primary_btn.setKeyEquivalent(&NSString::from_str(""));
        }
        OnboardingStep::RecordingInProgress => {
            ui.body_label.setStringValue(&NSString::from_str(
                "Recording now! Click ● 00:xx in the top-right menu bar → Stop Recording (or press ⇧⌘6) to open the Trim & Export editor.",
            ));
            ui.secondary_btn.setHidden(false);
            ui.secondary_btn
                .setTitle(&NSString::from_str("Skip Tutorial"));
            ui.secondary_btn.setKeyEquivalent(&NSString::from_str(""));
            ui.primary_btn.setHidden(false);
            ui.waiting_label.setHidden(true);
            ui.primary_btn
                .setTitle(&NSString::from_str("Stop Recording"));
            ui.primary_btn
                .setBezelColor(Some(&NSColor::systemRedColor()));
            ui.primary_btn.setKeyEquivalent(&NSString::from_str(""));
        }
        OnboardingStep::Completed => {
            let footer = completed_footer_spec(saved_path.is_some());
            ui.body_label.setStringValue(&NSString::from_str(
                "Your clip is saved. Start a new recording anytime from ◉ Rec in your menu bar or press ⇧⌘6.",
            ));
            if let Some(sec_title) = footer.secondary_title {
                ui.secondary_btn.setHidden(false);
                ui.secondary_btn.setTitle(&NSString::from_str(sec_title));
                ui.secondary_btn
                    .setKeyEquivalent(&NSString::from_str("\u{1b}"));
            } else {
                ui.secondary_btn.setHidden(true);
                ui.secondary_btn.setKeyEquivalent(&NSString::from_str(""));
            }
            ui.primary_btn.setHidden(false);
            ui.waiting_label.setHidden(true);
            ui.primary_btn
                .setTitle(&NSString::from_str(footer.primary_title));
            ui.primary_btn
                .setBezelColor(Some(&NSColor::systemBlueColor()));
            ui.primary_btn.setKeyEquivalent(&NSString::from_str("\r"));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompletedFooterSpec {
    pub secondary_title: Option<&'static str>,
    pub primary_title: &'static str,
}

pub(crate) fn completed_footer_spec(has_saved_path: bool) -> CompletedFooterSpec {
    if has_saved_path {
        CompletedFooterSpec {
            secondary_title: Some("Close"),
            primary_title: "Show in Finder",
        }
    } else {
        CompletedFooterSpec {
            secondary_title: None,
            primary_title: "Close",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_footer_promotes_show_in_finder_to_primary_and_close_to_secondary() {
        let with_file = completed_footer_spec(true);
        assert_eq!(with_file.secondary_title, Some("Close"));
        assert_eq!(with_file.primary_title, "Show in Finder");

        let without_file = completed_footer_spec(false);
        assert_eq!(without_file.secondary_title, None);
        assert_eq!(without_file.primary_title, "Close");
    }
}
