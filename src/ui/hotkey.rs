use std::ffi::c_void;
use std::ptr;

use objc2::MainThreadMarker;

type OSStatus = i32;
type EventTargetRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHotKeyRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
const CMD_KEY_MASK: u32 = 1 << 8;
const SHIFT_KEY_MASK: u32 = 1 << 9;
const KEYCODE_6: u32 = 22;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        in_target: EventTargetRef,
        in_handler: unsafe extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus,
        in_num_types: usize,
        in_list: *const EventTypeSpec,
        in_user_data: *mut c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OSStatus;
    fn RegisterEventHotKey(
        in_hot_key_code: u32,
        in_hot_key_modifiers: u32,
        in_hot_key_id: EventHotKeyID,
        in_target: EventTargetRef,
        in_options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OSStatus;
}

unsafe extern "C" fn hotkey_callback(
    _next_handler: EventHandlerCallRef,
    _event: EventRef,
    user_data: *mut c_void,
) -> OSStatus {
    if !user_data.is_null() {
        let cb = unsafe { &*(user_data as *const Box<dyn Fn(MainThreadMarker)>) };
        if let Some(mtm) = MainThreadMarker::new() {
            cb(mtm);
        }
    }
    0
}

/// Registers global `Cmd+Shift+6` via Carbon `RegisterEventHotKey`, which requires ZERO
/// Input Monitoring or Accessibility TCC permissions on macOS.
pub fn register_global_cmd_shift_6<F>(on_trigger: F)
where
    F: Fn(MainThreadMarker) + 'static,
{
    let boxed: Box<Box<dyn Fn(MainThreadMarker)>> = Box::new(Box::new(on_trigger));
    let user_ptr = Box::into_raw(boxed) as *mut c_void;

    let spec = EventTypeSpec {
        event_class: K_EVENT_CLASS_KEYBOARD,
        event_kind: K_EVENT_HOT_KEY_PRESSED,
    };
    let hk_id = EventHotKeyID {
        signature: u32::from_be_bytes(*b"QSR1"),
        id: 1,
    };

    unsafe {
        let target = GetApplicationEventTarget();
        if target.is_null() {
            return;
        }
        let mut handler_ref: EventHandlerRef = ptr::null_mut();
        let status = InstallEventHandler(
            target,
            hotkey_callback,
            1,
            &spec,
            user_ptr,
            &mut handler_ref,
        );
        if status == 0 {
            let mut hk_ref: EventHotKeyRef = ptr::null_mut();
            let _ = RegisterEventHotKey(
                KEYCODE_6,
                CMD_KEY_MASK | SHIFT_KEY_MASK,
                hk_id,
                target,
                0,
                &mut hk_ref,
            );
        }
    }
}

/// Registers a local AppKit keyDown monitor that intercepts Escape (keyCode 53) when `on_escape`
/// returns `true`. Ignores Escape while AppKit is tracking a popup menu (`NSEventTrackingRunLoopMode`)
/// or modal panel (`NSModalPanelRunLoopMode`) so Escape closes menus/sheets instead of the overlay.
pub fn register_local_escape_monitor<F>(on_escape: F)
where
    F: Fn() -> bool + 'static,
{
    use block2::RcBlock;
    use objc2_app_kit::{
        NSEvent, NSEventMask, NSEventTrackingRunLoopMode, NSModalPanelRunLoopMode,
    };
    use objc2_foundation::NSRunLoop;
    use std::ptr::NonNull;

    let block = RcBlock::new(move |ev: NonNull<NSEvent>| -> *mut NSEvent {
        let event = unsafe { ev.as_ref() };
        if event.keyCode() == 53 {
            let mode = NSRunLoop::currentRunLoop().currentMode();
            let in_menu_or_modal = mode.as_deref().is_some_and(|m| unsafe {
                m.isEqualToString(NSEventTrackingRunLoopMode)
                    || m.isEqualToString(NSModalPanelRunLoopMode)
            });
            if !in_menu_or_modal && on_escape() {
                return ptr::null_mut();
            }
        }
        ev.as_ptr()
    });
    unsafe {
        let _ = NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &block);
    }
}

/// Installs a standard AppKit Edit menu on `NSApplication` so Command-key editing shortcuts
/// (`⌘A`, `⌘C`, `⌘V`, `⌘X`, `⌘Z`, `⇧⌘Z`) work reliably inside `NSTextField` controls even when
/// running under `NSApplicationActivationPolicy::Accessory`.
pub fn install_standard_edit_menu(mtm: MainThreadMarker) {
    use objc2::{MainThreadOnly, sel};
    use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
    use objc2_foundation::NSString;

    let app = NSApplication::sharedApplication(mtm);
    let main_menu = NSMenu::new(mtm);

    // Index 0 is reserved by AppKit for the application menu.
    let app_item = NSMenuItem::new(mtm);
    let app_submenu = NSMenu::new(mtm);
    app_item.setSubmenu(Some(&app_submenu));
    main_menu.addItem(&app_item);

    let edit_item = NSMenuItem::new(mtm);
    let edit_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Edit"));
    let specs = [
        ("Undo", sel!(undo:), "z", NSEventModifierFlags::Command),
        (
            "Redo",
            sel!(redo:),
            "z",
            NSEventModifierFlags::Command | NSEventModifierFlags::Shift,
        ),
        ("Cut", sel!(cut:), "x", NSEventModifierFlags::Command),
        ("Copy", sel!(copy:), "c", NSEventModifierFlags::Command),
        ("Paste", sel!(paste:), "v", NSEventModifierFlags::Command),
        (
            "Select All",
            sel!(selectAll:),
            "a",
            NSEventModifierFlags::Command,
        ),
    ];
    for (idx, (title, action, key, mask)) in specs.into_iter().enumerate() {
        if idx == 2 {
            edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
        }
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        };
        item.setKeyEquivalentModifierMask(mask);
        edit_menu.addItem(&item);
    }
    edit_item.setSubmenu(Some(&edit_menu));
    main_menu.addItem(&edit_item);
    app.setMainMenu(Some(&main_menu));
}
