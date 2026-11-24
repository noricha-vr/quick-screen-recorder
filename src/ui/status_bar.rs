use std::cell::RefCell;
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSControlStateValueOff, NSControlStateValueOn, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString};

use crate::capture::RecordingCodec;
use crate::fs_safety::is_usable_export_dir;
use crate::ui::folder_picker::{FolderMenuItem, compact_tilde_folder, live_folder_menu_model};

pub struct StatusBarCallbacks {
    pub on_primary_click: Box<dyn Fn(MainThreadMarker)>,
    pub on_open_video: Box<dyn Fn(MainThreadMarker)>,
    pub on_select_export_dir: Box<dyn Fn(MainThreadMarker, PathBuf)>,
    pub on_choose_export_folder: Box<dyn Fn(MainThreadMarker)>,
    pub on_toggle_codec: Box<dyn Fn(MainThreadMarker)>,
    pub on_open_guide: Box<dyn Fn(MainThreadMarker)>,
    pub on_quit: Box<dyn Fn(MainThreadMarker)>,
}

struct HandlerIvars {
    callbacks: StatusBarCallbacks,
    folder_targets: RefCell<Vec<PathBuf>>,
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
            (self.ivars().callbacks.on_primary_click)(self.mtm());
        }

        #[unsafe(method(onOpenVideoAction:))]
        fn on_open_video_action(&self, _sender: Option<&AnyObject>) {
            (self.ivars().callbacks.on_open_video)(self.mtm());
        }

        #[unsafe(method(onSelectFolderItem:))]
        fn on_select_folder_item(&self, sender: &NSMenuItem) {
            let idx = sender.tag();
            if idx < 0 {
                return;
            }
            let target = self.ivars().folder_targets.borrow().get(idx as usize).cloned();
            if let Some(dir) = target {
                (self.ivars().callbacks.on_select_export_dir)(self.mtm(), dir);
            }
        }

        #[unsafe(method(onChooseFolderAction:))]
        fn on_choose_folder_action(&self, _sender: Option<&AnyObject>) {
            (self.ivars().callbacks.on_choose_export_folder)(self.mtm());
        }

        #[unsafe(method(onToggleCodecAction:))]
        fn on_toggle_codec_action(&self, _sender: Option<&AnyObject>) {
            (self.ivars().callbacks.on_toggle_codec)(self.mtm());
        }

        #[unsafe(method(onOpenGuideAction:))]
        fn on_open_guide_action(&self, _sender: Option<&AnyObject>) {
            (self.ivars().callbacks.on_open_guide)(self.mtm());
        }

        #[unsafe(method(onQuitAction:))]
        fn on_quit_action(&self, _sender: Option<&AnyObject>) {
            (self.ivars().callbacks.on_quit)(self.mtm());
        }
    }
);

impl StatusBarHandler {
    fn new(mtm: MainThreadMarker, callbacks: StatusBarCallbacks) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(HandlerIvars {
            callbacks,
            folder_targets: RefCell::new(Vec::new()),
        });
        unsafe { msg_send![super(this), init] }
    }
}

pub struct StatusBarController {
    status_item: Retained<NSStatusItem>,
    record_item: Retained<NSMenuItem>,
    folder_item: Retained<NSMenuItem>,
    folder_submenu: Retained<NSMenu>,
    codec_item: Retained<NSMenuItem>,
    handler: Retained<StatusBarHandler>,
}

impl StatusBarController {
    pub fn new(
        mtm: MainThreadMarker,
        codec: RecordingCodec,
        initial_export_dir: Option<&Path>,
        callbacks: StatusBarCallbacks,
    ) -> Self {
        let handler = StatusBarHandler::new(mtm, callbacks);
        let bar = NSStatusBar::systemStatusBar();
        let status_item = bar.statusItemWithLength(NSVariableStatusItemLength);

        if let Some(btn) = status_item.button(mtm) {
            btn.setTitle(&NSString::from_str("◉ Rec"));
        }

        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);

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

