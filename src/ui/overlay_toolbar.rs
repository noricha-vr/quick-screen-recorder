use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBox, NSBoxType, NSButton, NSColor, NSControlStateValueOff, NSControlStateValueOn, NSFont,
    NSMenuItem, NSPopUpButton, NSSegmentSwitchTracking, NSSegmentedControl, NSTextAlignment,
    NSTextField, NSView,
};
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use crate::capture::audio_devices::MicMenuItem;
use crate::capture::cadence::CadenceMenuItem;
use crate::config::Preferences;
pub use crate::ui::overlay_window::append_tagged_popup_item;

pub const TOOLBAR_W: f64 = 628.0;
pub const TOOLBAR_H: f64 = 88.0;

pub struct ToolbarActionIvars {
    pub on_mode: Box<dyn Fn(isize)>,
    pub on_aspect: Box<dyn Fn(isize)>,
    pub on_size_commit: Box<dyn Fn()>,
    pub on_target_pick: Box<dyn Fn(isize)>,
    pub on_fps: Box<dyn Fn(isize)>,
    pub on_sys_audio: Box<dyn Fn(bool)>,
    pub on_mic: Box<dyn Fn(isize)>,
    pub on_cursor: Box<dyn Fn(bool)>,
    pub on_clicks: Box<dyn Fn(bool)>,
    pub on_record: Box<dyn Fn()>,
    pub on_cancel: Box<dyn Fn()>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "QSROverlayToolbarActions"]
    #[ivars = ToolbarActionIvars]
    pub struct OverlayToolbarActions;

    unsafe impl NSObjectProtocol for OverlayToolbarActions {}

    impl OverlayToolbarActions {
        #[unsafe(method(modeChanged:))]
        fn mode_changed(&self, sender: &NSSegmentedControl) {
            (self.ivars().on_mode)(sender.selectedSegment());
        }
        #[unsafe(method(aspectChanged:))]
        fn aspect_changed(&self, sender: &NSPopUpButton) {
            (self.ivars().on_aspect)(sender.indexOfSelectedItem());
        }
        #[unsafe(method(sizeCommitted:))]
        fn size_committed(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_size_commit)();
        }
        #[unsafe(method(targetPicked:))]
        fn target_picked(&self, sender: &NSPopUpButton) {
            let tag = sender
                .selectedItem()
                .map(|mi| mi.tag())
                .unwrap_or_else(|| sender.indexOfSelectedItem());
            (self.ivars().on_target_pick)(tag);
        }
        #[unsafe(method(fpsChanged:))]
        fn fps_changed(&self, sender: &NSPopUpButton) {
            let tag = sender
                .selectedItem()
                .map(|mi| mi.tag())
                .unwrap_or_else(|| sender.indexOfSelectedItem());
            (self.ivars().on_fps)(tag);
        }
        #[unsafe(method(sysAudioToggled:))]
        fn sys_audio_toggled(&self, sender: &NSButton) {
            (self.ivars().on_sys_audio)(sender.state() == NSControlStateValueOn);
        }
        #[unsafe(method(micChanged:))]
        fn mic_changed(&self, sender: &NSPopUpButton) {
            let tag = sender
                .selectedItem()
                .map(|mi| mi.tag())
                .unwrap_or_else(|| sender.indexOfSelectedItem());
            (self.ivars().on_mic)(tag);
        }
        #[unsafe(method(cursorToggled:))]
        fn cursor_toggled(&self, sender: &NSButton) {
            (self.ivars().on_cursor)(sender.state() == NSControlStateValueOn);
        }
        #[unsafe(method(clicksToggled:))]
        fn clicks_toggled(&self, sender: &NSButton) {
            (self.ivars().on_clicks)(sender.state() == NSControlStateValueOn);
        }
        #[unsafe(method(startClicked:))]
        fn start_clicked(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_record)();
        }
        #[unsafe(method(cancelClicked:))]
        fn cancel_clicked(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_cancel)();
        }
    }
);

