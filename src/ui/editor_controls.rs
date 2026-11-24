use std::path::Path;

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBox, NSBoxType, NSButton, NSColor, NSFont, NSImageScaling, NSImageView, NSPopUpButton,
    NSTextAlignment, NSTextField, NSView,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use crate::capture::cadence::closest_editor_fps_preset;
use crate::config::Preferences;
use crate::export::ExportFormat;
use crate::export::avfoundation::extract_rgba_frame;
use crate::fs_safety::format_tilde_path;
use crate::ui::clipboard::rgba_to_nsimage;
use crate::ui::editor_worker::{
    ExportProgressView, FPS_PRESETS, ProgressVisualState, SIZE_PRESETS,
};
use crate::ui::folder_picker::{FolderMenuItem, populate_folder_popup};

pub struct EditorControls {
    pub image_view: Retained<NSImageView>,
    pub format_popup: Retained<NSPopUpButton>,
    pub fps_popup: Retained<NSPopUpButton>,
    pub size_popup: Retained<NSPopUpButton>,
    pub folder_popup: Retained<NSPopUpButton>,
    pub folder_items: Vec<FolderMenuItem>,
    pub name_field: Retained<NSTextField>,
    pub ext_label: Retained<NSTextField>,
    pub mute_checkbox: Retained<NSButton>,
    pub status_label: Retained<NSTextField>,
    pub progress_view: Retained<ExportProgressView>,
    pub copy_btn: Retained<NSButton>,
    pub save_btn: Retained<NSButton>,
    pub initial_fmt: ExportFormat,
    pub initial_fps: u32,
    pub initial_max_width: Option<u32>,
}

#[allow(clippy::too_many_arguments)]
pub fn build_editor_controls(
    mtm: MainThreadMarker,
    root: &NSView,
    win_w: f64,
    win_h: f64,
    input_path: &Path,
    initial_preview_sec: f64,
    prefs: &Preferences,
    initial_export_dir: Option<&Path>,
    default_stem: &str,
    has_audio: bool,
    source_nominal_fps: f32,
    demo_showcase: bool,
    tutorial_hint: bool,
) -> EditorControls {
    let image_view = build_preview_stage(mtm, root, win_w, win_h, input_path, initial_preview_sec);

    let divider = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        NSRect::new(NSPoint::new(20.0, 74.0), NSSize::new(win_w - 40.0, 1.0)),
    );
    divider.setBoxType(NSBoxType::Custom);
    divider.setBorderWidth(0.0);
    divider.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        1.0, 1.0, 1.0, 0.08,
    ));
    root.addSubview(&divider);

    let (format_popup, initial_fmt) = build_format_popup(mtm, root, prefs, demo_showcase);
    let (fps_popup, initial_fps) = build_fps_popup(mtm, root, initial_fmt, source_nominal_fps);
    let (size_popup, initial_max_width) = build_size_popup(mtm, root, prefs);

    let save_to_lbl = NSTextField::labelWithString(&NSString::from_str("Save to"), mtm);
    save_to_lbl.setFont(Some(&NSFont::systemFontOfSize_weight(12.0, 0.2)));
    save_to_lbl.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.74, 0.79, 0.89, 1.0,
    )));
    save_to_lbl.setAlignment(NSTextAlignment::Right);
    save_to_lbl.setFrame(NSRect::new(
        NSPoint::new(402.0, 134.0),
        NSSize::new(52.0, 20.0),
    ));
    root.addSubview(&save_to_lbl);

    let folder_popup = NSPopUpButton::new(mtm);
    folder_popup.setFrame(NSRect::new(
        NSPoint::new(460.0, 130.0),
        NSSize::new(280.0, 28.0),
    ));
    let folder_items = populate_folder_popup(mtm, &folder_popup, initial_export_dir, demo_showcase);
    root.addSubview(&folder_popup);

    let (name_field, ext_label, mute_checkbox) =
        build_name_row(mtm, root, default_stem, initial_fmt, has_audio);

    let ready_text = if tutorial_hint {
        "Tutorial (3/3) · Edit name or trim if you like, then click Save".to_string()
    } else {
        match initial_export_dir {
            Some(dir) => format!("Ready · Saves to {}", format_tilde_path(dir)),
            None => "Choose an export folder from Save to before saving".to_string(),
        }
    };
    let status_label = NSTextField::labelWithString(&NSString::from_str(&ready_text), mtm);
    status_label.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(
        12.0, 0.15,
    )));
    let status_color = if tutorial_hint {
        NSColor::colorWithSRGBRed_green_blue_alpha(0.58, 0.78, 1.0, 1.0)
    } else {
        NSColor::colorWithSRGBRed_green_blue_alpha(0.68, 0.74, 0.84, 1.0)
    };
    status_label.setTextColor(Some(&status_color));
    status_label.setFrame(NSRect::new(
        NSPoint::new(22.0, 36.0),
        NSSize::new(498.0, 20.0),
    ));
    root.addSubview(&status_label);

    let progress_view = ExportProgressView::new(
        mtm,
        NSRect::new(NSPoint::new(22.0, 18.0), NSSize::new(498.0, 10.0)),
    );
    if demo_showcase {
        progress_view.set_progress(680, ProgressVisualState::Running);
        status_label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            0.58, 0.78, 1.0, 1.0,
        )));
        status_label.setStringValue(&NSString::from_str("Exporting GIF...  68%"));
    }
    root.addSubview(&progress_view);

    let copy_btn = unsafe {
        NSButton::buttonWithTitle_target_action(&NSString::from_str("Copy"), None, None, mtm)
    };
    copy_btn.setFrame(NSRect::new(
        NSPoint::new(536.0, 20.0),
        NSSize::new(96.0, 34.0),
    ));
    root.addSubview(&copy_btn);

    let save_btn = unsafe {
        NSButton::buttonWithTitle_target_action(&NSString::from_str("Save"), None, None, mtm)
    };
    save_btn.setBezelColor(Some(&NSColor::systemBlueColor()));
    save_btn.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
    save_btn.setKeyEquivalent(&NSString::from_str("\r"));
    save_btn.setFrame(NSRect::new(
        NSPoint::new(640.0, 20.0),
        NSSize::new(100.0, 34.0),
    ));
    root.addSubview(&save_btn);

    EditorControls {
        image_view,
        format_popup,
        fps_popup,
        size_popup,
        folder_popup,
        folder_items,
        name_field,
        ext_label,
        mute_checkbox,
        status_label,
        progress_view,
        copy_btn,
        save_btn,
        initial_fmt,
        initial_fps,
        initial_max_width,
    }
}

