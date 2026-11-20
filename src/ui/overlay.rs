use std::cell::RefCell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSBackingStoreType,
    NSBezierPath, NSColor, NSEvent, NSFloatingWindowLevel, NSPopUpButton, NSScreen,
    NSStatusWindowLevel, NSTextField, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSNumber, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use crate::capture::shareable::{DisplayInfo, WindowInfo};
use crate::capture::{AudioCaptureMode, CaptureConfig};
use crate::config::Preferences;
use crate::geometry::{AspectRatio, DragMode, Rect, apply_drag, hit_test_selection};
use crate::ui::overlay_actions::build_toolbar_actions;
use crate::ui::overlay_toolbar::{OverlayToolbarActions, populate_toolbar_controls};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    Area,
    Window,
    Screen,
}

pub type StartCaptureCallback = Rc<dyn Fn(Result<(CaptureConfig, Preferences), String>)>;

struct ActiveDrag {
    mode: DragMode,
    start_x: f64,
    start_y: f64,
    initial_rect: Rect,
}

pub struct OverlayState {
    pub mode: SelectionMode,
    pub selection: Rect,
    pub bounds: Rect,
    pub overlay_display_id: u32,
    pub screen_display_id: u32,
    pub scale_factor: f64,
    pub aspect: AspectRatio,
    pub fps: u32,
    pub show_cursor: bool,
    pub highlight_clicks: bool,
    pub audio_mode: AudioCaptureMode,
    pub windows: Vec<WindowInfo>,
    pub selected_window_idx: usize,
    pub displays: Vec<DisplayInfo>,
    pub selected_display_idx: usize,
    pub demo_backdrop: bool,
    drag: Option<ActiveDrag>,
}

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

struct CanvasIvars {
    state: Rc<RefCell<OverlayState>>,
    on_selection_changed: Rc<dyn Fn()>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "QSROverlayCanvasView"]
    #[ivars = CanvasIvars]
    struct OverlayCanvasView;

    unsafe impl NSObjectProtocol for OverlayCanvasView {}

    impl OverlayCanvasView {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let st = self.ivars().state.borrow();
            let view_bounds = self.bounds();

            if st.demo_backdrop {
                let bg = NSColor::colorWithSRGBRed_green_blue_alpha(0.11, 0.14, 0.20, 1.0);
                bg.setFill();
                NSBezierPath::fillRect(view_bounds);

                let card = NSRect::new(
                    NSPoint::new(
                        view_bounds.size.width * 0.16,
                        view_bounds.size.height * 0.22,
                    ),
                    NSSize::new(
                        view_bounds.size.width * 0.68,
                        view_bounds.size.height * 0.58,
                    ),
                );
                let card_bg = NSColor::colorWithSRGBRed_green_blue_alpha(0.16, 0.21, 0.30, 1.0);
                card_bg.setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(card, 14.0, 14.0).fill();
            }

            let sel = st.selection;
            let dim = NSColor::colorWithSRGBRed_green_blue_alpha(0.04, 0.05, 0.08, 0.56);
            dim.setFill();

            let bw = view_bounds.size.width;
            let bh = view_bounds.size.height;
            let top_y = sel.top_y_up();
            if top_y < bh {
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(0.0, top_y),
                    NSSize::new(bw, bh - top_y),
                ));
            }
            if sel.y > 0.0 {
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(0.0, 0.0),
                    NSSize::new(bw, sel.y),
                ));
            }
            if sel.x > 0.0 {
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(0.0, sel.y),
                    NSSize::new(sel.x, sel.height),
                ));
            }
            if sel.right() < bw {
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(sel.right(), sel.y),
                    NSSize::new(bw - sel.right(), sel.height),
                ));
            }

            let sel_rect = NSRect::new(
                NSPoint::new(sel.x, sel.y),
                NSSize::new(sel.width, sel.height),
            );
            let border_color = NSColor::colorWithSRGBRed_green_blue_alpha(0.98, 0.99, 1.0, 0.95);
            border_color.setStroke();
            let border = NSBezierPath::bezierPathWithRect(sel_rect);
            border.setLineWidth(1.5);
            border.stroke();

            let grid_color = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.18);
            grid_color.setStroke();
            for step in [1.0 / 3.0, 2.0 / 3.0] {
                let gx = sel.x + sel.width * step;
                let gy = sel.y + sel.height * step;
                let vline = NSBezierPath::bezierPath();
                vline.moveToPoint(NSPoint::new(gx, sel.y));
                vline.lineToPoint(NSPoint::new(gx, sel.top_y_up()));
                vline.setLineWidth(1.0);
                vline.stroke();

                let hline = NSBezierPath::bezierPath();
                hline.moveToPoint(NSPoint::new(sel.x, gy));
                hline.lineToPoint(NSPoint::new(sel.right(), gy));
                hline.setLineWidth(1.0);
                hline.stroke();
            }

            if st.mode == SelectionMode::Area {
                border_color.setFill();
                let pts = [
                    (sel.x, sel.y),
                    (sel.x + sel.width * 0.5, sel.y),
                    (sel.right(), sel.y),
                    (sel.right(), sel.y + sel.height * 0.5),
                    (sel.right(), sel.top_y_up()),
                    (sel.x + sel.width * 0.5, sel.top_y_up()),
                    (sel.x, sel.top_y_up()),
                    (sel.x, sel.y + sel.height * 0.5),
                ];
                for (hx, hy) in pts {
                    let hrect =
                        NSRect::new(NSPoint::new(hx - 4.5, hy - 4.5), NSSize::new(9.0, 9.0));
                    NSBezierPath::bezierPathWithOvalInRect(hrect).fill();
                }
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let loc = event.locationInWindow();
            let mut st = self.ivars().state.borrow_mut();
            if st.mode != SelectionMode::Area {
                return;
            }
            let mode =
                hit_test_selection(st.selection, loc.x, loc.y, 10.0).unwrap_or(DragMode::Create);
            let initial_rect = st.selection;
            st.drag = Some(ActiveDrag {
                mode,
                start_x: loc.x,
                start_y: loc.y,
                initial_rect,
            });
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let loc = event.locationInWindow();
            let changed = {
                let mut st = self.ivars().state.borrow_mut();
                if st.mode != SelectionMode::Area {
                    false
                } else if let Some(drag) = st.drag.as_ref() {
                    st.selection = apply_drag(
                        drag.initial_rect,
                        drag.mode,
                        drag.start_x,
                        drag.start_y,
                        loc.x,
                        loc.y,
                        st.aspect,
                        st.bounds,
                    );
                    true
                } else {
                    false
                }
            };
            if changed {
                self.setNeedsDisplay(true);
                (self.ivars().on_selection_changed)();
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            self.ivars().state.borrow_mut().drag = None;
        }
    }
);