impl OverlayToolbarActions {
    pub fn new(mtm: MainThreadMarker, ivars: ToolbarActionIvars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn populate_toolbar_controls(
    mtm: MainThreadMarker,
    bar: &NSView,
    actions: &OverlayToolbarActions,
    aspect_popup: &NSPopUpButton,
    w_field: &NSTextField,
    x_lbl: &NSTextField,
    h_field: &NSTextField,
    target_popup: &NSPopUpButton,
    sys_audio_chk: &NSButton,
    mic_popup: &NSPopUpButton,
    mic_items: &[MicMenuItem],
    selected_mic_idx: usize,
    cadence_items: &[CadenceMenuItem],
    selected_cadence_idx: usize,
    prefs: &Preferences,
) {
    build_elevated_shell(mtm, bar);
    build_row_one(
        mtm,
        bar,
        actions,
        aspect_popup,
        w_field,
        x_lbl,
        h_field,
        target_popup,
    );
    build_row_two(
        mtm,
        bar,
        actions,
        sys_audio_chk,
        mic_popup,
        mic_items,
        selected_mic_idx,
        cadence_items,
        selected_cadence_idx,
        prefs,
    );
}

fn build_elevated_shell(mtm: MainThreadMarker, bar: &NSView) {
    let bounds = bar.bounds();
    // Outer dark hairline keeps contrast sharp against bright/white screens.
    let outer_box = NSBox::initWithFrame(NSBox::alloc(mtm), bounds);
    outer_box.setBoxType(NSBoxType::Custom);
    outer_box.setCornerRadius(14.0);
    outer_box.setBorderWidth(1.0);
    outer_box.setBorderColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.0, 0.0, 0.0, 0.80,
    ));
    outer_box.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.17, 0.20, 0.27, 0.97,
    ));
    bar.addSubview(&outer_box);

    // Bright inner rim gives crisp separation against black/dark backgrounds.
    let inner_rect = NSRect::new(
        NSPoint::new(1.0, 1.0),
        NSSize::new(
            (bounds.size.width - 2.0).max(2.0),
            (bounds.size.height - 2.0).max(2.0),
        ),
    );
    let inner_rim = NSBox::initWithFrame(NSBox::alloc(mtm), inner_rect);
    inner_rim.setBoxType(NSBoxType::Custom);
    inner_rim.setCornerRadius(13.0);
    inner_rim.setBorderWidth(1.25);
    inner_rim.setBorderColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        1.0, 1.0, 1.0, 0.42,
    ));
    inner_rim.setFillColor(&NSColor::clearColor());
    bar.addSubview(&inner_rim);

    // Row divider separating capture target/action row from audio/settings row.
    let row_div = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        NSRect::new(
            NSPoint::new(12.0, 43.0),
            NSSize::new(bounds.size.width - 24.0, 1.0),
        ),
    );
    row_div.setBoxType(NSBoxType::Custom);
    row_div.setBorderWidth(0.0);
    row_div.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        1.0, 1.0, 1.0, 0.18,
    ));
    bar.addSubview(&row_div);
}

