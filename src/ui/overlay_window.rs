use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSBackingStoreType, NSPopUpButton, NSScreen, NSWindow, NSWindowStyleMask};
use objc2_foundation::{NSNumber, NSObjectProtocol, NSRect, NSString};

use crate::fs_safety::{next_unique_token, sanitize_terminal_text};
use crate::geometry::Rect;

define_class!(
    #[unsafe(super(NSWindow))]
    #[thread_kind = MainThreadOnly]
    #[name = "QSRKeyableOverlayWindow"]
    pub struct KeyableOverlayWindow;

    unsafe impl NSObjectProtocol for KeyableOverlayWindow {}

    impl KeyableOverlayWindow {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            true
        }
    }
);

impl KeyableOverlayWindow {
    pub fn new_borderless(mtm: MainThreadMarker, frame: NSRect, defer: bool) -> Retained<NSWindow> {
        let win: Retained<Self> = unsafe {
            msg_send![
                Self::alloc(mtm),
                initWithContentRect: frame,
                styleMask: NSWindowStyleMask::Borderless,
                backing: NSBackingStoreType::Buffered,
                defer: defer
            ]
        };
        Retained::into_super(win)
    }
}

pub fn nsrect_to_rect(r: NSRect) -> Rect {
    Rect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}

pub fn extract_nsscreen_display_id(screen: &NSScreen) -> Option<u32> {
    let desc = screen.deviceDescription();
    let key = NSString::from_str("NSScreenNumber");
    let val: Option<Retained<AnyObject>> = desc.objectForKey(&key);
    let obj = val?;
    let num: &NSNumber = unsafe { &*(Retained::as_ptr(&obj) as *const NSNumber) };
    Some(num.unsignedIntValue())
}

/// Appends an item via AppKit's standard `addItemWithTitle:` using a control-character-prefixed,
/// per-call unique placeholder title, then immediately renames `lastItem()` to a control-free
/// sanitized display title and sets its `tag`. Because `sanitize_terminal_text` strips all ASCII
/// control characters (`< 0x20`), no external window/device title can ever collide with a future
/// placeholder or trigger `addItemWithTitle:` deduplication.
pub fn append_tagged_popup_item(popup: &NSPopUpButton, title: &str, tag: isize) {
    let safe_title = sanitize_popup_title(title);
    let placeholder = unique_popup_placeholder(popup.numberOfItems(), tag);
    popup.addItemWithTitle(&NSString::from_str(&placeholder));
    if let Some(item) = popup.lastItem() {
        item.setTitle(&NSString::from_str(&safe_title));
        item.setTag(tag);
        item.setEnabled(true);
    }
}

pub(crate) fn sanitize_popup_title(title: &str) -> String {
    sanitize_terminal_text(title, false)
}

pub(crate) fn unique_popup_placeholder(index: isize, tag: isize) -> String {
    format!("\u{1f}qsr-{}-{index}-{tag}", next_unique_token())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitized_popup_title_can_never_collide_with_reserved_placeholder() {
        let placeholder = unique_popup_placeholder(1, 1);
        assert!(placeholder.starts_with('\u{1f}'));
        let forged = sanitize_popup_title(&placeholder);
        assert!(!forged.contains('\u{1f}'));
        assert_ne!(forged, placeholder);
        assert_ne!(
            unique_popup_placeholder(1, 1),
            unique_popup_placeholder(1, 1)
        );
    }
}