pub struct SelectorOverlay {
    pub canvas_window: Retained<NSWindow>,
    pub toolbar_window: Retained<NSWindow>,
    _actions: Retained<OverlayToolbarActions>,
}

impl SelectorOverlay {
    pub fn show(
        mtm: MainThreadMarker,
        prefs: &Preferences,
        displays: Vec<DisplayInfo>,
        windows: Vec<WindowInfo>,
        demo_backdrop: bool,
        on_start: StartCaptureCallback,
        on_cancel: Rc<dyn Fn()>,
    ) -> Result<Self, String> {
        let screen = NSScreen::mainScreen(mtm).ok_or("No active macOS screen found")?;
        let screen_frame = screen.frame();
        let scale_factor = screen.backingScaleFactor();
        let matched_display_id = if demo_backdrop {
            displays.first().map(|d| d.display_id).unwrap_or(1)
        } else {
            let sid = extract_nsscreen_display_id(&screen)
                .ok_or("Unable to resolve CGDirectDisplayID for active NSScreen")?;
            displays
                .iter()
                .find(|d| d.display_id == sid)
                .map(|d| d.display_id)
                .ok_or_else(|| {
                    format!(
                        "Active screen (ID {sid}) is not present in ScreenCaptureKit display list"
                    )
                })?
        };
        let matched_idx = displays
            .iter()
            .position(|d| d.display_id == matched_display_id)
            .unwrap_or(0);

        let win_frame = if demo_backdrop {
            NSRect::new(
                NSPoint::new(
                    screen_frame.origin.x + (screen_frame.size.width - 1080.0).max(40.0) * 0.5,
                    screen_frame.origin.y + (screen_frame.size.height - 680.0).max(40.0) * 0.5,
                ),
                NSSize::new(1080.0, 680.0),
            )
        } else {
            screen_frame
        };

        let local_bounds = Rect::new(0.0, 0.0, win_frame.size.width, win_frame.size.height);
        let initial_sel = if demo_backdrop {
            Rect::new(190.0, 170.0, 700.0, 420.0)
        } else {
            prefs.last_region.clamp_and_align_even(local_bounds)
        };

        let state = Rc::new(RefCell::new(OverlayState {
            mode: SelectionMode::Area,
            selection: initial_sel,
            bounds: local_bounds,
            overlay_display_id: matched_display_id,
            screen_display_id: matched_display_id,
            scale_factor,
            aspect: prefs.aspect_ratio,
            fps: if prefs.fps >= 60 { 60 } else { 30 },
            show_cursor: prefs.show_cursor,
            highlight_clicks: prefs.highlight_clicks,
            audio_mode: prefs.audio_mode,
            windows,
            selected_window_idx: 0,
            displays,
            selected_display_idx: matched_display_idx(matched_idx),
            demo_backdrop,
            drag: None,
        }));

        let canvas_window = KeyableOverlayWindow::new_borderless(mtm, win_frame, false);
        unsafe { canvas_window.setReleasedWhenClosed(false) };
        if let Some(dark) = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }) {
            canvas_window.setAppearance(Some(&dark));
        }
        canvas_window.setOpaque(demo_backdrop);
        canvas_window.setBackgroundColor(Some(&NSColor::clearColor()));
        canvas_window.setLevel(NSFloatingWindowLevel);

        let scale_for_px = scale_factor.max(1.0);
        let w_field = NSTextField::textFieldWithString(
            &NSString::from_str(&format!(
                "{}",
                (initial_sel.width * scale_for_px).round() as u32
            )),
            mtm,
        );
        let h_field = NSTextField::textFieldWithString(
            &NSString::from_str(&format!(
                "{}",
                (initial_sel.height * scale_for_px).round() as u32
            )),
            mtm,
        );
        let aspect_popup = NSPopUpButton::new(mtm);
        for ar in AspectRatio::ALL {
            aspect_popup.addItemWithTitle(&NSString::from_str(ar.label()));
        }
        aspect_popup.selectItemAtIndex(prefs.aspect_ratio.index() as isize);

        let target_popup = NSPopUpButton::new(mtm);
        target_popup.setHidden(true);

        let w_sync = w_field.clone();
        let h_sync = h_field.clone();
        let state_sync = Rc::clone(&state);
        let sync_fields: Rc<dyn Fn()> = Rc::new(move || {
            let st = state_sync.borrow();
            let sf = st.scale_factor.max(1.0);
            w_sync.setStringValue(&NSString::from_str(&format!(
                "{}",
                (st.selection.width * sf).round() as u32
            )));
            h_sync.setStringValue(&NSString::from_str(&format!(
                "{}",
                (st.selection.height * sf).round() as u32
            )));
        });

        let canvas_view: Retained<OverlayCanvasView> = {
            let this = OverlayCanvasView::alloc(mtm).set_ivars(CanvasIvars {
                state: Rc::clone(&state),
                on_selection_changed: Rc::clone(&sync_fields),
            });
            unsafe {
                msg_send![super(this), initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), win_frame.size)]
            }
        };
        canvas_window.setContentView(Some(&canvas_view));

        let (bar_w, bar_h) = (840.0, 52.0);
        let bar_rect = NSRect::new(
            NSPoint::new(
                win_frame.origin.x + (win_frame.size.width - bar_w) * 0.5,
                win_frame.origin.y + 36.0,
            ),
            NSSize::new(bar_w, bar_h),
        );
        let toolbar_window = KeyableOverlayWindow::new_borderless(mtm, bar_rect, demo_backdrop);
        unsafe { toolbar_window.setReleasedWhenClosed(false) };
        if let Some(dark) = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }) {
            toolbar_window.setAppearance(Some(&dark));
        }

        let bar_container = NSView::initWithFrame(
            NSView::alloc(mtm),
            if demo_backdrop {
                NSRect::new(
                    NSPoint::new((win_frame.size.width - bar_w) * 0.5, 28.0),
                    NSSize::new(bar_w, bar_h),
                )
            } else {
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(bar_w, bar_h))
            },
        );

        let canvas_nsview: Retained<NSView> = Retained::into_super(canvas_view.clone());
        let actions = build_toolbar_actions(
            mtm,
            &state,
            &canvas_window,
            &toolbar_window,
            &canvas_nsview,
            &aspect_popup,
            &w_field,
            &h_field,
            &target_popup,
            &sync_fields,
            prefs,
            on_start,
            on_cancel,
        );

        populate_toolbar_controls(
            mtm,
            &bar_container,
            &actions,
            &aspect_popup,
            &w_field,
            &h_field,
            &target_popup,
            prefs,
        );

        if demo_backdrop {
            canvas_view.addSubview(&bar_container);
        } else {
            toolbar_window.setOpaque(false);
            toolbar_window.setBackgroundColor(Some(&NSColor::clearColor()));
            toolbar_window.setLevel(NSStatusWindowLevel);
            toolbar_window.setContentView(Some(&bar_container));
        }

        canvas_window.makeKeyAndOrderFront(None);
        if !demo_backdrop {
            toolbar_window.makeKeyAndOrderFront(None);
        }

        Ok(Self {
            canvas_window,
            toolbar_window,
            _actions: actions,
        })
    }

    pub fn close(&self) {
        self.toolbar_window.orderOut(None);
        self.canvas_window.orderOut(None);
        self.toolbar_window.close();
        self.canvas_window.close();
    }
}

pub fn extract_nsscreen_display_id(screen: &NSScreen) -> Option<u32> {
    let desc = screen.deviceDescription();
    let key = NSString::from_str("NSScreenNumber");
    let val: Option<Retained<AnyObject>> = desc.objectForKey(&key);
    let obj = val?;
    let num: &NSNumber = unsafe { &*(Retained::as_ptr(&obj) as *const NSNumber) };
    Some(num.unsignedIntValue())
}

const fn matched_display_idx(idx: usize) -> usize {
    idx
}
