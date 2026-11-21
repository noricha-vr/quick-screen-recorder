use std::cell::RefCell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSBezierPath, NSColor, NSCursor, NSEvent, NSPopUpButton, NSView};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize};

use crate::geometry::{DragMode, Rect, apply_drag, hit_test_selection};
use crate::ui::overlay::{ActiveDrag, OverlayState, SelectionMode};

/// Minimum non-zero alpha (`0.06 > 0.05`) so macOS WindowServer delivers mouse clicks and drags
/// inside the selection cutout instead of passing clicks through to underlying applications.
const HIT_TEST_FILL_ALPHA: f64 = 0.06;

pub struct CanvasIvars {
    pub state: Rc<RefCell<OverlayState>>,
    pub target_popup: Retained<NSPopUpButton>,
    pub on_selection_changed: Rc<dyn Fn()>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "QSROverlayCanvasView"]
    #[ivars = CanvasIvars]
    pub struct OverlayCanvasView;

    unsafe impl NSObjectProtocol for OverlayCanvasView {}

    impl OverlayCanvasView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let st = self.ivars().state.borrow();
            let view_bounds = self.bounds();

            if st.demo_backdrop {
                draw_demo_backdrop(view_bounds);
            }

            let sel = st.selection;
            draw_outer_dim_mask(view_bounds, sel);

            let sel_rect = NSRect::new(
                NSPoint::new(sel.x, sel.y),
                NSSize::new(sel.width, sel.height),
            );

            // Paint interior above WindowServer's 0.05 alpha threshold so interior drag & window clicks never pass through
            if !st.demo_backdrop {
                let hit_fill =
                    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, HIT_TEST_FILL_ALPHA);
                hit_fill.setFill();
                NSBezierPath::fillRect(sel_rect);
            }

            // In Window mode, highlight any hovered background window so the user sees it can be clicked
            if st.mode == SelectionMode::Window {
                if let Some(h_idx) = st.hovered_window_idx {
                    if h_idx != st.selected_window_idx {
                        if let Some(hover_r) = st.window_overlay_rect(h_idx) {
                            let h_rect = NSRect::new(
                                NSPoint::new(hover_r.x, hover_r.y),
                                NSSize::new(hover_r.width, hover_r.height),
                            );
                            let hover_fill =
                                NSColor::colorWithSRGBRed_green_blue_alpha(0.28, 0.66, 1.0, 0.12);
                            hover_fill.setFill();
                            NSBezierPath::fillRect(h_rect);

                            let hover_stroke =
                                NSColor::colorWithSRGBRed_green_blue_alpha(0.42, 0.76, 1.0, 0.88);
                            hover_stroke.setStroke();
                            let hp = NSBezierPath::bezierPathWithRect(h_rect);
                            hp.setLineWidth(2.0);
                            hp.stroke();
                        }
                    }
                }
            }

            let border_color = if st.mode == SelectionMode::Window {
                NSColor::colorWithSRGBRed_green_blue_alpha(0.36, 0.74, 1.0, 0.98)
            } else {
                NSColor::colorWithSRGBRed_green_blue_alpha(0.98, 0.99, 1.0, 0.95)
            };
            border_color.setStroke();
            let border = NSBezierPath::bezierPathWithRect(sel_rect);
            border.setLineWidth(if st.mode == SelectionMode::Window {
                2.2
            } else {
                1.5
            });
            border.stroke();

            if st.mode == SelectionMode::Area {
                draw_thirds_grid_and_handles(sel, &border_color);
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let loc = event.locationInWindow();
            let mut st = self.ivars().state.borrow_mut();
            match st.mode {
                SelectionMode::Area => {
                    match hit_test_selection(st.selection, loc.x, loc.y, 10.0) {
                        Some(DragMode::Move) => NSCursor::openHandCursor().set(),
                        _ => NSCursor::crosshairCursor().set(),
                    }
                }
                SelectionMode::Window => {
                    let hit = st.hit_test_window_at(loc.x, loc.y, false);
                    if hit.is_some() {
                        NSCursor::pointingHandCursor().set();
                    } else {
                        NSCursor::arrowCursor().set();
                    }
                    if st.hovered_window_idx != hit {
                        st.hovered_window_idx = hit;
                        drop(st);
                        self.setNeedsDisplay(true);
                    }
                }
                SelectionMode::Screen => {
                    NSCursor::arrowCursor().set();
                }
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let loc = event.locationInWindow();
            let mut st = self.ivars().state.borrow_mut();
            match st.mode {
                SelectionMode::Area => {
                    let mode = hit_test_selection(st.selection, loc.x, loc.y, 10.0)
                        .unwrap_or(DragMode::Create);
                    if mode == DragMode::Move {
                        NSCursor::closedHandCursor().set();
                    } else {
                        NSCursor::crosshairCursor().set();
                    }
                    let initial_rect = st.selection;
                    st.drag = Some(ActiveDrag {
                        mode,
                        start_x: loc.x,
                        start_y: loc.y,
                        initial_rect,
                    });
                }
                SelectionMode::Window => {
                    if let Some(idx) = st.hit_test_window_at(loc.x, loc.y, true) {
                        if st.select_window(idx) {
                            st.hovered_window_idx = Some(idx);
                            drop(st);
                            self.ivars().target_popup.selectItemAtIndex(idx as isize);
                            (self.ivars().on_selection_changed)();
                            self.setNeedsDisplay(true);
                        }
                    }
                }
                SelectionMode::Screen => {}
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let loc = event.locationInWindow();
            let changed = {
                let mut st = self.ivars().state.borrow_mut();
                if st.mode != SelectionMode::Area {
                    false
                } else if let Some(drag) = st.drag.as_ref() {
                    if drag.mode == DragMode::Move {
                        NSCursor::closedHandCursor().set();
                    }
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
        fn mouse_up(&self, event: &NSEvent) {
            let loc = event.locationInWindow();
            let mode_after = {
                let mut st = self.ivars().state.borrow_mut();
                st.drag = None;
                if st.mode == SelectionMode::Area {
                    hit_test_selection(st.selection, loc.x, loc.y, 10.0)
                } else {
                    None
                }
            };
            if mode_after == Some(DragMode::Move) {
                NSCursor::openHandCursor().set();
            }
        }
    }
);

impl OverlayCanvasView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect, ivars: CanvasIvars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

fn draw_demo_backdrop(view_bounds: NSRect) {
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

fn draw_outer_dim_mask(view_bounds: NSRect, sel: Rect) {
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
        NSBezierPath::fillRect(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(bw, sel.y)));
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
}

fn draw_thirds_grid_and_handles(sel: Rect, border_color: &NSColor) {
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
        let hrect = NSRect::new(NSPoint::new(hx - 4.5, hy - 4.5), NSSize::new(9.0, 9.0));
        NSBezierPath::bezierPathWithOvalInRect(hrect).fill();
    }
}