#[allow(clippy::too_many_arguments)]
fn build_row_one(
    mtm: MainThreadMarker,
    bar: &NSView,
    actions: &OverlayToolbarActions,
    aspect_popup: &NSPopUpButton,
    w_field: &NSTextField,
    x_lbl: &NSTextField,
    h_field: &NSTextField,
    target_popup: &NSPopUpButton,
) {
    let labels = NSArray::from_retained_slice(&[
        NSString::from_str("Area"),
        NSString::from_str("Window"),
        NSString::from_str("Screen"),
    ]);
    let seg = unsafe {
        NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
            &labels,
            NSSegmentSwitchTracking::SelectOne,
            Some(actions),
            Some(sel!(modeChanged:)),
            mtm,
        )
    };
    seg.setSelectedSegment(0);
    seg.setFrame(NSRect::new(
        NSPoint::new(12.0, 49.0),
        NSSize::new(178.0, 28.0),
    ));
    bar.addSubview(&seg);

    aspect_popup.setFrame(NSRect::new(
        NSPoint::new(198.0, 49.0),
        NSSize::new(80.0, 28.0),
    ));
    unsafe {
        aspect_popup.setTarget(Some(actions));
        aspect_popup.setAction(Some(sel!(aspectChanged:)));
    }
    bar.addSubview(aspect_popup);

    w_field.setFrame(NSRect::new(
        NSPoint::new(284.0, 51.0),
        NSSize::new(52.0, 24.0),
    ));
    unsafe {
        w_field.setTarget(Some(actions));
        w_field.setAction(Some(sel!(sizeCommitted:)));
    }
    bar.addSubview(w_field);

    x_lbl.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.78, 0.84, 0.94, 1.0,
    )));
    x_lbl.setAlignment(NSTextAlignment::Center);
    x_lbl.setFrame(NSRect::new(
        NSPoint::new(338.0, 52.0),
        NSSize::new(14.0, 20.0),
    ));
    bar.addSubview(x_lbl);

    h_field.setFrame(NSRect::new(
        NSPoint::new(352.0, 51.0),
        NSSize::new(52.0, 24.0),
    ));
    unsafe {
        h_field.setTarget(Some(actions));
        h_field.setAction(Some(sel!(sizeCommitted:)));
    }
    bar.addSubview(h_field);

    target_popup.setFrame(NSRect::new(
        NSPoint::new(198.0, 49.0),
        NSSize::new(236.0, 28.0),
    ));
    unsafe {
        target_popup.setTarget(Some(actions));
        target_popup.setAction(Some(sel!(targetPicked:)));
    }
    bar.addSubview(target_popup);

    let cancel_btn = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Cancel"),
            Some(actions),
            Some(sel!(cancelClicked:)),
            mtm,
        )
    };
    cancel_btn.setKeyEquivalent(&NSString::from_str("\u{1b}"));
    cancel_btn.setFrame(NSRect::new(
        NSPoint::new(458.0, 49.0),
        NSSize::new(66.0, 28.0),
    ));
    bar.addSubview(&cancel_btn);

    let rec_btn = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("● Record"),
            Some(actions),
            Some(sel!(startClicked:)),
            mtm,
        )
    };
    rec_btn.setBezelColor(Some(&NSColor::systemRedColor()));
    rec_btn.setFont(Some(&NSFont::boldSystemFontOfSize(12.5)));
    rec_btn.setKeyEquivalent(&NSString::from_str("\r"));
    rec_btn.setFrame(NSRect::new(
        NSPoint::new(530.0, 49.0),
        NSSize::new(86.0, 28.0),
    ));
    bar.addSubview(&rec_btn);
}

