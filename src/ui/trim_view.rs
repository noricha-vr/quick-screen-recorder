use std::cell::RefCell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBezierPath, NSColor, NSCursor, NSEvent, NSFont, NSTextAlignment, NSTextField, NSView,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

pub const MIN_TRIM_GAP_SEC: f64 = 0.05;
const HANDLE_WIDTH: f64 = 14.0;
const TRACK_INSET_X: f64 = 16.0;
const TRACK_Y: f64 = 2.0;
const TRACK_H: f64 = 32.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimDragTarget {
    StartBracket,
    EndBracket,
    Playhead,
}

#[derive(Debug, Clone, Copy)]
struct TrimTrackState {
    duration_sec: f64,
    start_sec: f64,
    end_sec: f64,
    playhead_sec: f64,
    video_w: u32,
    video_h: u32,
    active_drag: Option<TrimDragTarget>,
}

pub type TrimChangeCallback = Rc<dyn Fn(f64, f64, f64)>;

pub struct TrimTimelineIvars {
    state: RefCell<TrimTrackState>,
    in_label: Retained<NSTextField>,
    clip_label: Retained<NSTextField>,
    out_label: Retained<NSTextField>,
    on_change: TrimChangeCallback,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "QSRTrimTimelineView"]
    #[ivars = TrimTimelineIvars]
    pub struct TrimTimelineView;

    unsafe impl NSObjectProtocol for TrimTimelineView {}

    impl TrimTimelineView {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let st = *self.ivars().state.borrow();
            let bounds = self.bounds();
            let track_w = (bounds.size.width - TRACK_INSET_X * 2.0).max(40.0);
            let dur = st.duration_sec.max(MIN_TRIM_GAP_SEC);

            let start_x = time_to_x(st.start_sec, dur, TRACK_INSET_X, track_w);
            let end_x = time_to_x(st.end_sec, dur, TRACK_INSET_X, track_w);
            let play_x = time_to_x(st.playhead_sec, dur, TRACK_INSET_X, track_w);

            // 1. Recessed dark timeline trough
            let outer_rect = NSRect::new(
                NSPoint::new(TRACK_INSET_X - HANDLE_WIDTH, TRACK_Y),
                NSSize::new(track_w + HANDLE_WIDTH * 2.0, TRACK_H),
            );
            let trough_bg = NSColor::colorWithSRGBRed_green_blue_alpha(0.05, 0.06, 0.09, 1.0);
            trough_bg.setFill();
            let trough_path =
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(outer_rect, 6.0, 6.0);
            trough_path.fill();

            let trough_border = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.10);
            trough_border.setStroke();
            trough_path.setLineWidth(1.0);
            trough_path.stroke();

            // 2. Time-ruler tick marks across full duration
            let tick_count = 28_usize;
            for i in 0..=tick_count {
                let tx = TRACK_INSET_X + (i as f64 / tick_count as f64) * track_w;
                let major = i % 4 == 0;
                let th = if major { 14.0 } else { 8.0 };
                let ty = TRACK_Y + (TRACK_H - th) * 0.5;
                let alpha = if major { 0.24 } else { 0.12 };
                let tc = NSColor::colorWithSRGBRed_green_blue_alpha(0.80, 0.85, 0.95, alpha);
                tc.setStroke();
                let line = NSBezierPath::bezierPath();
                line.moveToPoint(NSPoint::new(tx, ty));
                line.lineToPoint(NSPoint::new(tx, ty + th));
                line.setLineWidth(1.0);
                line.stroke();
            }

            // 3. Dimmed trimmed-out zones (left of IN, right of OUT)
            let cut_dim = NSColor::colorWithSRGBRed_green_blue_alpha(0.02, 0.03, 0.05, 0.72);
            cut_dim.setFill();
            if start_x > TRACK_INSET_X {
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(TRACK_INSET_X, TRACK_Y + 1.0),
                    NSSize::new(start_x - TRACK_INSET_X, TRACK_H - 2.0),
                ));
            }
            let track_end_x = TRACK_INSET_X + track_w;
            if end_x < track_end_x {
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(end_x, TRACK_Y + 1.0),
                    NSSize::new(track_end_x - end_x, TRACK_H - 2.0),
                ));
            }

            // 4. Highlighted active clip interior
            if end_x > start_x {
                let active_fill =
                    NSColor::colorWithSRGBRed_green_blue_alpha(0.96, 0.76, 0.18, 0.18);
                active_fill.setFill();
                NSBezierPath::fillRect(NSRect::new(
                    NSPoint::new(start_x, TRACK_Y + 2.5),
                    NSSize::new(end_x - start_x, TRACK_H - 5.0),
                ));
            }

            // 5. QuickTime-style amber/gold top & bottom frame bars + rectangular [ IN / OUT ] brackets
            let amber = NSColor::colorWithSRGBRed_green_blue_alpha(0.96, 0.76, 0.18, 1.0);
            amber.setFill();

            let span_x = start_x - HANDLE_WIDTH;
            let span_w = (end_x + HANDLE_WIDTH) - span_x;
            NSBezierPath::fillRect(NSRect::new(
                NSPoint::new(span_x + 4.0, TRACK_Y + TRACK_H - 2.5),
                NSSize::new((span_w - 8.0).max(4.0), 2.5),
            ));
            NSBezierPath::fillRect(NSRect::new(
                NSPoint::new(span_x + 4.0, TRACK_Y),
                NSSize::new((span_w - 8.0).max(4.0), 2.5),
            ));

            draw_bracket_handle(start_x - HANDLE_WIDTH, TRACK_Y, HANDLE_WIDTH, TRACK_H, &amber);
            draw_bracket_handle(end_x, TRACK_Y, HANDLE_WIDTH, TRACK_H, &amber);

            // 6. Crisp vertical playhead indicator
            let play_color = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.96);
            play_color.setFill();
            NSBezierPath::fillRect(NSRect::new(
                NSPoint::new(play_x - 1.0, TRACK_Y - 1.0),
                NSSize::new(2.0, TRACK_H + 3.0),
            ));
            NSBezierPath::fillRect(NSRect::new(
                NSPoint::new(play_x - 3.5, TRACK_Y + TRACK_H - 2.0),
                NSSize::new(7.0, 4.0),
            ));
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let loc = self.convertPoint_fromView(event.locationInWindow(), None);
            let st = *self.ivars().state.borrow();
            let track_w = (self.bounds().size.width - TRACK_INSET_X * 2.0).max(40.0);
            match hit_test_trim_timeline(
                loc.x,
                TRACK_INSET_X,
                track_w,
                st.start_sec,
                st.end_sec,
                st.duration_sec,
            ) {
                TrimDragTarget::StartBracket | TrimDragTarget::EndBracket => {
                    set_horizontal_resize_cursor();
                }
                TrimDragTarget::Playhead => {
                    NSCursor::pointingHandCursor().set();
                }
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let loc = self.convertPoint_fromView(event.locationInWindow(), None);
            let track_w = (self.bounds().size.width - TRACK_INSET_X * 2.0).max(40.0);
            let target = {
                let st = *self.ivars().state.borrow();
                hit_test_trim_timeline(
                    loc.x,
                    TRACK_INSET_X,
                    track_w,
                    st.start_sec,
                    st.end_sec,
                    st.duration_sec,
                )
            };
            self.ivars().state.borrow_mut().active_drag = Some(target);
            self.apply_pointer_x(loc.x, target);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let loc = self.convertPoint_fromView(event.locationInWindow(), None);
            let target = self.ivars().state.borrow().active_drag;
            if let Some(t) = target {
                self.apply_pointer_x(loc.x, t);
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            self.ivars().state.borrow_mut().active_drag = None;
        }
    }
);

