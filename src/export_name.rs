use std::path::{Path, PathBuf};

use crate::observe::now_unix_secs;

pub const APP_DISPLAY_NAME: &str = "Quick Screen Recorder";
pub const DEFAULT_APP_FILE_TAG: &str = "Quick-Screen-Recorder";
const MAX_EXPORT_STEM_BYTES: usize = 160;
const MAX_APP_TAG_CHARS: usize = 28;

/// Generates a chronological, Spotlight-friendly default export file stem:
/// - Area / Screen / existing video: `YYYY-MM-DD_HH-MM-SS_Quick-Screen-Recorder`
/// - Window capture (e.g. `Safari`): `YYYY-MM-DD_HH-MM-SS_Quick-Screen-Recorder_Safari`
pub fn default_export_stem(unix_secs: u64, captured_app_name: Option<&str>) -> String {
    let ts = format_local_timestamp(unix_secs);
    if let Some(app_tag) = captured_app_name.and_then(sanitize_app_tag) {
        if !app_tag.eq_ignore_ascii_case(DEFAULT_APP_FILE_TAG) {
            return format!("{ts}_{DEFAULT_APP_FILE_TAG}_{app_tag}");
        }
    }
    format!("{ts}_{DEFAULT_APP_FILE_TAG}")
}

pub fn default_export_stem_now(captured_app_name: Option<&str>) -> String {
    default_export_stem(now_unix_secs(), captured_app_name)
}

/// Sanitizes a user-edited export file stem so it cannot traverse directories, create hidden
/// dotfiles, inject terminal/bidi control characters, duplicate the active extension, or exceed
/// filesystem byte limits. Falls back to a sanitized `fallback_stem` (or `Quick-Screen-Recorder`)
/// when the cleaned name is empty.
pub fn sanitize_export_stem(raw: &str, ext: &str, fallback_stem: &str) -> String {
    let clean_ext = sanitize_export_extension(ext);
    let cleaned = clean_single_component_stem(raw, Some(&clean_ext));
    if !cleaned.is_empty() {
        return cleaned;
    }
    let cleaned_fb = clean_single_component_stem(fallback_stem, Some(&clean_ext));
    if !cleaned_fb.is_empty() {
        cleaned_fb
    } else {
        DEFAULT_APP_FILE_TAG.to_string()
    }
}

/// Sanitizes an export file extension to ASCII alphanumerics only (defaults to `"mp4"`).
pub fn sanitize_export_extension(ext: &str) -> String {
    let clean: String = ext
        .trim()
        .trim_start_matches('.')
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect::<String>()
        .to_ascii_lowercase();
    if clean.is_empty() {
        "mp4".to_string()
    } else {
        clean
    }
}

/// Builds a guaranteed single-component destination path inside `dir` for a user-editable filename
/// stem and format extension. Atomic non-overwrite numbering (`-2`, `-3`, ...) is performed later
/// by `ExclusiveStagingDir::commit_with_policy(true)`.
pub fn build_export_output_path(
    dir: &Path,
    user_input: &str,
    ext: &str,
    fallback_stem: &str,
) -> PathBuf {
    let clean_ext = sanitize_export_extension(ext);
    let stem = sanitize_export_stem(user_input, &clean_ext, fallback_stem);
    dir.join(format!("{stem}.{clean_ext}"))
}

fn clean_single_component_stem(raw: &str, strip_ext: Option<&str>) -> String {
    let text = raw.trim();
    let mut cleaned = String::with_capacity(text.len());
    let mut prev_space = false;
    for ch in text.chars() {
        if ch == '\0' || ch.is_control() || is_bidi_or_line_sep(ch) {
            continue;
        }
        let mapped = match ch {
            '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*' => '-',
            c if c.is_whitespace() => ' ',
            c => c,
        };
        if mapped == ' ' {
            if !prev_space && !cleaned.is_empty() {
                cleaned.push(' ');
            }
            prev_space = true;
        } else {
            cleaned.push(mapped);
            prev_space = false;
        }
    }

    // Collapse traversal remnants like "..-..-etc", strip trailing dots/spaces, then strip duplicate active extension.
    let mut normalized = cleaned.replace("..", "");
    while normalized.contains("--") {
        normalized = normalized.replace("--", "-");
    }
    normalized = normalized.trim_end_matches([' ', '.']).to_string();
    if let Some(clean_ext) = strip_ext.filter(|e| !e.is_empty()) {
        let dot_ext = format!(".{clean_ext}");
        if normalized.len() >= dot_ext.len() {
            let split_at = normalized.len() - dot_ext.len();
            if normalized.is_char_boundary(split_at)
                && normalized[split_at..].eq_ignore_ascii_case(&dot_ext)
            {
                normalized.truncate(split_at);
            }
        }
    }
    let trimmed = normalized
        .trim_start_matches([' ', '.', '-'])
        .trim_end_matches([' ', '.']);
    let bounded = truncate_utf8_bytes(trimmed, MAX_EXPORT_STEM_BYTES)
        .trim_start_matches([' ', '.', '-'])
        .trim_end_matches([' ', '.']);

    if bounded.is_empty() || bounded == "." || bounded == ".." {
        String::new()
    } else {
        bounded.to_string()
    }
}

