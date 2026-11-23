use objc2_app_kit::{NSBezierPath, NSColor, NSFont};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use crate::ui::onboarding_visual::{
    draw_cursor_and_ripple, draw_text, draw_text_centered, ease_in_out, fill_rounded, lerp,
    stroke_rounded,
};

pub(crate) fn draw_permission_animation(bounds: NSRect, phase: f64) {
    let panel = NSRect::new(
        NSPoint::new((bounds.size.width - 436.0) * 0.5, 22.0),
        NSSize::new(436.0, 182.0),
    );
    fill_rounded(
        panel,
        10.0,
        &NSColor::colorWithSRGBRed_green_blue_alpha(0.11, 0.12, 0.17, 1.0),
    );
    stroke_rounded(
        panel,
        10.0,
        1.0,
        &NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.12),
    );

    let dot_colors = [
        NSColor::colorWithSRGBRed_green_blue_alpha(0.95, 0.36, 0.34, 0.9),
        NSColor::colorWithSRGBRed_green_blue_alpha(0.96, 0.74, 0.22, 0.9),
        NSColor::colorWithSRGBRed_green_blue_alpha(0.24, 0.80, 0.38, 0.9),
    ];
    for (i, c) in dot_colors.iter().enumerate() {
        c.setFill();
        NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
            NSPoint::new(
                panel.origin.x + 14.0 + i as f64 * 14.0,
                panel.origin.y + 158.0,
            ),
            NSSize::new(8.5, 8.5),
        ))
        .fill();
    }
    draw_text(
        "Privacy & Security  ›  Screen & System Audio Recording",
        panel.origin.x + 64.0,
        panel.origin.y + 155.0,
        &NSFont::systemFontOfSize_weight(11.0, 0.25),
        &NSColor::colorWithSRGBRed_green_blue_alpha(0.72, 0.78, 0.88, 1.0),
    );

    let row = NSRect::new(
        NSPoint::new(panel.origin.x + 18.0, panel.origin.y + 92.0),
        NSSize::new(panel.size.width - 36.0, 46.0),
    );
    fill_rounded(
        row,
        8.0,
        &NSColor::colorWithSRGBRed_green_blue_alpha(0.15, 0.17, 0.23, 1.0),
    );
    stroke_rounded(
        row,
        8.0,
        1.0,
        &NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.10),
    );

    // Mini app badge icon
    let icon = NSRect::new(
        NSPoint::new(row.origin.x + 12.0, row.origin.y + 10.0),
        NSSize::new(26.0, 26.0),
    );
    fill_rounded(
        icon,
        6.0,
        &NSColor::colorWithSRGBRed_green_blue_alpha(0.12, 0.15, 0.24, 1.0),
    );
    NSColor::colorWithSRGBRed_green_blue_alpha(0.95, 0.97, 1.0, 0.92).setStroke();
    let ring = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
        NSPoint::new(icon.origin.x + 5.0, icon.origin.y + 5.0),
        NSSize::new(16.0, 16.0),
    ));
    ring.setLineWidth(1.5);
    ring.stroke();
    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.27, 0.25, 1.0).setFill();
    NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
        NSPoint::new(icon.origin.x + 9.0, icon.origin.y + 9.0),
        NSSize::new(8.0, 8.0),
    ))
    .fill();

    draw_text(
        "Quick Screen Recorder",
        row.origin.x + 48.0,
        row.origin.y + 15.0,
        &NSFont::systemFontOfSize_weight(13.0, 0.25),
        &NSColor::colorWithSRGBRed_green_blue_alpha(0.95, 0.97, 1.0, 1.0),
    );

    let toggle_t = if phase < 0.38 {
        0.0
    } else if phase < 0.54 {
        ease_in_out((phase - 0.38) / 0.16)
    } else {
        1.0
    };
    let sw = NSRect::new(
        NSPoint::new(row.origin.x + row.size.width - 56.0, row.origin.y + 12.0),
        NSSize::new(42.0, 22.0),
    );
    let sw_color = NSColor::colorWithSRGBRed_green_blue_alpha(
        lerp(0.28, 0.19, toggle_t),
        lerp(0.31, 0.80, toggle_t),
        lerp(0.38, 0.36, toggle_t),
        1.0,
    );
    fill_rounded(sw, 11.0, &sw_color);
    let knob_x = lerp(sw.origin.x + 2.0, sw.origin.x + 22.0, toggle_t);
    NSColor::whiteColor().setFill();
    NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
        NSPoint::new(knob_x, sw.origin.y + 2.0),
        NSSize::new(18.0, 18.0),
    ))
    .fill();

    let prompt_alpha = if phase < 0.52 {
        0.42
    } else {
        lerp(0.42, 1.0, ease_in_out((phase - 0.52) / 0.14))
    };
    let sheet = NSRect::new(
        NSPoint::new(panel.origin.x + 18.0, panel.origin.y + 20.0),
        NSSize::new(panel.size.width - 36.0, 56.0),
    );
    fill_rounded(
        sheet,
        8.0,
        &NSColor::colorWithSRGBRed_green_blue_alpha(0.08, 0.10, 0.15, prompt_alpha),
    );
    stroke_rounded(
        sheet,
        8.0,
        1.0,
        &NSColor::colorWithSRGBRed_green_blue_alpha(0.22, 0.60, 1.0, 0.38 * prompt_alpha),
    );
    draw_text(
        "When macOS asks, click:",
        sheet.origin.x + 14.0,
        sheet.origin.y + 20.0,
        &NSFont::systemFontOfSize_weight(11.5, 0.1),
        &NSColor::colorWithSRGBRed_green_blue_alpha(0.76, 0.82, 0.92, prompt_alpha),
    );
    let reopen_btn = NSRect::new(
        NSPoint::new(
            sheet.origin.x + sheet.size.width - 134.0,
            sheet.origin.y + 14.0,
        ),
        NSSize::new(118.0, 28.0),
    );
    fill_rounded(
        reopen_btn,
        6.0,
        &NSColor::colorWithSRGBRed_green_blue_alpha(0.18, 0.54, 0.98, prompt_alpha),
    );
    draw_text_centered(
        "Quit & Reopen",
        reopen_btn,
        reopen_btn.origin.y + 6.5,
        &NSFont::systemFontOfSize_weight(12.0, 0.3),
        &NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, prompt_alpha),
    );

    let (cx, cy, ripple) = if phase < 0.34 {
        let t = ease_in_out(phase / 0.34);
        (
            lerp(row.origin.x + 180.0, sw.origin.x + 28.0, t),
            lerp(panel.origin.y + 42.0, sw.origin.y + 11.0, t),
            None,
        )
    } else if phase < 0.50 {
        (
            sw.origin.x + 28.0,
            sw.origin.y + 11.0,
            Some((phase - 0.34) / 0.16),
        )
    } else {
        let t = ease_in_out(((phase - 0.56) / 0.24).clamp(0.0, 1.0));
        (
            lerp(sw.origin.x + 28.0, reopen_btn.origin.x + 62.0, t),
            lerp(sw.origin.y + 11.0, reopen_btn.origin.y + 14.0, t),
            (0.80..0.94)
                .contains(&phase)
                .then_some((phase - 0.80) / 0.14),
        )
    };
    draw_cursor_and_ripple(cx, cy, ripple);
}