impl TrimTimelineView {
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        duration_sec: f64,
        video_w: u32,
        video_h: u32,
        on_change: TrimChangeCallback,
    ) -> Retained<Self> {
        let safe_dur = duration_sec.max(MIN_TRIM_GAP_SEC);
        let amber_text = NSColor::colorWithSRGBRed_green_blue_alpha(0.98, 0.80, 0.26, 1.0);
        let center_text = NSColor::colorWithSRGBRed_green_blue_alpha(0.86, 0.89, 0.95, 1.0);
        let mono_bold = NSFont::monospacedDigitSystemFontOfSize_weight(12.0, 0.35);
        let mono_reg = NSFont::monospacedDigitSystemFontOfSize_weight(12.0, 0.15);

        let in_label = NSTextField::labelWithString(&NSString::from_str("IN  0.00s"), mtm);
        in_label.setFont(Some(&mono_bold));
        in_label.setTextColor(Some(&amber_text));
        in_label.setFrame(NSRect::new(
            NSPoint::new(2.0, 38.0),
            NSSize::new(150.0, 18.0),
        ));

        let clip_label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        clip_label.setFont(Some(&mono_reg));
        clip_label.setTextColor(Some(&center_text));
        clip_label.setAlignment(NSTextAlignment::Center);
        clip_label.setFrame(NSRect::new(
            NSPoint::new(160.0, 38.0),
            NSSize::new((frame.size.width - 320.0).max(120.0), 18.0),
        ));

        let out_label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        out_label.setFont(Some(&mono_bold));
        out_label.setTextColor(Some(&amber_text));
        out_label.setAlignment(NSTextAlignment::Right);
        out_label.setFrame(NSRect::new(
            NSPoint::new((frame.size.width - 152.0).max(0.0), 38.0),
            NSSize::new(150.0, 18.0),
        ));

        let this = Self::alloc(mtm).set_ivars(TrimTimelineIvars {
            state: RefCell::new(TrimTrackState {
                duration_sec: safe_dur,
                start_sec: 0.0,
                end_sec: safe_dur,
                playhead_sec: 0.0,
                video_w,
                video_h,
                active_drag: None,
            }),
            in_label: in_label.clone(),
            clip_label: clip_label.clone(),
            out_label: out_label.clone(),
            on_change,
        });
        let view: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        view.addSubview(&in_label);
        view.addSubview(&clip_label);
        view.addSubview(&out_label);
        view.refresh_labels();
        view
    }

    pub fn set_trim_range(&self, start_sec: f64, end_sec: f64, playhead_sec: f64) {
        {
            let mut st = self.ivars().state.borrow_mut();
            let dur = st.duration_sec.max(MIN_TRIM_GAP_SEC);
            st.start_sec = start_sec.clamp(0.0, (dur - MIN_TRIM_GAP_SEC).max(0.0));
            st.end_sec = end_sec.clamp((st.start_sec + MIN_TRIM_GAP_SEC).min(dur), dur);
            st.playhead_sec = playhead_sec.clamp(st.start_sec, st.end_sec);
        }
        self.refresh_labels();
        self.setNeedsDisplay(true);
    }

    fn apply_pointer_x(&self, pointer_x: f64, target: TrimDragTarget) {
        let track_w = (self.bounds().size.width - TRACK_INSET_X * 2.0).max(40.0);
        let (start, end, preview) = {
            let mut st = self.ivars().state.borrow_mut();
            let t = x_to_time(pointer_x, st.duration_sec, TRACK_INSET_X, track_w);
            let (ns, ne, np) =
                compute_dragged_trim(t, target, st.start_sec, st.end_sec, st.duration_sec);
            st.start_sec = ns;
            st.end_sec = ne;
            st.playhead_sec = np;
            (ns, ne, np)
        };
        if matches!(
            target,
            TrimDragTarget::StartBracket | TrimDragTarget::EndBracket
        ) {
            set_horizontal_resize_cursor();
        }
        self.refresh_labels();
        self.setNeedsDisplay(true);
        (self.ivars().on_change)(start, end, preview);
    }

    fn refresh_labels(&self) {
        let st = *self.ivars().state.borrow();
        let clip_len = (st.end_sec - st.start_sec).max(0.0);
        self.ivars()
            .in_label
            .setStringValue(&NSString::from_str(&format!("IN  {:.2}s", st.start_sec)));
        self.ivars()
            .clip_label
            .setStringValue(&NSString::from_str(&format!(
                "{}×{}  ·  Selected {:.2}s",
                st.video_w, st.video_h, clip_len
            )));
        self.ivars()
            .out_label
            .setStringValue(&NSString::from_str(&format!("OUT  {:.2}s", st.end_sec)));
    }
}