fn sanitize_app_tag(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut prev_sep = false;
    for ch in raw.trim().chars() {
        if ch == '\0' || ch.is_control() || is_bidi_or_line_sep(ch) {
            continue;
        }
        if ch.is_alphanumeric() {
            out.push(ch);
            prev_sep = false;
        } else if (matches!(ch, ' ' | '-' | '_' | '.') || ch.is_whitespace())
            && !out.is_empty()
            && !prev_sep
        {
            out.push('-');
            prev_sep = true;
        }
        if out.chars().count() >= MAX_APP_TAG_CHARS {
            break;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn format_local_timestamp(unix_secs: u64) -> String {
    let ts = unix_secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&ts, &mut tm).is_null() };
    if ok {
        format!(
            "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    } else {
        unix_secs.to_string()
    }
}

fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

const fn is_bidi_or_line_sep(c: char) -> bool {
    matches!(
        c,
        '\u{061C}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_stem_places_chronological_timestamp_first_and_app_name_after() {
        let base = default_export_stem(1_763_940_000, None);
        assert!(base.ends_with("_Quick-Screen-Recorder"), "got {base}");
        assert_eq!(
            base.len(),
            "YYYY-MM-DD_HH-MM-SS_Quick-Screen-Recorder".len()
        );

        let with_win = default_export_stem(1_763_940_000, Some("Visual Studio Code"));
        assert!(
            with_win.ends_with("_Quick-Screen-Recorder_Visual-Studio-Code"),
            "got {with_win}"
        );

        // Avoid repeating our own app tag if somehow captured
        let dup = default_export_stem(1_763_940_000, Some("Quick Screen Recorder"));
        assert!(dup.ends_with("_Quick-Screen-Recorder"), "got {dup}");
        assert!(!dup.ends_with("_Quick-Screen-Recorder_Quick-Screen-Recorder"));

        // Lexicographical sort order matches chronological order
        let earlier = default_export_stem(1_763_940_000, None);
        let later = default_export_stem(1_763_940_065, None);
        assert!(earlier < later, "{earlier} should sort before {later}");
    }

    #[test]
    fn sanitize_export_stem_blocks_traversal_dotfiles_bidi_and_duplicate_ext() {
        let fb = "2026-11-24_04-00-00_Quick-Screen-Recorder";
        assert_eq!(
            sanitize_export_stem("  my-demo.MP4  ", "mp4", fb),
            "my-demo"
        );
        assert_eq!(sanitize_export_stem("demo.mp4\u{202E}", "mp4", fb), "demo");
        assert_eq!(sanitize_export_stem("demo.mp4.", "mp4", fb), "demo");
        assert_eq!(
            sanitize_export_stem("../../../etc/passwd", "mp4", fb),
            "etc-passwd"
        );
        assert_eq!(sanitize_export_stem(" .mp4 ", "mp4", fb), fb);
        assert_eq!(sanitize_export_stem("...", "gif", fb), fb);
        assert_eq!(sanitize_export_stem("   ", "gif", fb), fb);
        assert_eq!(
            sanitize_export_stem(".hidden:clip/test\u{202E}ok", "mp4", fb),
            "hidden-clip-testok"
        );
        assert_eq!(
            sanitize_export_stem("ログイン 画面 デモ", "mp4", fb),
            "ログイン 画面 デモ"
        );

        let huge = "あ".repeat(80); // 240 UTF-8 bytes
        let cut = sanitize_export_stem(&huge, "mp4", fb);
        assert!(cut.len() <= MAX_EXPORT_STEM_BYTES);
        assert!(!cut.is_empty());

        // Fallback and extension must also never escape `dir` or retain control/bidi chars
        let base_dir = Path::new("/tmp/safe-exports");
        let safe_out = build_export_output_path(
            base_dir,
            "   ",
            "../evil/mp4\u{202E}",
            "/outside/clip\n\u{202E}",
        );
        assert_eq!(safe_out.parent(), Some(base_dir));
        assert_eq!(
            safe_out.file_name().unwrap().to_str().unwrap(),
            "outside-clip.evilmp4"
        );
    }
}
