use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBox, NSBoxType, NSButton, NSColor, NSControlStateValueOff, NSControlStateValueOn, NSFont,
    NSPopUpButton, NSSegmentSwitchTracking, NSSegmentedControl, NSTextField, NSView,
};
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use crate::capture::AudioCaptureMode;
use crate::config::Preferences;

pub struct ToolbarActionIvars {
    pub on_mode: Box<dyn Fn(isize)>,
    pub on_aspect: Box<dyn Fn(isize)>,
    pub on_size_commit: Box<dyn Fn()>,
    pub on_target_pick: Box<dyn Fn(isize)>,
    pub on_fps: Box<dyn Fn(isize)>,
    pub on_audio: Box<dyn Fn(isize)>,
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
            (self.ivars().on_target_pick)(sender.indexOfSelectedItem());
        }
        #[unsafe(method(fpsChanged:))]
        fn fps_changed(&self, sender: &NSPopUpButton) {
            (self.ivars().on_fps)(sender.indexOfSelectedItem());
        }
        #[unsafe(method(audioChanged:))]
        fn audio_changed(&self, sender: &NSPopUpButton) {
            (self.ivars().on_audio)(sender.indexOfSelectedItem());
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
    h_field: &NSTextField,
    target_popup: &NSPopUpButton,
    prefs: &Preferences,
) {
    let bg_box = NSBox::initWithFrame(NSBox::alloc(mtm), bar.bounds());
    bg_box.setBoxType(NSBoxType::Custom);
    bg_box.setCornerRadius(12.0);
    bg_box.setBorderWidth(1.0);
    bg_box.setBorderColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        1.0, 1.0, 1.0, 0.16,
    ));
    bg_box.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.09, 0.10, 0.14, 0.94,
    ));
    bar.addSubview(&bg_box);

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
        NSPoint::new(12.0, 12.0),
        NSSize::new(178.0, 28.0),
    ));
    bar.addSubview(&seg);

    aspect_popup.setFrame(NSRect::new(
        NSPoint::new(198.0, 12.0),
        NSSize::new(82.0, 28.0),
    ));
    unsafe {
        aspect_popup.setTarget(Some(actions));
        aspect_popup.setAction(Some(sel!(aspectChanged:)));
    }
    bar.addSubview(aspect_popup);

    w_field.setFrame(NSRect::new(
        NSPoint::new(286.0, 14.0),
        NSSize::new(54.0, 24.0),
    ));
    unsafe {
        w_field.setTarget(Some(actions));
        w_field.setAction(Some(sel!(sizeCommitted:)));
    }
    bar.addSubview(w_field);

    let x_lbl = NSTextField::labelWithString(&NSString::from_str("×"), mtm);
    x_lbl.setTextColor(Some(&NSColor::lightGrayColor()));
    x_lbl.setFrame(NSRect::new(
        NSPoint::new(342.0, 15.0),
        NSSize::new(14.0, 20.0),
    ));
    bar.addSubview(&x_lbl);

    h_field.setFrame(NSRect::new(
        NSPoint::new(356.0, 14.0),
        NSSize::new(54.0, 24.0),
    ));
    unsafe {
        h_field.setTarget(Some(actions));
        h_field.setAction(Some(sel!(sizeCommitted:)));
    }
    bar.addSubview(h_field);

    target_popup.setFrame(NSRect::new(
        NSPoint::new(198.0, 12.0),
        NSSize::new(212.0, 28.0),
    ));
    unsafe {
        target_popup.setTarget(Some(actions));
        target_popup.setAction(Some(sel!(targetPicked:)));
    }
    bar.addSubview(target_popup);

    let fps_popup = NSPopUpButton::new(mtm);
    fps_popup.addItemWithTitle(&NSString::from_str("30 fps"));
    fps_popup.addItemWithTitle(&NSString::from_str("60 fps"));
    fps_popup.selectItemAtIndex(if prefs.fps >= 60 { 1 } else { 0 });
    fps_popup.setFrame(NSRect::new(
        NSPoint::new(418.0, 12.0),
        NSSize::new(76.0, 28.0),
    ));
    unsafe {
        fps_popup.setTarget(Some(actions));
        fps_popup.setAction(Some(sel!(fpsChanged:)));
    }
    bar.addSubview(&fps_popup);

    let audio_popup = NSPopUpButton::new(mtm);
    for m in AudioCaptureMode::ALL {
        audio_popup.addItemWithTitle(&NSString::from_str(m.label()));
    }
    audio_popup.selectItemAtIndex(prefs.audio_mode.index() as isize);
    audio_popup.setFrame(NSRect::new(
        NSPoint::new(500.0, 12.0),
        NSSize::new(112.0, 28.0),
    ));
    unsafe {
        audio_popup.setTarget(Some(actions));
        audio_popup.setAction(Some(sel!(audioChanged:)));
    }
    bar.addSubview(&audio_popup);

    let cursor_chk = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str("Cursor"),
            Some(actions),
            Some(sel!(cursorToggled:)),
            mtm,
        )
    };
    cursor_chk.setState(if prefs.show_cursor {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    cursor_chk.setFrame(NSRect::new(
        NSPoint::new(618.0, 26.0),
        NSSize::new(68.0, 18.0),
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
    clicks_chk.setState(if prefs.highlight_clicks {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    clicks_chk.setFrame(NSRect::new(
        NSPoint::new(618.0, 8.0),
        NSSize::new(68.0, 18.0),
    ));
    bar.addSubview(&clicks_chk);

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
        NSPoint::new(690.0, 12.0),
        NSSize::new(64.0, 28.0),
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
        NSPoint::new(756.0, 12.0),
        NSSize::new(76.0, 28.0),
    ));
    bar.addSubview(&rec_btn);
}