#[allow(deprecated)]
fn set_horizontal_resize_cursor() {
    NSCursor::resizeLeftRightCursor().set();
}

fn draw_bracket_handle(x: f64, y: f64, w: f64, h: f64, fill: &NSColor) {
    fill.setFill();
    let rect = NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, 4.5, 4.5).fill();

    let grip = NSColor::colorWithSRGBRed_green_blue_alpha(0.10, 0.08, 0.03, 0.78);
    grip.setFill();
    let slit_h = 14.0;
    let sy = y + (h - slit_h) * 0.5;
    for offset in [-2.2, 1.0] {
        NSBezierPath::fillRect(NSRect::new(
            NSPoint::new(x + w * 0.5 + offset, sy),
            NSSize::new(1.4, slit_h),
        ));
    }
}

pub fn time_to_x(time_sec: f64, duration_sec: f64, track_x0: f64, track_w: f64) -> f64 {
    let dur = duration_sec.max(MIN_TRIM_GAP_SEC);
    track_x0 + (time_sec.clamp(0.0, dur) / dur) * track_w
}

pub fn x_to_time(px: f64, duration_sec: f64, track_x0: f64, track_w: f64) -> f64 {
    let dur = duration_sec.max(MIN_TRIM_GAP_SEC);
    let ratio = ((px - track_x0) / track_w.max(1.0)).clamp(0.0, 1.0);
    ratio * dur
}

