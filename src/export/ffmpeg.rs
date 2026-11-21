use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::export::ffmpeg_spawn::{spawn_disclaimed_with_timeout, verify_trusted_binary};
use crate::export::{ExportFormat, ExportRequest, SharedExportProgress};

const ALLOWLISTED_FFMPEG_PATHS: [&str; 3] = [
    "/opt/homebrew/bin/ffmpeg",
    "/usr/local/bin/ffmpeg",
    "/usr/bin/ffmpeg",
];

const FFMPEG_TIMEOUT: Duration = Duration::from_secs(180);

pub fn detect_ffmpeg_binary() -> Option<PathBuf> {
    let euid = unsafe { libc::geteuid() };
    for candidate in ALLOWLISTED_FFMPEG_PATHS {
        if let Some(verified) = verify_trusted_binary(Path::new(candidate), euid) {
            return Some(verified);
        }
    }
    None
}

pub fn export_with_ffmpeg(
    req: &ExportRequest,
    clip_duration_sec: f64,
    progress: Option<&SharedExportProgress>,
) -> Result<(), String> {
    let ffmpeg = detect_ffmpeg_binary().ok_or_else(|| {
        format!(
            "{} export requires a trusted 'ffmpeg' binary at /opt/homebrew/bin/ffmpeg or /usr/local/bin/ffmpeg. MP4, HEVC, GIF, and APNG work natively without ffmpeg.",
            req.format.label()
        )
    })?;

    let local_in = verified_local_file_arg(&req.input, true)?;
    let local_out = verified_local_file_arg(&req.output, false)?;

    match req.format {
        ExportFormat::Webm => run_ffmpeg_encode(
            &ffmpeg,
            &local_in,
            &local_out,
            req,
            &[
                "-c:v",
                "libvpx-vp9",
                "-crf",
                "30",
                "-b:v",
                "0",
                "-row-mt",
                "1",
                "-cpu-used",
                "4",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "libopus",
            ],
            "webm",
            clip_duration_sec,
            progress,
        ),
        ExportFormat::Av1 => {
            let svt = run_ffmpeg_encode(
                &ffmpeg,
                &local_in,
                &local_out,
                req,
                &[
                    "-c:v",
                    "libsvtav1",
                    "-crf",
                    "32",
                    "-preset",
                    "8",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                ],
                "mp4",
                clip_duration_sec,
                progress,
            );
            if svt.is_ok() {
                return Ok(());
            }
            run_ffmpeg_encode(
                &ffmpeg,
                &local_in,
                &local_out,
                req,
                &[
                    "-c:v",
                    "libaom-av1",
                    "-crf",
                    "34",
                    "-cpu-used",
                    "6",
                    "-row-mt",
                    "1",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                ],
                "mp4",
                clip_duration_sec,
                progress,
            )
        }
        other => Err(format!(
            "Internal error: {} should use native exporter",
            other.label()
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_ffmpeg_encode(
    ffmpeg_bin: &Path,
    input_file_url: &str,
    output_file_url: &str,
    req: &ExportRequest,
    codec_args: &[&str],
    container: &str,
    clip_duration_sec: f64,
    progress: Option<&SharedExportProgress>,
) -> Result<(), String> {
    let mut args: Vec<String> = vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostats".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-y".into(),
        "-protocol_whitelist".into(),
        "file".into(),
    ];

    if let Some(start) = req.start_sec {
        if start > 0.005 {
            args.push("-ss".into());
            args.push(format!("{start:.3}"));
        }
    }
    if let Some(end) = req.end_sec {
        args.push("-to".into());
        args.push(format!("{end:.3}"));
    }

    // Restrict demuxer to MP4/MOV family and disable external MOV data references (`dref`)
    args.extend([
        "-f".into(),
        "mov,mp4,m4a,3gp,3g2,mj2".into(),
        "-enable_drefs".into(),
        "0".into(),
        "-use_absolute_path".into(),
        "0".into(),
        "-i".into(),
        input_file_url.into(),
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "0:a:0?".into(),
        "-sn".into(),
        "-dn".into(),
        "-map_metadata".into(),
        "-1".into(),
        "-map_chapters".into(),
        "-1".into(),
        "-r".into(),
        req.fps.clamp(1, 120).to_string(),
    ]);

    if let Some(max_w) = req.max_width {
        let safe_w = (max_w.max(32)) & !1;
        args.push("-vf".into());
        args.push(format!("scale='min({safe_w},iw)':-2:flags=lanczos"));
    } else {
        args.push("-vf".into());
        args.push("scale=trunc(iw/2)*2:trunc(ih/2)*2".into());
    }

    for &c in codec_args {
        args.push(c.into());
    }
    args.extend([
        "-protocol_whitelist".into(),
        "file".into(),
        "-f".into(),
        container.into(),
        output_file_url.into(),
    ]);

    spawn_disclaimed_with_timeout(
        ffmpeg_bin,
        &args,
        FFMPEG_TIMEOUT,
        clip_duration_sec,
        progress.cloned(),
    )
}

pub fn verified_local_file_arg(path: &Path, must_exist: bool) -> Result<String, String> {
    let resolved = if must_exist {
        fs::canonicalize(path).map_err(|e| format!("Invalid local input file: {e}"))?
    } else {
        let file_name = path
            .file_name()
            .ok_or("Output path is missing a file name")?;
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => std::env::current_dir().map_err(|e| format!("cwd error: {e}"))?,
        };
        let canon_parent =
            fs::canonicalize(&parent).map_err(|e| format!("Invalid output directory: {e}"))?;
        canon_parent.join(file_name)
    };

    if !resolved.is_absolute() {
        return Err("Resolved path must be an absolute local path".to_string());
    }
    let s = resolved.to_str().ok_or("Path is not valid UTF-8")?;
    Ok(format!("file:{s}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_local_file_arg_always_uses_file_scheme_on_canonical_path() {
        assert!(
            verified_local_file_arg(Path::new("http://127.0.0.1:9999/evil.webm"), false).is_err()
        );
        let valid = std::env::temp_dir().join("safe-out.webm");
        let arg = verified_local_file_arg(&valid, false).expect("valid local target");
        assert!(arg.starts_with("file:/"));
    }
}
