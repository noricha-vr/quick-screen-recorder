use std::path::Path;

use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSArray, NSString, NSURL};

use crate::export::ffmpeg::detect_ffmpeg_binary;

pub const SCREEN_RECORDING_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

pub fn has_screen_capture_permission() -> bool {
    // SAFETY: Standard macOS CoreGraphics C function taking no arguments.
    unsafe { CGPreflightScreenCaptureAccess() }
}

pub fn request_screen_capture_permission() -> bool {
    // SAFETY: Standard macOS CoreGraphics C function taking no arguments.
    unsafe { CGRequestScreenCaptureAccess() }
}

/// Triggers macOS Screen Recording registration if needed and opens System Settings directly to
/// Privacy & Security > Screen & System Audio Recording.
pub fn open_screen_recording_settings() -> bool {
    if !has_screen_capture_permission() {
        let _ = request_screen_capture_permission();
    }
    let url_str = NSString::from_str(SCREEN_RECORDING_SETTINGS_URL);
    let Some(url) = NSURL::URLWithString(&url_str) else {
        return false;
    };
    NSWorkspace::sharedWorkspace().openURL(&url)
}

/// Reveals a saved recording or folder in Finder without spawning any subprocesses.
pub fn reveal_in_finder(path: &Path) {
    let Some(path_str) = path.to_str() else {
        return;
    };
    let url = NSURL::fileURLWithPath(&NSString::from_str(path_str));
    let urls = NSArray::from_retained_slice(&[url]);
    NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&urls);
}

pub fn doctor_report() -> String {
    let screen_perm = has_screen_capture_permission();
    let ffmpeg_available = detect_ffmpeg_binary().is_some();

    let mut lines = Vec::new();
    lines.push("Quick Screen Recorder — Environment Check".to_string());
    lines.push(format!(
        "  Screen Recording permission : {}",
        if screen_perm {
            "granted"
        } else {
            "not granted (grant in System Settings > Privacy & Security > Screen & System Audio Recording)"
        }
    ));
    lines.push("  Native formats (zero-dep)   : mp4 (H.264), hevc (H.265), gif, apng".to_string());
    lines.push(format!(
        "  Optional FFmpeg formats     : {}",
        if ffmpeg_available {
            "webm (VP9), av1 (available)"
        } else {
            "not detected (install ffmpeg to enable webm and av1 export)"
        }
    ));
    lines.push("  Shortcut (Cmd+Shift+6)      : registered via Carbon HotKey (no Input Monitoring permission needed)".to_string());
    lines.join("\n")
}
