use serde::{Deserialize, Serialize};

pub const MIN_CAPTURE_DIMENSION: f64 = 32.0;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> f64 {
        self.x + self.width
    }
    pub fn top_y_up(&self) -> f64 {
        self.y + self.height
    }
    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && px <= self.x + self.width && py >= self.y && py <= self.y + self.height
    }
    /// Converts AppKit screen-local coordinates (bottom-left, Y-up) into top-left (Y-down).
    pub fn appkit_to_top_left(&self, display_height: f64) -> Self {
        let y = (display_height - (self.y + self.height)).max(0.0);
        Self::new(
            self.x.max(0.0),
            y,
            self.width.max(MIN_CAPTURE_DIMENSION),
            self.height.max(MIN_CAPTURE_DIMENSION),
        )
    }
    /// Converts top-left (CoreGraphics) display-local coordinates into AppKit bottom-left coordinates.
    pub fn top_left_to_appkit(&self, display_height: f64) -> Self {
        let y = (display_height - (self.y + self.height)).max(0.0);
        Self::new(
            self.x.max(0.0),
            y,
            self.width.max(MIN_CAPTURE_DIMENSION),
            self.height.max(MIN_CAPTURE_DIMENSION),
        )
    }

    /// Clamps rectangle inside `bounds` and aligns dimensions so `dim * scale_factor` is an even pixel integer.
    pub fn clamp_and_align_for_scale(&self, bounds: Rect, scale_factor: f64) -> Self {
        let sf = if scale_factor >= 1.0 {
            scale_factor
        } else {
            1.0
        };
        let max_px_w = round_down_even((bounds.width * sf).max(MIN_CAPTURE_DIMENSION));
        let max_px_h = round_down_even((bounds.height * sf).max(MIN_CAPTURE_DIMENSION));
        let target_px_w =
            round_nearest_even((self.width * sf).clamp(MIN_CAPTURE_DIMENSION, max_px_w))
                .min(max_px_w);
        let target_px_h =
            round_nearest_even((self.height * sf).clamp(MIN_CAPTURE_DIMENSION, max_px_h))
                .min(max_px_h);
        let w = target_px_w / sf;
        let h = target_px_h / sf;
        let x = self
            .x
            .clamp(bounds.x, (bounds.right() - w).max(bounds.x))
            .round();
        let y = self
            .y
            .clamp(bounds.y, (bounds.top_y_up() - h).max(bounds.y))
            .round();
        Self {
            x,
            y,
            width: w,
            height: h,
        }
    }

    /// Clamps rectangle inside `bounds` and rounds dimensions to even numbers for hardware video encoders.
    pub fn clamp_and_align_even(&self, bounds: Rect) -> Self {
        self.clamp_and_align_for_scale(bounds, 1.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AspectRatio {
    #[default]
    Free,
    R16x9,
    R16x10,
    R4x3,
    R3x2,
    R1x1,
    R9x16,
}

impl AspectRatio {
    pub const ALL: [Self; 7] = [
        Self::Free,
        Self::R16x9,
        Self::R16x10,
        Self::R4x3,
        Self::R3x2,
        Self::R1x1,
        Self::R9x16,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Free => "Free",
            Self::R16x9 => "16:9",
            Self::R16x10 => "16:10",
            Self::R4x3 => "4:3",
            Self::R3x2 => "3:2",
            Self::R1x1 => "1:1",
            Self::R9x16 => "9:16",
        }
    }

    pub fn ratio(self) -> Option<f64> {
        match self {
            Self::Free => None,
            Self::R16x9 => Some(16.0 / 9.0),
            Self::R16x10 => Some(16.0 / 10.0),
            Self::R4x3 => Some(4.0 / 3.0),
            Self::R3x2 => Some(3.0 / 2.0),
            Self::R1x1 => Some(1.0),
            Self::R9x16 => Some(9.0 / 16.0),
        }
    }

    pub fn from_index(index: usize) -> Self {
        Self::ALL.get(index).copied().unwrap_or(Self::Free)
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|&r| r == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragMode {
    Create,
    Move,
    ResizeTopLeft,
    ResizeTop,
    ResizeTopRight,
    ResizeRight,
    ResizeBottomRight,
    ResizeBottom,
    ResizeBottomLeft,
    ResizeLeft,
}

pub fn hit_test_selection(rect: Rect, px: f64, py: f64, handle_radius: f64) -> Option<DragMode> {
    let corners = [
        (rect.x, rect.top_y_up(), DragMode::ResizeTopLeft),
        (rect.right(), rect.top_y_up(), DragMode::ResizeTopRight),
        (rect.right(), rect.y, DragMode::ResizeBottomRight),
        (rect.x, rect.y, DragMode::ResizeBottomLeft),
    ];
    for (cx, cy, mode) in corners {
        if (px - cx).abs() <= handle_radius && (py - cy).abs() <= handle_radius {
            return Some(mode);
        }
    }

    let on_horiz_span = px >= rect.x - handle_radius && px <= rect.right() + handle_radius;
    let on_vert_span = py >= rect.y - handle_radius && py <= rect.top_y_up() + handle_radius;

    if on_horiz_span && (py - rect.top_y_up()).abs() <= handle_radius {
        return Some(DragMode::ResizeTop);
    }
    if on_horiz_span && (py - rect.y).abs() <= handle_radius {
        return Some(DragMode::ResizeBottom);
    }
    if on_vert_span && (px - rect.right()).abs() <= handle_radius {
        return Some(DragMode::ResizeRight);
    }
    if on_vert_span && (px - rect.x).abs() <= handle_radius {
        return Some(DragMode::ResizeLeft);
    }
    if rect.contains(px, py) {
        return Some(DragMode::Move);
    }
    None
}

pub fn drag_rect_from_points(
    anchor_x: f64,
    anchor_y: f64,
    current_x: f64,
    current_y: f64,
    aspect: AspectRatio,
    bounds: Rect,
) -> Rect {
    let dx = current_x - anchor_x;
    let dy = current_y - anchor_y;
    let mut w = dx.abs().max(MIN_CAPTURE_DIMENSION);
    let mut h = dy.abs().max(MIN_CAPTURE_DIMENSION);

    if let Some(r) = aspect.ratio() {
        if w / h > r {
            h = (w / r).max(MIN_CAPTURE_DIMENSION);
        } else {
            w = (h * r).max(MIN_CAPTURE_DIMENSION);
        }
        if w > bounds.width {
            w = bounds.width;
            h = w / r;
        }
        if h > bounds.height {
            h = bounds.height;
            w = h * r;
        }
    }

    let x = if dx >= 0.0 { anchor_x } else { anchor_x - w };
    let y = if dy >= 0.0 { anchor_y } else { anchor_y - h };

    Rect::new(x, y, w, h).clamp_and_align_even(bounds)
}

#[allow(clippy::too_many_arguments)]
pub fn apply_drag(
    initial: Rect,
    mode: DragMode,
    start_x: f64,
    start_y: f64,
    cur_x: f64,
    cur_y: f64,
    aspect: AspectRatio,
    bounds: Rect,
) -> Rect {
    let dx = cur_x - start_x;
    let dy = cur_y - start_y;

    match mode {
        DragMode::Create => drag_rect_from_points(start_x, start_y, cur_x, cur_y, aspect, bounds),
        DragMode::Move => {
            let max_w = bounds.width.max(MIN_CAPTURE_DIMENSION);
            let max_h = bounds.height.max(MIN_CAPTURE_DIMENSION);
            let w = initial.width.clamp(MIN_CAPTURE_DIMENSION, max_w);
            let h = initial.height.clamp(MIN_CAPTURE_DIMENSION, max_h);
            let x = (initial.x + dx)
                .clamp(bounds.x, (bounds.right() - w).max(bounds.x))
                .round();
            let y = (initial.y + dy)
                .clamp(bounds.y, (bounds.top_y_up() - h).max(bounds.y))
                .round();
            Rect::new(x, y, w, h)
        }
        DragMode::ResizeBottomRight => {
            drag_rect_from_points(initial.x, initial.top_y_up(), cur_x, cur_y, aspect, bounds)
        }
        DragMode::ResizeTopLeft => {
            drag_rect_from_points(initial.right(), initial.y, cur_x, cur_y, aspect, bounds)
        }
        DragMode::ResizeTopRight => {
            drag_rect_from_points(initial.x, initial.y, cur_x, cur_y, aspect, bounds)
        }
        DragMode::ResizeBottomLeft => drag_rect_from_points(
            initial.right(),
            initial.top_y_up(),
            cur_x,
            cur_y,
            aspect,
            bounds,
        ),
        DragMode::ResizeRight => {
            let new_w = (initial.width + dx).max(MIN_CAPTURE_DIMENSION);
            let new_h = aspect.ratio().map_or(initial.height, |r| new_w / r);
            Rect::new(initial.x, initial.y, new_w, new_h).clamp_and_align_even(bounds)
        }
        DragMode::ResizeLeft => {
            let new_w = (initial.width - dx).max(MIN_CAPTURE_DIMENSION);
            let new_h = aspect.ratio().map_or(initial.height, |r| new_w / r);
            Rect::new(initial.right() - new_w, initial.y, new_w, new_h).clamp_and_align_even(bounds)
        }
        DragMode::ResizeTop => {
            let new_h = (initial.height + dy).max(MIN_CAPTURE_DIMENSION);
            let new_w = aspect.ratio().map_or(initial.width, |r| new_h * r);
            Rect::new(initial.x, initial.y, new_w, new_h).clamp_and_align_even(bounds)
        }
        DragMode::ResizeBottom => {
            let new_h = (initial.height - dy).max(MIN_CAPTURE_DIMENSION);
            let new_w = aspect.ratio().map_or(initial.width, |r| new_h * r);
            Rect::new(initial.x, initial.top_y_up() - new_h, new_w, new_h)
                .clamp_and_align_even(bounds)
        }
    }
}

pub fn apply_aspect_to_rect(rect: Rect, aspect: AspectRatio, bounds: Rect) -> Rect {
    let Some(r) = aspect.ratio() else {
        return rect.clamp_and_align_even(bounds);
    };
    let center_x = rect.x + rect.width * 0.5;
    let center_y = rect.y + rect.height * 0.5;
    let mut w = rect.width;
    let mut h = (w / r).max(MIN_CAPTURE_DIMENSION);
    if h > bounds.height {
        h = bounds.height;
        w = h * r;
    }
    if w > bounds.width {
        w = bounds.width;
        h = w / r;
    }
    Rect::new(center_x - w * 0.5, center_y - h * 0.5, w, h).clamp_and_align_even(bounds)
}

pub fn scaled_even_dimensions(src_w: u32, src_h: u32, max_width: Option<u32>) -> (u32, u32) {
    let base_w = src_w.max(2);
    let base_h = src_h.max(2);
    let (w, h) = match max_width {
        Some(limit) if limit > 0 && base_w > limit => {
            let scale = f64::from(limit) / f64::from(base_w);
            let target_h = ((f64::from(base_h) * scale).round() as u32).max(2);
            (limit, target_h)
        }
        _ => (base_w, base_h),
    };
    ((w & !1).max(2), (h & !1).max(2))
}

/// Hit-tests ScreenCaptureKit windows (ordered front-to-back in CoreGraphics global top-left coordinates)
/// from an AppKit overlay-local point `(local_x, local_y)` (origin at bottom-left, Y-up).
/// Selects the frontmost window at that point, or cycles through overlapping windows if already selected.
pub fn hit_test_window_frames(
    window_frames: &[Rect],
    display_origin: (f64, f64),
    overlay_height: f64,
    local_x: f64,
    local_y: f64,
    current_selected: Option<usize>,
) -> Option<usize> {
    let cg_x = display_origin.0 + local_x;
    let cg_y = display_origin.1 + (overlay_height - local_y);
    let hits: Vec<usize> = window_frames
        .iter()
        .enumerate()
        .filter_map(|(i, f)| f.contains(cg_x, cg_y).then_some(i))
        .collect();
    let first = *hits.first()?;
    if let Some(cur) = current_selected {
        if let Some(pos) = hits.iter().position(|&idx| idx == cur) {
            return Some(hits[(pos + 1) % hits.len()]);
        }
    }
    Some(first)
}

/// Projects a ScreenCaptureKit global top-left window frame onto the active display's AppKit overlay bounds,
/// clipping any off-screen overhang. Returns `None` if the window does not intersect this display.
pub fn window_frame_to_overlay_rect(
    win_frame: Rect,
    display_origin: (f64, f64),
    overlay_bounds: Rect,
) -> Option<Rect> {
    let lx0 = win_frame.x - display_origin.0;
    let ly0 = win_frame.y - display_origin.1;
    let lx1 = lx0 + win_frame.width;
    let ly1 = ly0 + win_frame.height;

    let cx0 = lx0.clamp(0.0, overlay_bounds.width);
    let cy0 = ly0.clamp(0.0, overlay_bounds.height);
    let cx1 = lx1.clamp(0.0, overlay_bounds.width);
    let cy1 = ly1.clamp(0.0, overlay_bounds.height);

    let vis_w = cx1 - cx0;
    let vis_h = cy1 - cy0;
    if vis_w < 1.0 || vis_h < 1.0 {
        return None;
    }
    let appkit_y =
        (overlay_bounds.height - cy1).clamp(0.0, (overlay_bounds.height - vis_h).max(0.0));
    Some(Rect::new(cx0, appkit_y, vis_w, vis_h))
}

fn round_down_even(v: f64) -> f64 {
    let n = v.floor() as i64;
    (n & !1).max(2) as f64
}

fn round_nearest_even(v: f64) -> f64 {
    let n = (v / 2.0).round() as i64 * 2;
    n.max(2) as f64
}

#[cfg(test)]
#[path = "geometry_tests.rs"]
mod tests;
