use std::cell::RefCell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSCursor, NSEvent, NSEventTrackingRunLoopMode,
    NSModalPanelRunLoopMode, NSPopUpButton, NSScreen, NSTrackingArea, NSTrackingAreaOptions,
    NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSNumber, NSObjectProtocol, NSRect, NSRunLoop, NSString};

use crate::fs_safety::{next_unique_token, sanitize_terminal_text};
use crate::geometry::Rect;
use crate::ui::overlay::OverlayState;

pub struct OverlayWindowIvars {
    can_become_key: bool,
}

define_class!(
    #[unsafe(super(NSWindow))]
    #[thread_kind = MainThreadOnly]
    #[name = "QSRKeyableOverlayWindow"]
    #[ivars = OverlayWindowIvars]
    pub struct KeyableOverlayWindow;

    unsafe impl NSObjectProtocol for KeyableOverlayWindow {}

    impl KeyableOverlayWindow {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            self.ivars().can_become_key
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            self.ivars().can_become_key
        }
    }
);

impl KeyableOverlayWindow {
    pub fn new_borderless(
        mtm: MainThreadMarker,
        frame: NSRect,
        defer: bool,
        can_become_key: bool,
    ) -> Retained<NSWindow> {
        let this = Self::alloc(mtm).set_ivars(OverlayWindowIvars { can_become_key });
        let win: Retained<Self> = unsafe {
            msg_send![
                super(this),
                initWithContentRect: frame,
                styleMask: NSWindowStyleMask::Borderless,
                backing: NSBackingStoreType::Buffered,
                defer: defer
            ]
        };
        Retained::into_super(win)
    }
}

pub struct ToolbarContainerIvars {
    state: Rc<RefCell<OverlayState>>,
    canvas_view: RefCell<Option<Retained<NSView>>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "QSRToolbarContainerView"]
    #[ivars = ToolbarContainerIvars]
    pub struct ToolbarContainerView;

    unsafe impl NSObjectProtocol for ToolbarContainerView {}

    impl ToolbarContainerView {
        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.clear_canvas_window_hover();
            // Reset any canvas pointing-hand / crosshair cursor once on entry; leave `mouseMoved:`
            // free so `NSTextField` I-beam cursor rects inside the toolbar continue working.
            NSCursor::arrowCursor().set();
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, _event: &NSEvent) {
            self.clear_canvas_window_hover();
        }
    }
);

impl ToolbarContainerView {
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        state: Rc<RefCell<OverlayState>>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ToolbarContainerIvars {
            state,
            canvas_view: RefCell::new(None),
        });
        let view: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        install_hover_tracking_area(&view);
        view
    }

    pub fn attach_canvas(&self, canvas_view: Retained<NSView>) {
        *self.ivars().canvas_view.borrow_mut() = Some(canvas_view);
    }

    pub fn detach_canvas(&self) {
        *self.ivars().canvas_view.borrow_mut() = None;
    }

    fn clear_canvas_window_hover(&self) {
        let changed = self
            .ivars()
            .state
            .borrow_mut()
            .hovered_window_idx
            .take()
            .is_some();
        if changed {
            if let Some(cv) = self.ivars().canvas_view.borrow().as_ref() {
                cv.setNeedsDisplay(true);
            }
        }
    }
}

/// Attaches a full-visible-rect tracking area so mouse moves and enters/exits deliver reliably
/// on both non-key canvas windows and the floating toolbar across display moves and resizes.
pub fn install_hover_tracking_area(view: &NSView) {
    let options = NSTrackingAreaOptions::MouseEnteredAndExited
        | NSTrackingAreaOptions::MouseMoved
        | NSTrackingAreaOptions::ActiveAlways
        | NSTrackingAreaOptions::InVisibleRect;
    let area = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            NSTrackingArea::alloc(),
            view.bounds(),
            options,
            Some(view),
            None,
        )
    };
    view.addTrackingArea(&area);
}

/// Keeps `toolbar_window` as the active key window ONLY while our app is active, the selector is
/// visible, and AppKit is not currently tracking a popup menu or modal panel.
pub fn ensure_toolbar_key_if_active(
    mtm: MainThreadMarker,
    toolbar_window: &NSWindow,
    demo_backdrop: bool,
) {
    if demo_backdrop {
        return;
    }
    let app = NSApplication::sharedApplication(mtm);
    if !app.isActive() || !toolbar_window.isVisible() {
        return;
    }
    let in_menu_or_modal = NSRunLoop::currentRunLoop()
        .currentMode()
        .as_deref()
        .is_some_and(|m| unsafe {
            m.isEqualToString(NSEventTrackingRunLoopMode)
                || m.isEqualToString(NSModalPanelRunLoopMode)
        });
    if in_menu_or_modal {
        return;
    }
    toolbar_window.makeKeyAndOrderFront(None);
}

/// Ends active `NSTextField` editing on `toolbar_window` BEFORE borrowing `OverlayState`, avoiding
/// `RefCell` re-entrancy when AppKit commits `sizeCommitted:` synchronously on end-editing.
pub fn commit_toolbar_text_editing(toolbar_window: &NSWindow) {
    let Some(responder) = toolbar_window.firstResponder() else {
        return;
    };
    let win_ptr = toolbar_window as *const NSWindow as *const AnyObject;
    let resp_ptr = Retained::as_ptr(&responder) as *const AnyObject;
    if win_ptr == resp_ptr {
        return;
    }
    if !toolbar_window.makeFirstResponder(None) {
        unsafe { toolbar_window.endEditingFor(None) };
    }
}

/// Canonical coordinate-based check suppressing canvas window hover whenever the pointer is
/// currently inside the floating toolbar rectangle (in matching screen or canvas coordinates).
pub fn is_cursor_over_toolbar(cursor_pos: (f64, f64), toolbar_rect: Rect) -> bool {
    toolbar_rect.contains(cursor_pos.0, cursor_pos.1)
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

    #[test]
    fn cursor_over_toolbar_handles_primary_and_negative_secondary_display_coords() {
        let primary_tb = Rect::new(320.0, 80.0, 628.0, 88.0);
        assert!(is_cursor_over_toolbar((320.0, 80.0), primary_tb));
        assert!(is_cursor_over_toolbar((600.0, 120.0), primary_tb));
        assert!(is_cursor_over_toolbar((948.0, 168.0), primary_tb));
        assert!(!is_cursor_over_toolbar((319.9, 120.0), primary_tb));
        assert!(!is_cursor_over_toolbar((600.0, 168.1), primary_tb));

        // Secondary display positioned to the left of primary (negative X screen coordinates).
        let left_display_tb = Rect::new(-1400.0, 120.0, 628.0, 88.0);
        assert!(is_cursor_over_toolbar((-1100.0, 150.0), left_display_tb));
        assert!(!is_cursor_over_toolbar((-771.0, 150.0), left_display_tb));
    }
}
