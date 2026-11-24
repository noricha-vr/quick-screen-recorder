use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::NSArray;
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCRunningApplication, SCShareableContent, SCWindow,
};

use crate::capture::{CaptureConfig, CaptureTarget};

pub(crate) fn build_filter_and_dimensions(
    content: &SCShareableContent,
    config: &CaptureConfig,
) -> Result<(Retained<SCContentFilter>, usize, usize, Option<CGRect>), String> {
    match &config.target {
        CaptureTarget::Window { window_id } => {
            let sc_windows = unsafe { content.windows() };
            let mut found: Option<Retained<SCWindow>> = None;
            for i in 0..sc_windows.count() {
                let w = sc_windows.objectAtIndex(i);
                if unsafe { w.windowID() } == *window_id {
                    found = Some(w);
                    break;
                }
            }
            let win =
                found.ok_or_else(|| format!("Window ID {window_id} is no longer available"))?;
            let frame = unsafe { win.frame() };
            let filter = unsafe {
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &win)
            };
            let scale = unsafe { f64::from(filter.pointPixelScale()).max(1.0) };
            let pw = even_usize((frame.size.width * scale).round());
            let ph = even_usize((frame.size.height * scale).round());
            Ok((filter, pw, ph, None))
        }
        CaptureTarget::Display {
            display_id,
            scale_factor,
        } => {
            let display = find_display(content, *display_id)?;
            let filter = build_display_filter(content, &display, &config.excluded_window_ids);
            let scale = if *scale_factor > 0.5 {
                *scale_factor
            } else {
                unsafe { f64::from(filter.pointPixelScale()).max(1.0) }
            };
            let dw = unsafe { display.width().max(2) as f64 };
            let dh = unsafe { display.height().max(2) as f64 };
            let pw = even_usize((dw * scale).round());
            let ph = even_usize((dh * scale).round());
            Ok((filter, pw, ph, None))
        }
        CaptureTarget::Region {
            display_id,
            rect_points,
            scale_factor,
        } => {
            let display = find_display(content, *display_id)?;
            let filter = build_display_filter(content, &display, &config.excluded_window_ids);
            let scale = if *scale_factor > 0.5 {
                *scale_factor
            } else {
                unsafe { f64::from(filter.pointPixelScale()).max(1.0) }
            };
            let dw = unsafe { display.width().max(32) as f64 };
            let dh = unsafe { display.height().max(32) as f64 };
            let clamped = rect_points
                .clamp_and_align_for_scale(crate::geometry::Rect::new(0.0, 0.0, dw, dh), scale);
            let cg_rect = CGRect::new(
                CGPoint::new(clamped.x, clamped.y),
                CGSize::new(clamped.width, clamped.height),
            );
            let pw = even_usize((clamped.width * scale).round());
            let ph = even_usize((clamped.height * scale).round());
            Ok((filter, pw, ph, Some(cg_rect)))
        }
    }
}

fn find_display(
    content: &SCShareableContent,
    display_id: u32,
) -> Result<Retained<SCDisplay>, String> {
    let displays = unsafe { content.displays() };
    if displays.count() == 0 {
        return Err("No active displays detected".to_string());
    }
    if display_id == 0 {
        return Ok(displays.objectAtIndex(0));
    }
    for i in 0..displays.count() {
        let d = displays.objectAtIndex(i);
        if unsafe { d.displayID() } == display_id {
            return Ok(d);
        }
    }
    Err(format!(
        "Selected display (ID {display_id}) is no longer connected"
    ))
}

fn build_display_filter(
    content: &SCShareableContent,
    display: &SCDisplay,
    extra_window_ids: &[u32],
) -> Retained<SCContentFilter> {
    let current_pid = std::process::id() as i32;
    let apps = unsafe { content.applications() };
    let mut own_apps: Vec<Retained<SCRunningApplication>> = Vec::new();
    for i in 0..apps.count() {
        let app = apps.objectAtIndex(i);
        if unsafe { app.processID() } == current_pid {
            own_apps.push(app);
            break;
        }
    }
    if !own_apps.is_empty() {
        let excluded_apps = NSArray::from_retained_slice(&own_apps);
        let empty_except: Retained<NSArray<SCWindow>> = NSArray::from_retained_slice(&[]);
        return unsafe {
            SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                SCContentFilter::alloc(),
                display,
                &excluded_apps,
                &empty_except,
            )
        };
    }

    let excluded = collect_excluded_windows(content, extra_window_ids);
    unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            display,
            &excluded,
        )
    }
}

fn collect_excluded_windows(
    content: &SCShareableContent,
    extra_window_ids: &[u32],
) -> Retained<NSArray<SCWindow>> {
    let current_pid = std::process::id() as i32;
    let sc_windows = unsafe { content.windows() };
    let mut vec_win: Vec<Retained<SCWindow>> = Vec::new();
    for i in 0..sc_windows.count() {
        let w = sc_windows.objectAtIndex(i);
        let wid = unsafe { w.windowID() };
        let own_pid = unsafe { w.owningApplication().map(|a| a.processID()).unwrap_or(-1) };
        if own_pid == current_pid || extra_window_ids.contains(&wid) {
            vec_win.push(w);
        }
    }
    NSArray::from_retained_slice(&vec_win)
}

fn even_usize(v: f64) -> usize {
    let n = (v.max(32.0) as usize) & !1;
    n.max(32)
}
