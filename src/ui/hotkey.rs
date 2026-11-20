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