pub fn hit_test_trim_timeline(
    px: f64,
    track_x0: f64,
    track_w: f64,
    start_sec: f64,
    end_sec: f64,
    duration_sec: f64,
) -> TrimDragTarget {
    let start_x = time_to_x(start_sec, duration_sec, track_x0, track_w);
    let end_x = time_to_x(end_sec, duration_sec, track_x0, track_w);

    let in_left_zone = px >= start_x - HANDLE_WIDTH - 6.0 && px <= start_x + 6.0;
    let in_right_zone = px >= end_x - 6.0 && px <= end_x + HANDLE_WIDTH + 6.0;

    if in_left_zone && in_right_zone {
        if px <= (start_x + end_x) * 0.5 {
            TrimDragTarget::StartBracket
        } else {
            TrimDragTarget::EndBracket
        }
    } else if in_left_zone || px < start_x - HANDLE_WIDTH - 6.0 {
        TrimDragTarget::StartBracket
    } else if in_right_zone || px > end_x + HANDLE_WIDTH + 6.0 {
        TrimDragTarget::EndBracket
    } else {
        TrimDragTarget::Playhead
    }
}

pub fn compute_dragged_trim(
    pointer_time: f64,
    target: TrimDragTarget,
    start_sec: f64,
    end_sec: f64,
    duration_sec: f64,
) -> (f64, f64, f64) {
    let dur = duration_sec.max(MIN_TRIM_GAP_SEC);
    match target {
        TrimDragTarget::StartBracket => {
            let max_start = (end_sec - MIN_TRIM_GAP_SEC).max(0.0);
            let next_start = pointer_time.clamp(0.0, max_start);
            (next_start, end_sec, next_start)
        }
        TrimDragTarget::EndBracket => {
            let min_end = (start_sec + MIN_TRIM_GAP_SEC).min(dur);
            let next_end = pointer_time.clamp(min_end, dur);
            (start_sec, next_end, next_end)
        }
        TrimDragTarget::Playhead => {
            let next_play = pointer_time.clamp(start_sec, end_sec);
            (start_sec, end_sec, next_play)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_brackets_hit_test_and_clamp_both_start_and_end() {
        let track_x0 = 16.0;
        let track_w = 600.0;
        let dur = 10.0;
        // Start at 2.0s (x = 136), End at 8.0s (x = 496)
        assert_eq!(
            hit_test_trim_timeline(130.0, track_x0, track_w, 2.0, 8.0, dur),
            TrimDragTarget::StartBracket
        );
        assert_eq!(
            hit_test_trim_timeline(502.0, track_x0, track_w, 2.0, 8.0, dur),
            TrimDragTarget::EndBracket
        );
        assert_eq!(
            hit_test_trim_timeline(300.0, track_x0, track_w, 2.0, 8.0, dur),
            TrimDragTarget::Playhead
        );
        // Clicking in excluded left region jumps StartBracket
        assert_eq!(
            hit_test_trim_timeline(40.0, track_x0, track_w, 2.0, 8.0, dur),
            TrimDragTarget::StartBracket
        );

        // StartBracket cannot cross EndBracket - MIN_TRIM_GAP_SEC
        let (s, e, p) = compute_dragged_trim(9.5, TrimDragTarget::StartBracket, 2.0, 8.0, dur);
        assert!((s - 7.95).abs() < 1e-6);
        assert_eq!(e, 8.0);
        assert_eq!(p, s);

        // EndBracket cannot cross StartBracket + MIN_TRIM_GAP_SEC
        let (s2, e2, p2) = compute_dragged_trim(0.5, TrimDragTarget::EndBracket, 2.0, 8.0, dur);
        assert_eq!(s2, 2.0);
        assert!((e2 - 2.05).abs() < 1e-6);
        assert_eq!(p2, e2);
    }
}
