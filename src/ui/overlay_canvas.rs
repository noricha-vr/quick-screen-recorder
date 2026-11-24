use std::cell::RefCell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBezierPath, NSColor, NSCursor, NSEvent, NSGraphicsContext, NSPopUpButton, NSView,
    NSWindingRule, NSWindow,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize};

use crate::geometry::{DragMode, Rect, apply_drag, hit_test_selection};
use crate::ui::overlay::{ActiveDrag, OverlayState, SelectionMode};
use crate::ui::overlay_window::{
    commit_toolbar_text_editing, install_hover_tracking_area, is_cursor_over_toolbar,
    nsrect_to_rect,
};

/// Minimum non-zero alpha (`0.06 > 0.05`) so macOS WindowServer delivers mouse clicks and drags
/// inside the selection cutout instead of passing clicks through to underlying applications.
const HIT_TEST_FILL_ALPHA: f64 = 0.06;

/// Subtle previous-gen macOS window corner radius (Ventura/Sonoma 10pt style: crisp, not overly round).
const SELECTION_CORNER_RADIUS: f64 = 10.0;

pub struct CanvasIvars {
    pub state: Rc<RefCell<OverlayState>>,
    pub toolbar_window: Retained<NSWindow>,
    pub toolbar_view: Retained<NSView>,
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
            let radius = effective_corner_radius(st.mode, sel);
            draw_outer_dim_mask(view_bounds, sel, radius);

            let sel_rect = NSRect::new(
                NSPoint::new(sel.x, sel.y),
                NSSize::new(sel.width, sel.height),
            );
            let sel_path = rounded_or_square_path(sel_rect, radius);

            // Paint interior above WindowServer's 0.05 alpha threshold so interior drag & window clicks never pass through
            if !st.demo_backdrop {
                let hit_fill =
                    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, HIT_TEST_FILL_ALPHA);
                hit_fill.setFill();
                sel_path.fill();
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
                            let h_rad = effective_corner_radius(SelectionMode::Window, hover_r);
                            let hp = rounded_or_square_path(h_rect, h_rad);
                            let hover_fill =
                                NSColor::colorWithSRGBRed_green_blue_alpha(0.28, 0.66, 1.0, 0.12);
                            hover_fill.setFill();
                            hp.fill();

                            let hover_stroke =
                                NSColor::colorWithSRGBRed_green_blue_alpha(0.42, 0.76, 1.0, 0.88);
                            hover_stroke.setStroke();
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
            sel_path.setLineWidth(if st.mode == SelectionMode::Window {
                2.2
            } else {
                1.75
            });
            sel_path.stroke();

            if st.mode == SelectionMode::Area {
                draw_thirds_grid_and_handles(sel, &sel_path, radius, &border_color);
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let loc = event.locationInWindow();
            let over_tb = self.is_local_point_over_toolbar(loc);
            self.update_hover_for_local_point(loc, over_tb);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            let changed = self
                .ivars()
                .state
                .borrow_mut()
                .hovered_window_idx
                .take()
                .is_some();
            if changed {
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let loc = event.locationInWindow();
            let demo_backdrop = self.ivars().state.borrow().demo_backdrop;
            if !demo_backdrop {
                // Commit any active `w_field`/`h_field` editing BEFORE borrowing `OverlayState`
                // so synchronous `sizeCommitted:` actions can never cause a `RefCell` double borrow.
                commit_toolbar_text_editing(&self.ivars().toolbar_window);
            }
            if self.is_local_point_over_toolbar(loc) {
                return;
            }
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
            let over_tb = self.is_local_point_over_toolbar(loc);
            let mode_after = {
                let mut st = self.ivars().state.borrow_mut();
                st.drag = None;
                if !over_tb && st.mode == SelectionMode::Area {
                    hit_test_selection(st.selection, loc.x, loc.y, 10.0)
                } else {
                    None
                }
            };
            if mode_after == Some(DragMode::Move) {
                NSCursor::openHandCursor().set();
            } else if over_tb {
                NSCursor::arrowCursor().set();
            }
        }
    }
);

