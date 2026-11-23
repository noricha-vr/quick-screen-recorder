use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{CGWindowListCopyWindowInfo, CGWindowListOption};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSString};
use objc2_screen_capture_kit::{SCDisplay, SCShareableContent, SCWindow};

use crate::geometry::Rect;

#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    pub frame: Rect,
}

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub window_id: u32,
    pub app_name: String,
    pub title: String,
    pub frame: Rect,
}

impl WindowInfo {
    pub fn display_label(&self) -> String {
        if self.title.trim().is_empty() {
            self.app_name.clone()
        } else {
            let clean_title: String = self.title.chars().take(48).collect();
            format!("{} — {}", self.app_name, clean_title)
        }
    }
}

pub struct ShareableSnapshot {
    pub content: Retained<SCShareableContent>,
    pub displays: Vec<DisplayInfo>,
    pub windows: Vec<WindowInfo>,
}

pub fn query_shareable_content() -> Result<ShareableSnapshot, String> {
    query_shareable_content_with_options(true)
}

/// Queries all displays, running applications, and windows (including temporarily hidden
/// windows belonging to our own app) so capture filters exclude them reliably.
pub fn query_shareable_content_for_capture() -> Result<ShareableSnapshot, String> {
    query_shareable_content_with_options(false)
}

fn query_shareable_content_with_options(
    on_screen_windows_only: bool,
) -> Result<ShareableSnapshot, String> {
    let (tx, rx) = mpsc::sync_channel::<Result<Retained<SCShareableContent>, String>>(1);

    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let res = if !error.is_null() {
                // SAFETY: `error` is valid for the duration of the completion block.
                let err_desc = unsafe { (&*error).localizedDescription().to_string() };
                Err(format!(
                    "ScreenCaptureKit shareable content error: {err_desc}"
                ))
            } else if let Some(retained) = unsafe { Retained::retain(content) } {
                Ok(retained)
            } else {
                Err("ScreenCaptureKit returned no shareable content".to_string())
            };
            let _ = tx.send(res);
        },
    );

    // SAFETY: Completion handler is heap-allocated via RcBlock and valid across threads.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true,
            on_screen_windows_only,
            &handler,
        );
    }

    let content = rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| {
            "Timed out querying ScreenCaptureKit content. Ensure Screen Recording permission is granted."
                .to_string()
        })??;

    let current_pid = std::process::id() as i32;
    let mut displays = Vec::new();
    // SAFETY: Reading immutable properties from retained SCDisplay objects.
    unsafe {
        let sc_displays = content.displays();
        for i in 0..sc_displays.count() {
            let d: Retained<SCDisplay> = sc_displays.objectAtIndex(i);
            let f = d.frame();
            displays.push(DisplayInfo {
                display_id: d.displayID(),
                width: d.width().max(1) as u32,
                height: d.height().max(1) as u32,
                frame: Rect::new(f.origin.x, f.origin.y, f.size.width, f.size.height),
            });
        }
    }

    let mut windows = Vec::new();
    // SAFETY: Reading immutable properties from retained SCWindow objects.
    unsafe {
        let sc_windows = content.windows();
        for i in 0..sc_windows.count() {
            let w: Retained<SCWindow> = sc_windows.objectAtIndex(i);
            if !w.isOnScreen() || w.windowLayer() != 0 {
                continue;
            }
            let f = w.frame();
            if f.size.width < 80.0 || f.size.height < 80.0 {
                continue;
            }
            let Some(app) = w.owningApplication() else {
                continue;
            };
            if app.processID() == current_pid {
                continue;
            }
            let bundle_id = app.bundleIdentifier().to_string();
            if matches!(
                bundle_id.as_str(),
                "com.apple.dock"
                    | "com.apple.WindowManager"
                    | "com.apple.controlcenter"
                    | "com.apple.notificationcenterui"
                    | "com.apple.systemuiserver"
                    | "com.apple.wallpaper.agent"
            ) {
                continue;
            }
            let app_name = app.applicationName().to_string();
            if app_name.trim().is_empty() {
                continue;
            }
            let title = w.title().map(|t| t.to_string()).unwrap_or_default();
            windows.push(WindowInfo {
                window_id: w.windowID(),
                app_name,
                title,
                frame: Rect::new(f.origin.x, f.origin.y, f.size.width, f.size.height),
            });
        }
    }

    let z_order = query_window_z_order();
    if !z_order.is_empty() {
        windows.sort_by_key(|w| {
            z_order
                .iter()
                .position(|&id| id == w.window_id)
                .unwrap_or(usize::MAX)
        });
    }

    Ok(ShareableSnapshot {
        content,
        displays,
        windows,
    })
}

/// Queries CoreGraphics WindowServer for explicit front-to-back on-screen window Z-order.
fn query_window_z_order() -> Vec<u32> {
    let opts = CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements;
    let Some(cf_arr) = CGWindowListCopyWindowInfo(opts, 0) else {
        return Vec::new();
    };
    let ns_arr: &NSArray<NSDictionary<NSString, AnyObject>> =
        unsafe { &*(CFRetained::as_ptr(&cf_arr).as_ptr() as *const _) };
    let key = NSString::from_str("kCGWindowNumber");
    let mut ids = Vec::with_capacity(ns_arr.count());
    for i in 0..ns_arr.count() {
        let dict = ns_arr.objectAtIndex(i);
        if let Some(val) = dict.objectForKey(&key) {
            let num: &NSNumber = unsafe { &*(Retained::as_ptr(&val) as *const NSNumber) };
            ids.push(num.unsignedIntValue());
        }
    }
    ids
}