#[allow(clippy::too_many_arguments)]
fn build_row_two(
    mtm: MainThreadMarker,
    bar: &NSView,
    actions: &OverlayToolbarActions,
    sys_audio_chk: &NSButton,
    mic_popup: &NSPopUpButton,
    mic_items: &[MicMenuItem],
    selected_mic_idx: usize,
    cadence_items: &[CadenceMenuItem],
    selected_cadence_idx: usize,
    prefs: &Preferences,
) {
    let audio_allowed = !prefs.capture_cadence.is_timelapse();
    sys_audio_chk.setFont(Some(&NSFont::systemFontOfSize_weight(12.0, 0.15)));
    sys_audio_chk.setState(if prefs.capture_system_audio {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    sys_audio_chk.setEnabled(audio_allowed);
    sys_audio_chk.setToolTip(Some(&NSString::from_str(
        "Record macOS system audio output (automatically disabled in timelapse mode)",
    )));
    sys_audio_chk.setFrame(NSRect::new(
        NSPoint::new(14.0, 14.0),
        NSSize::new(110.0, 20.0),
    ));
    unsafe {
        sys_audio_chk.setTarget(Some(actions));
        sys_audio_chk.setAction(Some(sel!(sysAudioToggled:)));
    }
    bar.addSubview(sys_audio_chk);

    add_v_sep(mtm, bar, 130.0);

    let mic_lbl = NSTextField::labelWithString(&NSString::from_str("Mic"), mtm);
    mic_lbl.setFont(Some(&NSFont::systemFontOfSize_weight(11.5, 0.2)));
    mic_lbl.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.76, 0.82, 0.92, 1.0,
    )));
    mic_lbl.setFrame(NSRect::new(
        NSPoint::new(138.0, 14.0),
        NSSize::new(26.0, 18.0),
    ));
    bar.addSubview(&mic_lbl);

    for (idx, item) in mic_items.iter().enumerate() {
        append_tagged_popup_item(mic_popup, &item.menu_title(), idx as isize);
    }
    if !mic_items.is_empty() {
        mic_popup.selectItemAtIndex(selected_mic_idx.min(mic_items.len() - 1) as isize);
    }
    mic_popup.setEnabled(audio_allowed);
    mic_popup.setToolTip(Some(&NSString::from_str(
        "Select microphone input device (automatically disabled in timelapse mode)",
    )));
    mic_popup.setFrame(NSRect::new(
        NSPoint::new(166.0, 10.0),
        NSSize::new(174.0, 28.0),
    ));
    unsafe {
        mic_popup.setTarget(Some(actions));
        mic_popup.setAction(Some(sel!(micChanged:)));
    }
    bar.addSubview(mic_popup);

    add_v_sep(mtm, bar, 348.0);

    let fps_popup = NSPopUpButton::new(mtm);
    for (idx, item) in cadence_items.iter().enumerate() {
        match item {
            CadenceMenuItem::Option { label, .. } => {
                append_tagged_popup_item(&fps_popup, label, idx as isize);
            }
            CadenceMenuItem::Separator => {
                if let Some(menu) = fps_popup.menu() {
                    menu.addItem(&NSMenuItem::separatorItem(mtm));
                }
            }
        }
    }
    if !cadence_items.is_empty() {
        fps_popup.selectItemAtIndex(selected_cadence_idx.min(cadence_items.len() - 1) as isize);
    }
    fps_popup.setToolTip(Some(&NSString::from_str(
        "Frame rate: 1–60 fps real-time, or interval timelapse (30 fps playback, audio muted)",
    )));
    fps_popup.setFrame(NSRect::new(
        NSPoint::new(356.0, 10.0),
        NSSize::new(108.0, 28.0),
    ));
    unsafe {
        fps_popup.setTarget(Some(actions));
        fps_popup.setAction(Some(sel!(fpsChanged:)));
    }
    bar.addSubview(&fps_popup);

    let cursor_chk = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str("Cursor"),
            Some(actions),
            Some(sel!(cursorToggled:)),
            mtm,
        )
    };
    cursor_chk.setFont(Some(&NSFont::systemFontOfSize_weight(12.0, 0.1)));
    cursor_chk.setState(if prefs.show_cursor {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    cursor_chk.setFrame(NSRect::new(
        NSPoint::new(474.0, 14.0),
        NSSize::new(66.0, 20.0),
    ));
    bar.addSubview(&cursor_chk);

    let clicks_chk = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str("Clicks"),
            Some(actions),
            Some(sel!(clicksToggled:)),
            mtm,
        )
    };
    clicks_chk.setFont(Some(&NSFont::systemFontOfSize_weight(12.0, 0.1)));
    clicks_chk.setState(if prefs.highlight_clicks {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    clicks_chk.setFrame(NSRect::new(
        NSPoint::new(548.0, 14.0),
        NSSize::new(68.0, 20.0),
    ));
    bar.addSubview(&clicks_chk);
}

fn add_v_sep(mtm: MainThreadMarker, bar: &NSView, x: f64) {
    let sep = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        NSRect::new(NSPoint::new(x, 15.0), NSSize::new(1.0, 18.0)),
    );
    sep.setBoxType(NSBoxType::Custom);
    sep.setBorderWidth(0.0);
    sep.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        1.0, 1.0, 1.0, 0.16,
    ));
    bar.addSubview(&sep);
}
