use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString};

use crate::capture::RecordingCodec;

pub struct StatusBarCallbacks {
    pub on_primary_click: Box<dyn Fn(MainThreadMarker)>,
    pub on_open_video: Box<dyn Fn(MainThreadMarker)>,
    pub on_toggle_codec: Box<dyn Fn(MainThreadMarker)>,
    pub on_quit: Box<dyn Fn(MainThreadMarker)>,
}

struct HandlerIvars {
    callbacks: StatusBarCallbacks,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "QSRStatusBarHandler"]
    #[ivars = HandlerIvars]
    struct StatusBarHandler;

    unsafe impl NSObjectProtocol for StatusBarHandler {}

    impl StatusBarHandler {
        #[unsafe(method(onPrimaryAction:))]
        fn on_primary_action(&self, _sender: Option<&AnyObject>) {
            let mtm = self.mtm();
            (self.ivars().callbacks.on_primary_click)(mtm);
        }

        #[unsafe(method(onOpenVideoAction:))]
        fn on_open_video_action(&self, _sender: Option<&AnyObject>) {
            let mtm = self.mtm();
            (self.ivars().callbacks.on_open_video)(mtm);
        }

        #[unsafe(method(onToggleCodecAction:))]
        fn on_toggle_codec_action(&self, _sender: Option<&AnyObject>) {
            let mtm = self.mtm();
            (self.ivars().callbacks.on_toggle_codec)(mtm);
        }

        #[unsafe(method(onQuitAction:))]
        fn on_quit_action(&self, _sender: Option<&AnyObject>) {
            let mtm = self.mtm();
            (self.ivars().callbacks.on_quit)(mtm);
        }
    }
);

impl StatusBarHandler {
    fn new(mtm: MainThreadMarker, callbacks: StatusBarCallbacks) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(HandlerIvars { callbacks });
        unsafe { msg_send![super(this), init] }
    }
}

pub struct StatusBarController {
    status_item: Retained<NSStatusItem>,
    record_item: Retained<NSMenuItem>,
    codec_item: Retained<NSMenuItem>,
    _handler: Retained<StatusBarHandler>,
}

impl StatusBarController {
    pub fn new(
        mtm: MainThreadMarker,
        codec: RecordingCodec,
        callbacks: StatusBarCallbacks,
    ) -> Self {
        let handler = StatusBarHandler::new(mtm, callbacks);
        let bar = NSStatusBar::systemStatusBar();
        let status_item = bar.statusItemWithLength(NSVariableStatusItemLength);

        if let Some(btn) = status_item.button(mtm) {
            btn.setTitle(&NSString::from_str("◉ Rec"));
        }

        let menu = NSMenu::new(mtm);

        let record_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("New Recording..."),
                Some(sel!(onPrimaryAction:)),
                &NSString::from_str("6"),
            )
        };
        unsafe {
            record_item.setTarget(Some(&handler));
        }
        menu.addItem(&record_item);

        let open_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Open Video in Editor..."),
                Some(sel!(onOpenVideoAction:)),
                &NSString::from_str("o"),
            )
        };
        unsafe {
            open_item.setTarget(Some(&handler));
        }
        menu.addItem(&open_item);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let codec_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&format!("Capture Codec: {}", codec.label())),
                Some(sel!(onToggleCodecAction:)),
                &NSString::from_str(""),
            )
        };
        unsafe {
            codec_item.setTarget(Some(&handler));
        }
        menu.addItem(&codec_item);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let quit_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Quit Quick Screen Recorder"),
                Some(sel!(onQuitAction:)),
                &NSString::from_str("q"),
            )
        };
        unsafe {
            quit_item.setTarget(Some(&handler));
        }
        menu.addItem(&quit_item);

        status_item.setMenu(Some(&menu));

        Self {
            status_item,
            record_item,
            codec_item,
            _handler: handler,
        }
    }

    pub fn set_idle(&self, mtm: MainThreadMarker) {
        if let Some(btn) = self.status_item.button(mtm) {
            btn.setTitle(&NSString::from_str("◉ Rec"));
        }
        self.record_item
            .setTitle(&NSString::from_str("New Recording..."));
    }

    pub fn set_recording_elapsed(&self, mtm: MainThreadMarker, secs: u64) {
        let mins = secs / 60;
        let rem = secs % 60;
        if let Some(btn) = self.status_item.button(mtm) {
            btn.setTitle(&NSString::from_str(&format!("● {mins:02}:{rem:02}")));
        }
        self.record_item
            .setTitle(&NSString::from_str("Stop Recording"));
    }

    pub fn set_codec_label(&self, codec: RecordingCodec) {
        self.codec_item.setTitle(&NSString::from_str(&format!(
            "Capture Codec: {}",
            codec.label()
        )));
    }
}
