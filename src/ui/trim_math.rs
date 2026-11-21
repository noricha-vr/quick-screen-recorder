pub const MIN_TRIM_GAP_SEC: f64 = 0.05;
pub const HANDLE_WIDTH: f64 = 14.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimDragTarget {
    StartBracket,
    EndBracket,
    Playhead,
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

pub fn begin_trim_drag(
    pointer_x: f64,
    track_x0: f64,
    track_w: f64,
    start_sec: f64,
    end_sec: f64,
    duration_sec: f64,
) -> (TrimDragTarget, f64, f64, f64, f64) {
    let start_x = time_to_x(start_sec, duration_sec, track_x0, track_w);
    let end_x = time_to_x(end_sec, duration_sec, track_x0, track_w);
    let in_left_zone = pointer_x >= start_x - HANDLE_WIDTH - 6.0 && pointer_x <= start_x + 6.0;
    let in_right_zone = pointer_x >= end_x - 6.0 && pointer_x <= end_x + HANDLE_WIDTH + 6.0;
    let target = hit_test_trim_timeline(
        pointer_x,
        track_x0,
        track_w,
        start_sec,
        end_sec,
        duration_sec,
    );

    // Grabbing directly on a bracket handle records the pointer-to-boundary offset so clicking never shifts IN/OUT
    match target {
        TrimDragTarget::StartBracket if in_left_zone => {
            (target, pointer_x - start_x, start_sec, end_sec, start_sec)
        }
        TrimDragTarget::EndBracket if in_right_zone => {
            (target, pointer_x - end_x, start_sec, end_sec, end_sec)
        }
        _ => {
            let t = x_to_time(pointer_x, duration_sec, track_x0, track_w);
            let (ns, ne, np) = compute_dragged_trim(t, target, start_sec, end_sec, duration_sec);
            (target, 0.0, ns, ne, np)
        }
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
        assert_eq!(
            hit_test_trim_timeline(40.0, track_x0, track_w, 2.0, 8.0, dur),
            TrimDragTarget::StartBracket
        );

        let (s, e, p) = compute_dragged_trim(9.5, TrimDragTarget::StartBracket, 2.0, 8.0, dur);
        assert!((s - 7.95).abs() < 1e-6);
        assert_eq!(e, 8.0);
        assert_eq!(p, s);

        let (s2, e2, p2) = compute_dragged_trim(0.5, TrimDragTarget::EndBracket, 2.0, 8.0, dur);
        assert_eq!(s2, 2.0);
        assert!((e2 - 2.05).abs() < 1e-6);
        assert_eq!(p2, e2);

        let (tgt, grab_dx, s0, e0, p0) =
            begin_trim_drag(109.0, track_x0, track_w, 100.0, 500.0, 600.0);
        assert_eq!(tgt, TrimDragTarget::StartBracket);
        assert_eq!(s0, 100.0);
        assert_eq!(e0, 500.0);
        assert_eq!(p0, 100.0);
        let moved_t = x_to_time(119.0 - grab_dx, 600.0, track_x0, track_w);
        let (s_drag, _, _) = compute_dragged_trim(moved_t, tgt, s0, e0, 600.0);
        assert!((s_drag - 110.0).abs() < 1e-6);
    }
}