fn build_format_popup(
    mtm: MainThreadMarker,
    root: &NSView,
    prefs: &Preferences,
    demo_showcase: bool,
) -> (Retained<NSPopUpButton>, ExportFormat) {
    let format_popup = NSPopUpButton::new(mtm);
    for fmt in ExportFormat::ALL {
        let title = if fmt.is_available() {
            fmt.label().to_string()
        } else {
            format!("{} (needs ffmpeg)", fmt.label())
        };
        format_popup.addItemWithTitle(&NSString::from_str(&title));
    }
    let initial_fmt = if demo_showcase {
        ExportFormat::Gif
    } else if prefs.default_export_format.is_available() {
        prefs.default_export_format
    } else {
        ExportFormat::Mp4
    };
    if let Some(idx) = ExportFormat::ALL.iter().position(|&f| f == initial_fmt) {
        format_popup.selectItemAtIndex(idx as isize);
    }
    format_popup.setFrame(NSRect::new(
        NSPoint::new(20.0, 130.0),
        NSSize::new(154.0, 28.0),
    ));
    root.addSubview(&format_popup);
    (format_popup, initial_fmt)
}

fn build_fps_popup(
    mtm: MainThreadMarker,
    root: &NSView,
    initial_fmt: ExportFormat,
    source_nominal_fps: f32,
) -> (Retained<NSPopUpButton>, u32) {
    let fps_popup = NSPopUpButton::new(mtm);
    for fps in FPS_PRESETS {
        fps_popup.addItemWithTitle(&NSString::from_str(&format!("{fps} fps")));
    }
    let is_anim = matches!(initial_fmt, ExportFormat::Gif | ExportFormat::Apng);
    let initial_fps = closest_editor_fps_preset(source_nominal_fps, is_anim);
    let fps_idx = FPS_PRESETS
        .iter()
        .position(|&f| f == initial_fps)
        .unwrap_or(FPS_PRESETS.len() - 1);
    fps_popup.selectItemAtIndex(fps_idx as isize);
    fps_popup.setFrame(NSRect::new(
        NSPoint::new(180.0, 130.0),
        NSSize::new(82.0, 28.0),
    ));
    root.addSubview(&fps_popup);
    (fps_popup, initial_fps)
}

fn build_size_popup(
    mtm: MainThreadMarker,
    root: &NSView,
    prefs: &Preferences,
) -> (Retained<NSPopUpButton>, Option<u32>) {
    let size_popup = NSPopUpButton::new(mtm);
    for (title, _) in SIZE_PRESETS {
        size_popup.addItemWithTitle(&NSString::from_str(title));
    }
    let size_idx = SIZE_PRESETS
        .iter()
        .position(|(_, w)| *w == prefs.export_max_width)
        .unwrap_or(0);
    size_popup.selectItemAtIndex(size_idx as isize);
    size_popup.setFrame(NSRect::new(
        NSPoint::new(268.0, 130.0),
        NSSize::new(128.0, 28.0),
    ));
    root.addSubview(&size_popup);
    (size_popup, SIZE_PRESETS[size_idx].1)
}