pub(crate) fn draw_menubar_tutorial(bounds: NSRect, phase: f64, is_recording: bool) {
    let w = bounds.size.width;
    let h = bounds.size.height;

    fill_rounded(
        NSRect::new(NSPoint::new(1.0, 1.0), NSSize::new(w - 2.0, h - 2.0)),
        11.0,
        &NSColor::colorWithSRGBRed_green_blue_alpha(0.06, 0.08, 0.13, 1.0),
    );

    let bar_h = 30.0;
    let bar = NSRect::new(
        NSPoint::new(1.0, h - bar_h - 1.0),
        NSSize::new(w - 2.0, bar_h),
    );
    fill_rounded(
        bar,
        4.0,
        &NSColor::colorWithSRGBRed_green_blue_alpha(0.13, 0.15, 0.21, 0.98),
    );
    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.10).setFill();
    NSBezierPath::fillRect(NSRect::new(
        NSPoint::new(1.0, bar.origin.y),
        NSSize::new(w - 2.0, 1.0),
    ));

    let hint_pulse = 0.72 + 0.28 * (phase * std::f64::consts::TAU).sin().abs();
    draw_text(
        if is_recording {
            "Click ● 00:04 in your top-right menu bar to stop  ↗"
        } else {
            "Look at ◉ Rec in your top-right menu bar  ↗"
        },
        16.0,
        bar.origin.y + 7.5,
        &NSFont::systemFontOfSize_weight(11.5, 0.25),
        &NSColor::colorWithSRGBRed_green_blue_alpha(0.56, 0.80, 1.0, hint_pulse),
    );

    let rec_pill = NSRect::new(
        NSPoint::new(w - 134.0, bar.origin.y + 4.0),
        NSSize::new(72.0, 22.0),
    );
    let menu_open = phase >= 0.34;
    let pill_bg = if is_recording {
        NSColor::colorWithSRGBRed_green_blue_alpha(
            0.88,
            0.22,
            0.22,
            if menu_open { 0.95 } else { 0.78 },
        )
    } else if menu_open {
        NSColor::colorWithSRGBRed_green_blue_alpha(0.20, 0.56, 0.98, 0.92)
    } else {
        NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.14)
    };
    fill_rounded(rec_pill, 5.0, &pill_bg);
    draw_text_centered(
        if is_recording { "● 00:04" } else { "◉ Rec" },
        rec_pill,
        rec_pill.origin.y + 3.8,
        &NSFont::monospacedDigitSystemFontOfSize_weight(12.0, 0.35),
        &NSColor::whiteColor(),
    );

    draw_text(
        "09:30",
        w - 50.0,
        bar.origin.y + 7.5,
        &NSFont::monospacedDigitSystemFontOfSize_weight(11.5, 0.15),
        &NSColor::colorWithSRGBRed_green_blue_alpha(0.74, 0.79, 0.88, 0.85),
    );

    let preview_alpha = if phase >= 0.68 {
        lerp(
            0.28,
            1.0,
            ease_in_out(((phase - 0.68) / 0.12).clamp(0.0, 1.0)),
        )
    } else {
        0.26
    };
    let preview = NSRect::new(NSPoint::new(22.0, 24.0), NSSize::new(260.0, 146.0));
    fill_rounded(
        preview,
        8.0,
        &NSColor::colorWithSRGBRed_green_blue_alpha(0.09, 0.12, 0.18, preview_alpha),
    );
    if is_recording {
        stroke_rounded(
            preview,
            8.0,
            1.2,
            &NSColor::colorWithSRGBRed_green_blue_alpha(0.22, 0.60, 1.0, 0.65 * preview_alpha),
        );
        draw_text(
            "Trim & Export Editor",
            preview.origin.x + 14.0,
            preview.origin.y + 118.0,
            &NSFont::systemFontOfSize_weight(11.5, 0.3),
            &NSColor::colorWithSRGBRed_green_blue_alpha(0.90, 0.94, 1.0, preview_alpha),
        );
        fill_rounded(
            NSRect::new(
                NSPoint::new(preview.origin.x + 14.0, preview.origin.y + 54.0),
                NSSize::new(232.0, 52.0),
            ),
            6.0,
            &NSColor::colorWithSRGBRed_green_blue_alpha(0.05, 0.06, 0.10, preview_alpha),
        );
        let save_pill = NSRect::new(
            NSPoint::new(preview.origin.x + 180.0, preview.origin.y + 14.0),
            NSSize::new(66.0, 24.0),
        );
        fill_rounded(
            save_pill,
            6.0,
            &NSColor::colorWithSRGBRed_green_blue_alpha(0.18, 0.56, 1.0, preview_alpha),
        );
        draw_text_centered(
            "Save",
            save_pill,
            save_pill.origin.y + 5.0,
            &NSFont::systemFontOfSize_weight(11.5, 0.35),
            &NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, preview_alpha),
        );
    } else {
        stroke_rounded(
            preview,
            8.0,
            1.5,
            &NSColor::colorWithSRGBRed_green_blue_alpha(0.32, 0.72, 1.0, 0.90 * preview_alpha),
        );
        draw_text(
            "Area · Window · Screen",
            preview.origin.x + 14.0,
            preview.origin.y + 118.0,
            &NSFont::systemFontOfSize_weight(11.0, 0.25),
            &NSColor::colorWithSRGBRed_green_blue_alpha(0.84, 0.90, 0.98, preview_alpha),
        );
        let rec_btn = NSRect::new(
            NSPoint::new(preview.origin.x + 162.0, preview.origin.y + 14.0),
            NSSize::new(84.0, 24.0),
        );
        fill_rounded(
            rec_btn,
            12.0,
            &NSColor::colorWithSRGBRed_green_blue_alpha(0.92, 0.24, 0.24, preview_alpha),
        );
        draw_text_centered(
            "● Record",
            rec_btn,
            rec_btn.origin.y + 5.0,
            &NSFont::systemFontOfSize_weight(11.0, 0.35),
            &NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, preview_alpha),
        );
    }

    let first_item_y = h - bar_h - 38.0;
    let menu_x = w - 224.0;
    if menu_open {
        let open_t = ease_in_out(((phase - 0.34) / 0.12).clamp(0.0, 1.0));
        let menu_rect = NSRect::new(
            NSPoint::new(menu_x, h - bar_h - 122.0 + (1.0 - open_t) * 10.0),
            NSSize::new(208.0, 114.0),
        );
        fill_rounded(
            menu_rect,
            9.0,
            &NSColor::colorWithSRGBRed_green_blue_alpha(0.14, 0.16, 0.22, 0.98 * open_t),
        );
        stroke_rounded(
            menu_rect,
            9.0,
            1.0,
            &NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.15 * open_t),
        );

        let highlight_row = phase >= 0.54;
        let row1 = NSRect::new(
            NSPoint::new(menu_rect.origin.x + 6.0, menu_rect.origin.y + 82.0),
            NSSize::new(menu_rect.size.width - 12.0, 24.0),
        );
        if highlight_row {
            fill_rounded(
                row1,
                5.0,
                &NSColor::colorWithSRGBRed_green_blue_alpha(0.18, 0.54, 0.98, open_t),
            );
        }
        draw_text(
            if is_recording {
                "Stop Recording"
            } else {
                "New Recording..."
            },
            row1.origin.x + 10.0,
            row1.origin.y + 5.0,
            &NSFont::systemFontOfSize_weight(12.0, 0.25),
            &NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, open_t),
        );
        draw_text(
            "⇧⌘6",
            row1.origin.x + row1.size.width - 36.0,
            row1.origin.y + 5.0,
            &NSFont::systemFontOfSize_weight(11.0, 0.15),
            &NSColor::colorWithSRGBRed_green_blue_alpha(0.84, 0.92, 1.0, 0.85 * open_t),
        );

        let muted = NSColor::colorWithSRGBRed_green_blue_alpha(0.68, 0.74, 0.84, 0.72 * open_t);
        let item_font = NSFont::systemFontOfSize_weight(11.5, 0.1);
        draw_text(
            "Open Video in Editor...",
            row1.origin.x + 10.0,
            menu_rect.origin.y + 57.0,
            &item_font,
            &muted,
        );
        draw_text(
            "Save Folder: ~/Movies/…",
            row1.origin.x + 10.0,
            menu_rect.origin.y + 33.0,
            &item_font,
            &muted,
        );
        draw_text(
            "Capture Codec: H.264",
            row1.origin.x + 10.0,
            menu_rect.origin.y + 11.0,
            &item_font,
            &muted,
        );
    }

    let rec_cx = rec_pill.origin.x + rec_pill.size.width * 0.5;
    let rec_cy = rec_pill.origin.y + rec_pill.size.height * 0.5;
    let item_cx = menu_x + 96.0;
    let item_cy = first_item_y + 12.0;
    let (cx, cy, ripple) = if phase < 0.26 {
        let t = ease_in_out(phase / 0.26);
        (lerp(240.0, rec_cx, t), lerp(88.0, rec_cy, t), None)
    } else if phase < 0.38 {
        (rec_cx, rec_cy, Some((phase - 0.26) / 0.12))
    } else if phase < 0.58 {
        let t = ease_in_out((phase - 0.38) / 0.20);
        (lerp(rec_cx, item_cx, t), lerp(rec_cy, item_cy, t), None)
    } else if phase < 0.76 {
        (item_cx, item_cy, Some((phase - 0.58) / 0.18))
    } else {
        (item_cx, item_cy, None)
    };
    draw_cursor_and_ripple(cx, cy, ripple);
}
