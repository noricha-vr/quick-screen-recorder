use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBezierPath, NSColor, NSFont, NSFontAttributeName, NSForegroundColorAttributeName,
    NSStringDrawing, NSView,
};
use objc2_foundation::{NSDictionary, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use crate::ui::onboarding_scenes::{draw_menubar_tutorial, draw_permission_animation};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingStep {
    Permission,
    StartRecording,
    RecordingInProgress,
    Completed,
}

impl OnboardingStep {
    pub const fn initial(has_permission: bool) -> Self {
        if has_permission {
            Self::StartRecording
        } else {
            Self::Permission
        }
    }

    /// 0 = Permission, 1 = Record, 2 = Save, 3 = All Done
    pub const fn progress_index(self) -> usize {
        match self {
            Self::Permission => 0,
            Self::StartRecording | Self::RecordingInProgress => 1,
            Self::Completed => 3,
        }
    }
}

pub fn ease_in_out(t: f64) -> f64 {
    let x = t.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

pub fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

pub struct VisualIvars {
    step: Cell<OnboardingStep>,
    phase: Cell<f64>,
    saved_file: RefCell<String>,
    saved_folder: RefCell<String>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "QSRTutorialVisualView"]
    #[ivars = VisualIvars]
    pub struct TutorialVisualView;

    unsafe impl NSObjectProtocol for TutorialVisualView {}

    impl TutorialVisualView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let bounds = self.bounds();
            let card_bg = NSColor::colorWithSRGBRed_green_blue_alpha(0.05, 0.06, 0.09, 1.0);
            card_bg.setFill();
            let outer = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(bounds, 12.0, 12.0);
            outer.fill();

            let border = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.11);
            border.setStroke();
            outer.setLineWidth(1.0);
            outer.stroke();

            let phase = self.ivars().phase.get().rem_euclid(1.0);
            match self.ivars().step.get() {
                OnboardingStep::Permission => draw_permission_animation(bounds, phase),
                OnboardingStep::StartRecording => draw_menubar_tutorial(bounds, phase, false),
                OnboardingStep::RecordingInProgress => draw_menubar_tutorial(bounds, phase, true),
                OnboardingStep::Completed => {
                    let fname = self.ivars().saved_file.borrow();
                    let folder = self.ivars().saved_folder.borrow();
                    draw_completed_visual(bounds, &fname, &folder);
                }
            }
        }
    }
);

impl TutorialVisualView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect, step: OnboardingStep) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(VisualIvars {
            step: Cell::new(step),
            phase: Cell::new(0.0),
            saved_file: RefCell::new(String::new()),
            saved_folder: RefCell::new(String::new()),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    pub fn set_step(&self, step: OnboardingStep, phase: f64) {
        self.ivars().step.set(step);
        self.ivars().phase.set(phase);
        self.setNeedsDisplay(true);
    }

    pub fn set_saved_summary(&self, file_name: String, folder_label: String) {
        *self.ivars().saved_file.borrow_mut() = file_name;
        *self.ivars().saved_folder.borrow_mut() = folder_label;
        self.setNeedsDisplay(true);
    }

    pub fn advance_phase(&self, delta: f64) {
        let next = (self.ivars().phase.get() + delta).rem_euclid(1.0);
        self.ivars().phase.set(next);
        self.setNeedsDisplay(true);
    }
}

pub(crate) fn draw_text(text: &str, x: f64, y: f64, font: &NSFont, color: &NSColor) {
    let ns_text = NSString::from_str(text);
    let keys = [unsafe { NSFontAttributeName }, unsafe {
        NSForegroundColorAttributeName
    }];
    let font_obj: &AnyObject = font.as_ref();
    let color_obj: &AnyObject = color.as_ref();
    let vals = [font_obj, color_obj];
    let attrs = NSDictionary::from_slices(&keys, &vals);
    unsafe {
        ns_text.drawAtPoint_withAttributes(NSPoint::new(x, y), Some(&attrs));
    }
}

pub(crate) fn draw_text_centered(text: &str, rect: NSRect, y: f64, font: &NSFont, color: &NSColor) {
    let ns_text = NSString::from_str(text);
    let keys = [unsafe { NSFontAttributeName }, unsafe {
        NSForegroundColorAttributeName
    }];
    let font_obj: &AnyObject = font.as_ref();
    let color_obj: &AnyObject = color.as_ref();
    let vals = [font_obj, color_obj];
    let attrs = NSDictionary::from_slices(&keys, &vals);
    let size = unsafe { ns_text.sizeWithAttributes(Some(&attrs)) };
    let x = rect.origin.x + (rect.size.width - size.width) * 0.5;
    unsafe {
        ns_text.drawAtPoint_withAttributes(NSPoint::new(x, y), Some(&attrs));
    }
}

pub(crate) fn fill_rounded(rect: NSRect, r: f64, color: &NSColor) {
    color.setFill();
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, r, r).fill();
}

pub(crate) fn stroke_rounded(rect: NSRect, r: f64, width: f64, color: &NSColor) {
    color.setStroke();
    let p = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, r, r);
    p.setLineWidth(width);
    p.stroke();
}