fn build_name_row(
    mtm: MainThreadMarker,
    root: &NSView,
    default_stem: &str,
    initial_fmt: ExportFormat,
    has_audio: bool,
) -> (
    Retained<NSTextField>,
    Retained<NSTextField>,
    Retained<NSButton>,
) {
    let name_lbl = NSTextField::labelWithString(&NSString::from_str("Name"), mtm);
    name_lbl.setFont(Some(&NSFont::systemFontOfSize_weight(12.0, 0.2)));
    name_lbl.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.74, 0.79, 0.89, 1.0,
    )));
    name_lbl.setFrame(NSRect::new(
        NSPoint::new(20.0, 92.0),
        NSSize::new(44.0, 20.0),
    ));
    root.addSubview(&name_lbl);

    let name_field = NSTextField::new(mtm);
    name_field.setEditable(true);
    name_field.setSelectable(true);
    name_field.setBezeled(true);
    name_field.setDrawsBackground(true);
    name_field.setUsesSingleLineMode(true);
    name_field.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(
        12.5, 0.0,
    )));
    name_field.setStringValue(&NSString::from_str(default_stem));
    name_field.setPlaceholderString(Some(&NSString::from_str(default_stem)));
    name_field.setToolTip(Some(&NSString::from_str(
        "Edit the output filename (extension is appended automatically)",
    )));
    name_field.setFrame(NSRect::new(
        NSPoint::new(68.0, 90.0),
        NSSize::new(498.0, 24.0),
    ));
    root.addSubview(&name_field);

    let ext_box = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        NSRect::new(NSPoint::new(572.0, 90.0), NSSize::new(52.0, 24.0)),
    );
    ext_box.setBoxType(NSBoxType::Custom);
    ext_box.setCornerRadius(6.0);
    ext_box.setBorderWidth(1.0);
    ext_box.setBorderColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        1.0, 1.0, 1.0, 0.12,
    ));
    ext_box.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.13, 0.15, 0.21, 1.0,
    ));
    root.addSubview(&ext_box);

    let ext_label = NSTextField::labelWithString(
        &NSString::from_str(&format!(".{}", initial_fmt.extension())),
        mtm,
    );
    ext_label.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(
        12.0, 0.25,
    )));
    ext_label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.80, 0.86, 0.96, 1.0,
    )));
    ext_label.setAlignment(NSTextAlignment::Center);
    ext_label.setFrame(NSRect::new(
        NSPoint::new(572.0, 92.0),
        NSSize::new(52.0, 19.0),
    ));
    root.addSubview(&ext_label);

    let mute_checkbox = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str("Mute audio"),
            None,
            None,
            mtm,
        )
    };
    mute_checkbox.setFont(Some(&NSFont::systemFontOfSize_weight(12.0, 0.15)));
    mute_checkbox.setFrame(NSRect::new(
        NSPoint::new(636.0, 91.0),
        NSSize::new(104.0, 22.0),
    ));
    let can_mute = has_audio && !matches!(initial_fmt, ExportFormat::Gif | ExportFormat::Apng);
    mute_checkbox.setEnabled(can_mute);
    mute_checkbox.setToolTip(Some(&NSString::from_str(
        "Remove audio tracks from the exported video",
    )));
    root.addSubview(&mute_checkbox);

    (name_field, ext_label, mute_checkbox)
}

fn build_preview_stage(
    mtm: MainThreadMarker,
    root: &NSView,
    win_w: f64,
    win_h: f64,
    input_path: &Path,
    initial_preview_sec: f64,
) -> Retained<NSImageView> {
    let full_bg = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(win_w, win_h)),
    );
    full_bg.setBoxType(NSBoxType::Custom);
    full_bg.setBorderWidth(0.0);
    full_bg.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.09, 0.10, 0.14, 1.0,
    ));
    root.addSubview(&full_bg);

    let stage_box = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        NSRect::new(NSPoint::new(20.0, 246.0), NSSize::new(win_w - 40.0, 346.0)),
    );
    stage_box.setBoxType(NSBoxType::Custom);
    stage_box.setCornerRadius(10.0);
    stage_box.setBorderWidth(1.0);
    stage_box.setBorderColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        1.0, 1.0, 1.0, 0.12,
    ));
    stage_box.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.04, 0.05, 0.07, 1.0,
    ));
    root.addSubview(&stage_box);

    let image_view = NSImageView::initWithFrame(
        NSImageView::alloc(mtm),
        NSRect::new(NSPoint::new(28.0, 254.0), NSSize::new(win_w - 56.0, 330.0)),
    );
    image_view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
    root.addSubview(&image_view);

    if let Ok(frame) = extract_rgba_frame(input_path, initial_preview_sec, Some(960)) {
        if let Some(img) = rgba_to_nsimage(&frame) {
            image_view.setImage(Some(&img));
        }
    }
    image_view
}