impl OverlayCanvasView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect, ivars: CanvasIvars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        let view: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        install_hover_tracking_area(&view);
        view
    }

    pub fn ivars_toolbar_rect_demo(&self) -> Rect {
        nsrect_to_rect(self.ivars().toolbar_view.frame())
    }

    /// Evaluates whether a canvas-local point falls inside the floating toolbar using canonical
    /// coordinates (AppKit global screen points in live two-window mode; canvas points in demo mode).
    pub fn is_local_point_over_toolbar(&self, local_pt: NSPoint) -> bool {
        let demo_backdrop = self.ivars().state.borrow().demo_backdrop;
        if demo_backdrop {
            let tb_rect = nsrect_to_rect(self.ivars().toolbar_view.frame());
            is_cursor_over_toolbar((local_pt.x, local_pt.y), tb_rect)
        } else {
            let tb_rect = nsrect_to_rect(self.ivars().toolbar_window.frame());
            let screen_pt = self
                .window()
                .map(|w| w.convertPointToScreen(local_pt))
                .unwrap_or_else(NSEvent::mouseLocation);
            is_cursor_over_toolbar((screen_pt.x, screen_pt.y), tb_rect)
        }
    }

    /// Updates canvas hover highlight and cursor for `local_pt`, suppressing any background window
    /// highlight whenever `cursor_over_toolbar` is `true`.
    pub fn update_hover_for_local_point(&self, local_pt: NSPoint, cursor_over_toolbar: bool) {
        let mut st = self.ivars().state.borrow_mut();
        if cursor_over_toolbar {
            if st.hovered_window_idx.take().is_some() {
                drop(st);
                self.setNeedsDisplay(true);
            }
            return;
        }
        match st.mode {
            SelectionMode::Area => {
                match hit_test_selection(st.selection, local_pt.x, local_pt.y, 10.0) {
                    Some(DragMode::Move) => NSCursor::openHandCursor().set(),
                    _ => NSCursor::crosshairCursor().set(),
                }
            }
            SelectionMode::Window => {
                let hit = st.hit_test_window_at(local_pt.x, local_pt.y, false);
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
}

fn effective_corner_radius(mode: SelectionMode, sel: Rect) -> f64 {
    if mode == SelectionMode::Screen {
        0.0
    } else {
        SELECTION_CORNER_RADIUS
            .min(sel.width * 0.25)
            .min(sel.height * 0.25)
    }
}

fn rounded_or_square_path(rect: NSRect, radius: f64) -> Retained<NSBezierPath> {
    if radius > 0.0 {
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius, radius)
    } else {
        NSBezierPath::bezierPathWithRect(rect)
    }
}

fn draw_demo_backdrop(view_bounds: NSRect) {
    // Dark desktop wallpaper + dark app window so the elevated floating toolbar's
    // high-contrast visibility on dark screens is verified directly in screenshots.
    let bg = NSColor::colorWithSRGBRed_green_blue_alpha(0.06, 0.08, 0.12, 1.0);
    bg.setFill();
    NSBezierPath::fillRect(view_bounds);

    let card = NSRect::new(
        NSPoint::new(
            view_bounds.size.width * 0.16,
            view_bounds.size.height * 0.23,
        ),
        NSSize::new(
            view_bounds.size.width * 0.68,
            view_bounds.size.height * 0.62,
        ),
    );
    let card_bg = NSColor::colorWithSRGBRed_green_blue_alpha(0.11, 0.14, 0.21, 1.0);
    card_bg.setFill();
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(card, 11.0, 11.0).fill();
}

fn draw_outer_dim_mask(view_bounds: NSRect, sel: Rect, radius: f64) {
    let dim = NSColor::colorWithSRGBRed_green_blue_alpha(0.03, 0.04, 0.07, 0.56);
    dim.setFill();

    let mask = NSBezierPath::bezierPathWithRect(view_bounds);
    let sel_rect = NSRect::new(
        NSPoint::new(sel.x, sel.y),
        NSSize::new(sel.width, sel.height),
    );
    mask.appendBezierPath(&rounded_or_square_path(sel_rect, radius));
    mask.setWindingRule(NSWindingRule::EvenOdd);
    mask.fill();
}

fn draw_thirds_grid_and_handles(
    sel: Rect,
    sel_path: &NSBezierPath,
    radius: f64,
    border_color: &NSColor,
) {
    NSGraphicsContext::saveGraphicsState_class();
    sel_path.addClip();
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
    NSGraphicsContext::restoreGraphicsState_class();

    // Seat corner handles squarely on the 45-degree arc of the rounded corners,
    // sized cleanly (7pt + dark halo) so the 10pt corner curve remains visible.
    let corner_inset = radius * (1.0 - std::f64::consts::FRAC_1_SQRT_2);
    let halo = NSColor::colorWithSRGBRed_green_blue_alpha(0.04, 0.05, 0.08, 0.72);
    let pts = [
        (sel.x + corner_inset, sel.y + corner_inset),
        (sel.x + sel.width * 0.5, sel.y),
        (sel.right() - corner_inset, sel.y + corner_inset),
        (sel.right(), sel.y + sel.height * 0.5),
        (sel.right() - corner_inset, sel.top_y_up() - corner_inset),
        (sel.x + sel.width * 0.5, sel.top_y_up()),
        (sel.x + corner_inset, sel.top_y_up() - corner_inset),
        (sel.x, sel.y + sel.height * 0.5),
    ];
    for (hx, hy) in pts {
        let hrect = NSRect::new(NSPoint::new(hx - 3.5, hy - 3.5), NSSize::new(7.0, 7.0));
        let dot = NSBezierPath::bezierPathWithOvalInRect(hrect);
        border_color.setFill();
        dot.fill();
        halo.setStroke();
        dot.setLineWidth(1.0);
        dot.stroke();
    }
}