pub(crate) fn draw_cursor_and_ripple(cx: f64, cy: f64, ripple_t: Option<f64>) {
    if let Some(rt) = ripple_t {
        let r = lerp(5.0, 22.0, rt);
        let alpha = (1.0 - rt) * 0.72;
        let ring_rect = NSRect::new(NSPoint::new(cx - r, cy - r), NSSize::new(r * 2.0, r * 2.0));
        let ring = NSBezierPath::bezierPathWithOvalInRect(ring_rect);
        NSColor::colorWithSRGBRed_green_blue_alpha(0.35, 0.72, 1.0, alpha).setStroke();
        ring.setLineWidth(2.2);
        ring.stroke();
    }

    let arrow = NSBezierPath::bezierPath();
    arrow.moveToPoint(NSPoint::new(cx, cy));
    arrow.lineToPoint(NSPoint::new(cx, cy - 17.0));
    arrow.lineToPoint(NSPoint::new(cx + 4.4, cy - 13.0));
    arrow.lineToPoint(NSPoint::new(cx + 8.0, cy - 20.0));
    arrow.lineToPoint(NSPoint::new(cx + 10.6, cy - 18.8));
    arrow.lineToPoint(NSPoint::new(cx + 7.2, cy - 11.8));
    arrow.lineToPoint(NSPoint::new(cx + 12.2, cy - 11.8));
    arrow.closePath();

    NSColor::colorWithSRGBRed_green_blue_alpha(0.06, 0.08, 0.12, 0.95).setFill();
    arrow.fill();
    NSColor::colorWithSRGBRed_green_blue_alpha(0.98, 0.99, 1.0, 1.0).setStroke();
    arrow.setLineWidth(1.5);
    arrow.stroke();
}

fn draw_completed_visual(bounds: NSRect, file_name: &str, folder_label: &str) {
    let cx = bounds.size.width * 0.5;
    let cy = 152.0;

    NSColor::colorWithSRGBRed_green_blue_alpha(0.19, 0.82, 0.35, 0.16).setFill();
    NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
        NSPoint::new(cx - 42.0, cy - 42.0),
        NSSize::new(84.0, 84.0),
    ))
    .fill();
    NSColor::colorWithSRGBRed_green_blue_alpha(0.19, 0.82, 0.35, 0.95).setFill();
    NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
        NSPoint::new(cx - 30.0, cy - 30.0),
        NSSize::new(60.0, 60.0),
    ))
    .fill();

    let check = NSBezierPath::bezierPath();
    check.moveToPoint(NSPoint::new(cx - 13.0, cy - 1.0));
    check.lineToPoint(NSPoint::new(cx - 3.5, cy - 10.5));
    check.lineToPoint(NSPoint::new(cx + 14.0, cy + 9.5));
    NSColor::whiteColor().setStroke();
    check.setLineWidth(3.8);
    check.stroke();

    let pills = ["✓ Permission", "✓ Recorded", "✓ Exported"];
    let pill_w = 118.0;
    let gap = 12.0;
    let total_w = pill_w * 3.0 + gap * 2.0;
    let start_x = (bounds.size.width - total_w) * 0.5;
    let pill_font = NSFont::systemFontOfSize_weight(12.0, 0.25);
    let pill_fg = NSColor::colorWithSRGBRed_green_blue_alpha(0.76, 0.98, 0.82, 1.0);
    for (i, label) in pills.iter().enumerate() {
        let px = start_x + i as f64 * (pill_w + gap);
        let rect = NSRect::new(NSPoint::new(px, 70.0), NSSize::new(pill_w, 26.0));
        fill_rounded(
            rect,
            13.0,
            &NSColor::colorWithSRGBRed_green_blue_alpha(0.08, 0.24, 0.14, 1.0),
        );
        stroke_rounded(
            rect,
            13.0,
            1.0,
            &NSColor::colorWithSRGBRed_green_blue_alpha(0.19, 0.82, 0.35, 0.50),
        );
        draw_text_centered(label, rect, 75.5, &pill_font, &pill_fg);
    }

    if !file_name.is_empty() {
        draw_text_centered(
            file_name,
            bounds,
            37.0,
            &NSFont::monospacedDigitSystemFontOfSize_weight(12.0, 0.25),
            &NSColor::colorWithSRGBRed_green_blue_alpha(0.92, 0.96, 1.0, 1.0),
        );
    }
    if !folder_label.is_empty() {
        draw_text_centered(
            folder_label,
            bounds,
            16.0,
            &NSFont::systemFontOfSize_weight(11.5, 0.1),
            &NSColor::colorWithSRGBRed_green_blue_alpha(0.62, 0.70, 0.82, 1.0),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onboarding_step_starts_on_permission_when_denied_and_record_when_granted() {
        assert_eq!(OnboardingStep::initial(false), OnboardingStep::Permission);
        assert_eq!(
            OnboardingStep::initial(true),
            OnboardingStep::StartRecording
        );
        assert_eq!(OnboardingStep::Permission.progress_index(), 0);
        assert_eq!(OnboardingStep::StartRecording.progress_index(), 1);
        assert_eq!(OnboardingStep::Completed.progress_index(), 3);
    }

    #[test]
    fn easing_and_interpolation_clamp_cleanly() {
        assert_eq!(ease_in_out(-1.0), 0.0);
        assert_eq!(ease_in_out(0.5), 0.5);
        assert_eq!(ease_in_out(2.0), 1.0);
        assert!((lerp(10.0, 30.0, 0.25) - 15.0).abs() < 1e-9);
    }
}
