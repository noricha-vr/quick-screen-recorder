use crate::export::ffmpeg::detect_ffmpeg_binary;

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
