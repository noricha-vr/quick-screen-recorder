use super::*;

#[test]
fn coordinate_flip_round_trip() {
    let screen_h = 1440.0;
    let appkit = Rect::new(100.0, 240.0, 800.0, 600.0);
    let cg = appkit.appkit_to_top_left(screen_h);
    assert_eq!(cg, Rect::new(100.0, 600.0, 800.0, 600.0));
    let back = cg.top_left_to_appkit(screen_h);
    assert_eq!(back, appkit);
}

#[test]
fn clamp_and_even_alignment() {
    let bounds = Rect::new(0.0, 0.0, 1920.0, 1080.0);
    let odd = Rect::new(1800.0, -25.0, 301.0, 205.0).clamp_and_align_even(bounds);
    assert_eq!(odd.width as u32 % 2, 0);
    assert_eq!(odd.height as u32 % 2, 0);
    assert!(odd.right() <= bounds.right());
    assert_eq!(odd.y, 0.0);
}

#[test]
fn aspect_ratio_3x2_lock() {
    let bounds = Rect::new(0.0, 0.0, 2560.0, 1440.0);
    let r = drag_rect_from_points(100.0, 100.0, 700.0, 350.0, AspectRatio::R3x2, bounds);
    let actual_ratio = r.width / r.height;
    assert!(
        (actual_ratio - 1.5).abs() < 0.02,
        "ratio was {actual_ratio}"
    );
}

#[test]
fn hit_test_handles_and_move() {
    let rect = Rect::new(100.0, 100.0, 400.0, 300.0);
    assert_eq!(
        hit_test_selection(rect, 100.0, 400.0, 8.0),
        Some(DragMode::ResizeTopLeft)
    );
    assert_eq!(
        hit_test_selection(rect, 500.0, 100.0, 8.0),
        Some(DragMode::ResizeBottomRight)
    );
    assert_eq!(
        hit_test_selection(rect, 250.0, 250.0, 8.0),
        Some(DragMode::Move)
    );
    assert_eq!(hit_test_selection(rect, 20.0, 20.0, 8.0), None);
}

#[test]
fn scaled_dimensions_keep_even_aspect() {
    assert_eq!(scaled_even_dimensions(2560, 1440, Some(1280)), (1280, 720));
    assert_eq!(scaled_even_dimensions(801, 601, None), (800, 600));
    assert_eq!(scaled_even_dimensions(640, 480, Some(1280)), (640, 480));
}

#[test]
fn retina_2x_pixel_alignment_preserves_800x450() {
    let bounds = Rect::new(0.0, 0.0, 1440.0, 900.0);
    let r =
        Rect::new(100.0, 100.0, 800.0 / 2.0, 450.0 / 2.0).clamp_and_align_for_scale(bounds, 2.0);
    assert_eq!((r.width * 2.0).round() as u32, 800);
    assert_eq!((r.height * 2.0).round() as u32, 450);

    let moved = apply_drag(
        r,
        DragMode::Move,
        200.0,
        200.0,
        320.0,
        160.0,
        AspectRatio::Free,
        bounds,
    );
    assert_eq!(moved, Rect::new(220.0, 60.0, 400.0, 225.0));
}

#[test]
fn window_hit_test_and_overhang_projection() {
    let frames = [
        Rect::new(300.0, 120.0, 500.0, 360.0),
        Rect::new(100.0, 80.0, 600.0, 500.0),
    ];
    assert_eq!(
        hit_test_window_frames(&frames, (0.0, 0.0), 900.0, 400.0, 600.0, None),
        Some(0)
    );
    assert_eq!(
        hit_test_window_frames(&frames, (0.0, 0.0), 900.0, 400.0, 600.0, Some(0)),
        Some(1)
    );
    assert_eq!(
        hit_test_window_frames(&frames, (0.0, 0.0), 900.0, 180.0, 600.0, Some(0)),
        Some(1)
    );
    assert_eq!(
        hit_test_window_frames(&frames, (0.0, 0.0), 900.0, 50.0, 50.0, None),
        None
    );

    let bounds = Rect::new(0.0, 0.0, 1440.0, 900.0);
    let proj =
        window_frame_to_overlay_rect(Rect::new(-100.0, 600.0, 600.0, 400.0), (0.0, 0.0), bounds);
    assert_eq!(proj, Some(Rect::new(0.0, 0.0, 500.0, 300.0)));
    assert_eq!(
        window_frame_to_overlay_rect(Rect::new(1920.0, 100.0, 800.0, 600.0), (0.0, 0.0), bounds),
        None
    );
}

#[test]
fn toolbar_follows_below_flips_above_and_respects_dock_and_multi_display() {
    let screen = Rect::new(0.0, 0.0, 1440.0, 900.0);
    // 70pt Dock at bottom, 25pt Menu Bar at top -> visible y: 70..875
    let visible = Rect::new(0.0, 70.0, 1440.0, 805.0);
    let (bw, bh) = (588.0, 88.0);

    // 1. Centered selection with room below -> directly below by 12pt
    let mid = Rect::new(320.0, 240.0, 800.0, 460.0);
    let (x, y) = compute_toolbar_origin(mid, screen, visible, bw, bh);
    assert_eq!(x, 426.0);
    assert_eq!(y, 140.0);

    // 2. Selection near bottom (Dock blocks below) -> flips cleanly above by 12pt
    let low = Rect::new(320.0, 80.0, 800.0, 420.0);
    let (x2, y2) = compute_toolbar_origin(low, screen, visible, bw, bh);
    assert_eq!(x2, 426.0);
    assert_eq!(y2, 512.0);

    // 3. Fullscreen / maximized selection -> floats safely 20pt above Dock
    let full = Rect::new(0.0, 0.0, 1440.0, 900.0);
    let (x3, y3) = compute_toolbar_origin(full, screen, visible, bw, bh);
    assert_eq!(x3, 426.0);
    assert_eq!(y3, 90.0);

    // 4. Narrow selection near far-left edge -> clamps X inside visible frame margin
    let left_edge = Rect::new(10.0, 260.0, 200.0, 200.0);
    let (x4, y4) = compute_toolbar_origin(left_edge, screen, visible, bw, bh);
    assert_eq!(x4, 12.0);
    assert_eq!(y4, 160.0);

    // 5. Secondary display with non-zero global origin
    let sec_screen = Rect::new(1440.0, 100.0, 1920.0, 1080.0);
    let sec_visible = Rect::new(1440.0, 100.0, 1920.0, 1055.0);
    let sec_sel = Rect::new(400.0, 300.0, 900.0, 500.0);
    let (x5, y5) = compute_toolbar_origin(sec_sel, sec_screen, sec_visible, bw, bh);
    assert_eq!(x5, 1440.0 + 850.0 - 294.0);
    assert_eq!(y5, 100.0 + 300.0 - 12.0 - 88.0);
}
