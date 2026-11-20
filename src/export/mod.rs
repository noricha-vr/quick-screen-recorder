pub mod av_writer;
pub mod avfoundation;
pub mod ffmpeg;
pub mod gif_apng;

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::fs_safety::{ExclusiveStagingDir, ensure_distinct_files};
use crate::observe::{PerfEvent, PerfKind, now_unix_secs, record_perf_event};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    #[default]
    Mp4,
    Hevc,
    Gif,
    Apng,
    Webm,
    Av1,
}

impl ExportFormat {
    pub const ALL: [Self; 6] = [
        Self::Mp4,
        Self::Hevc,
        Self::Gif,
        Self::Apng,
        Self::Webm,
        Self::Av1,
    ];

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "mp4" | "h264" => Ok(Self::Mp4),
            "hevc" | "h265" => Ok(Self::Hevc),
            "gif" => Ok(Self::Gif),
            "apng" | "png" => Ok(Self::Apng),
            "webm" | "vp9" => Ok(Self::Webm),
            "av1" => Ok(Self::Av1),
            other => {
                let clean = crate::fs_safety::sanitize_terminal_text(other, false);
                Err(format!(
                    "Unsupported format '{clean}'. Expected mp4, hevc, gif, apng, webm, or av1"
                ))
            }
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Hevc => "hevc",
            Self::Gif => "gif",
            Self::Apng => "apng",
            Self::Webm => "webm",
            Self::Av1 => "av1",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mp4 => "MP4 (H.264)",
            Self::Hevc => "MP4 (HEVC)",
            Self::Gif => "GIF",
            Self::Apng => "APNG",
            Self::Webm => "WebM (VP9)",
            Self::Av1 => "MP4 (AV1)",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp4 | Self::Hevc | Self::Av1 => "mp4",
            Self::Gif => "gif",
            Self::Apng => "apng",
            Self::Webm => "webm",
        }
    }

    pub fn requires_ffmpeg(self) -> bool {
        matches!(self, Self::Webm | Self::Av1)
    }

    pub fn is_available(self) -> bool {
        !self.requires_ffmpeg() || ffmpeg::detect_ffmpeg_binary().is_some()
    }
}

#[derive(Debug, Clone)]
pub struct ExportRequest {
    pub input: PathBuf,
    pub output: PathBuf,
    pub format: ExportFormat,
    pub fps: u32,
    pub start_sec: Option<f64>,
    pub end_sec: Option<f64>,
    pub max_width: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ExportResult {
    pub output_path: PathBuf,
    pub elapsed_ms: u64,
    pub output_bytes: u64,
}

pub fn run_export(req: &ExportRequest) -> Result<ExportResult, String> {
    if !req.input.exists() {
        return Err("Input recording file does not exist".to_string());
    }
    ensure_distinct_files(&req.input, &req.output)?;
    if let Ok(sym) = fs::symlink_metadata(&req.output) {
        if sym.file_type().is_symlink() {
            return Err("Refusing to overwrite symlink output path".to_string());
        }
    }

    let canon_input = fs::canonicalize(&req.input)
        .map_err(|e| format!("Failed resolving input file path: {e}"))?;
    let stage = ExclusiveStagingDir::create_for_target(&req.output, req.format.extension())?;

    let meta = avfoundation::probe_video_metadata(&canon_input)?;
    let start = req.start_sec.unwrap_or(0.0).clamp(0.0, meta.duration_sec);
    let end = req
        .end_sec
        .unwrap_or(meta.duration_sec)
        .clamp(0.0, meta.duration_sec);
    if end <= start + 0.02 {
        return Err("Selected trim range is too short (minimum 0.02s)".to_string());
    }
    let clip_duration_ms = ((end - start) * 1000.0).round() as u64;

    let staged_req = ExportRequest {
        input: canon_input,
        output: stage.staged_file().to_path_buf(),
        format: req.format,
        fps: req.fps.clamp(1, 120),
        start_sec: Some(start),
        end_sec: Some(end),
        max_width: req.max_width,
    };

    let t0 = Instant::now();
    let res = match staged_req.format {
        ExportFormat::Mp4 | ExportFormat::Hevc => {
            avfoundation::export_mp4_or_hevc(&staged_req, &meta)
        }
        ExportFormat::Gif => gif_apng::export_gif(&staged_req, &meta),
        ExportFormat::Apng => gif_apng::export_apng(&staged_req, &meta),
        ExportFormat::Webm | ExportFormat::Av1 => ffmpeg::export_with_ffmpeg(&staged_req),
    };
    let elapsed_ms = t0.elapsed().as_millis() as u64;

    match res {
        Ok(()) => {
            let bytes = match stage.commit() {
                Ok(len) => len,
                Err(err) => {
                    record_perf_event(&PerfEvent {
                        ts_unix: now_unix_secs(),
                        kind: PerfKind::Export,
                        format: req.format.slug().to_string(),
                        fps: staged_req.fps,
                        input_duration_ms: clip_duration_ms,
                        elapsed_ms,
                        output_bytes: 0,
                        ok: false,
                    });
                    return Err(err);
                }
            };
            record_perf_event(&PerfEvent {
                ts_unix: now_unix_secs(),
                kind: PerfKind::Export,
                format: req.format.slug().to_string(),
                fps: staged_req.fps,
                input_duration_ms: clip_duration_ms,
                elapsed_ms,
                output_bytes: bytes,
                ok: true,
            });
            Ok(ExportResult {
                output_path: req.output.clone(),
                elapsed_ms,
                output_bytes: bytes,
            })
        }
        Err(err) => {
            record_perf_event(&PerfEvent {
                ts_unix: now_unix_secs(),
                kind: PerfKind::Export,
                format: req.format.slug().to_string(),
                fps: staged_req.fps,
                input_duration_ms: clip_duration_ms,
                elapsed_ms,
                output_bytes: 0,
                ok: false,
            });
            Err(err)
        }
    }
}