        let folder_submenu = NSMenu::new(mtm);
        folder_submenu.setAutoenablesItems(false);
        let folder_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Save Folder"),
                None,
                &NSString::from_str(""),
            )
        };
        folder_item.setSubmenu(Some(&folder_submenu));
        menu.addItem(&folder_item);

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

        let guide_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Quick Start Guide..."),
                Some(sel!(onOpenGuideAction:)),
                &NSString::from_str(""),
            )
        };
        unsafe {
            guide_item.setTarget(Some(&handler));
        }
        menu.addItem(&guide_item);

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

        let controller = Self {
            status_item,
            record_item,
            folder_item,
            folder_submenu,
            codec_item,
            handler,
        };
        controller.set_export_dir(mtm, initial_export_dir);
        controller
    }

    pub fn set_idle(&self, mtm: MainThreadMarker) {
        if let Some(btn) = self.status_item.button(mtm) {
            btn.setTitle(&NSString::from_str("◉ Rec"));
        }
        self.record_item
            .setTitle(&NSString::from_str("New Recording..."));
        self.record_item.setEnabled(true);
    }

    pub fn set_recording_elapsed(&self, mtm: MainThreadMarker, secs: u64) {
        let mins = secs / 60;
        let rem = secs % 60;
        if let Some(btn) = self.status_item.button(mtm) {
            btn.setTitle(&NSString::from_str(&format!("● {mins:02}:{rem:02}")));
        }
        self.record_item
            .setTitle(&NSString::from_str("Stop Recording"));
        self.record_item.setEnabled(true);
    }

    pub fn set_stopping(&self, mtm: MainThreadMarker) {
        if let Some(btn) = self.status_item.button(mtm) {
            btn.setTitle(&NSString::from_str("… Saving"));
        }
        self.record_item
            .setTitle(&NSString::from_str("Saving Recording..."));
        self.record_item.setEnabled(false);
    }

    pub fn set_codec_label(&self, codec: RecordingCodec) {
        self.codec_item.setTitle(&NSString::from_str(&format!(
            "Capture Codec: {}",
            codec.label()
        )));
    }

    pub fn set_export_dir(&self, mtm: MainThreadMarker, current: Option<&Path>) {
        let header = match current.filter(|d| is_usable_export_dir(d)) {
            Some(dir) => format!("Save Folder: {}", compact_tilde_folder(dir, 32)),
            None => "Save Folder: Choose...".to_string(),
        };
        self.folder_item.setTitle(&NSString::from_str(&header));

        self.folder_submenu.removeAllItems();
        let (items, _) = live_folder_menu_model(current, false, 34);
        let mut targets = Vec::new();

        for item in items {
            match item {
                FolderMenuItem::Dir {
                    path,
                    label,
                    selected,
                } => {
                    let tag = targets.len() as isize;
                    targets.push(path);
                    let mi = unsafe {
                        NSMenuItem::initWithTitle_action_keyEquivalent(
                            NSMenuItem::alloc(mtm),
                            &NSString::from_str(&label),
                            Some(sel!(onSelectFolderItem:)),
                            &NSString::from_str(""),
                        )
                    };
                    unsafe {
                        mi.setTarget(Some(&self.handler));
                    }
                    mi.setTag(tag);
                    mi.setState(if selected {
                        NSControlStateValueOn
                    } else {
                        NSControlStateValueOff
                    });
                    mi.setEnabled(true);
                    self.folder_submenu.addItem(&mi);
                }
                FolderMenuItem::Placeholder => {}
                FolderMenuItem::Separator => {
                    self.folder_submenu.addItem(&NSMenuItem::separatorItem(mtm));
                }
                FolderMenuItem::ChooseCustom => {
                    let mi = unsafe {
                        NSMenuItem::initWithTitle_action_keyEquivalent(
                            NSMenuItem::alloc(mtm),
                            &NSString::from_str("Choose Folder..."),
                            Some(sel!(onChooseFolderAction:)),
                            &NSString::from_str(""),
                        )
                    };
                    unsafe {
                        mi.setTarget(Some(&self.handler));
                    }
                    mi.setEnabled(true);
                    self.folder_submenu.addItem(&mi);
                }
            }
        }

        *self.handler.ivars().folder_targets.borrow_mut() = targets;
    }
}
