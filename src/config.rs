use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::capture::{AudioCaptureMode, MicInputPreference, RecordingCodec};
use crate::export::ExportFormat;
use crate::fs_safety::{
    atomic_write_private, default_movies_export_dir, ensure_private_dir, is_usable_export_dir,
    valid_home_dir,
};
use crate::geometry::{AspectRatio, Rect};

pub const APP_SUPPORT_DIR_NAME: &str = "QuickScreenRecorder";
const MAX_PREFS_BYTES: u64 = 65_536;

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Preferences {
    pub fps: u32,
    pub show_cursor: bool,
    pub highlight_clicks: bool,
    pub audio_mode: AudioCaptureMode,
    #[serde(default = "default_true")]
    pub capture_system_audio: bool,
    #[serde(default)]
    pub mic_input: MicInputPreference,
    pub aspect_ratio: AspectRatio,
    pub last_region: Rect,
    pub recording_codec: RecordingCodec,
    pub default_export_format: ExportFormat,
    pub export_max_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export_dir: Option<PathBuf>,
    #[serde(default)]
    pub onboarding_completed: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            fps: 60,
            show_cursor: true,
            highlight_clicks: true,
            audio_mode: AudioCaptureMode::Both,
            capture_system_audio: true,
            mic_input: MicInputPreference::SystemDefault,
            aspect_ratio: AspectRatio::Free,
            last_region: Rect::new(160.0, 140.0, 960.0, 600.0),
            recording_codec: RecordingCodec::H264,
            default_export_format: ExportFormat::Mp4,
            export_max_width: None,
            export_dir: None,
            onboarding_completed: false,
        }
    }
}

impl Preferences {
    /// Resolves the pre-selected persistent export destination directory, defaulting to `~/Movies/Quick Screen Recorder`.
    pub fn resolved_export_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = &self.export_dir {
            if is_usable_export_dir(dir) {
                return Some(dir.clone());
            }
        }
        default_movies_export_dir()
    }
    pub fn default_path() -> Option<PathBuf> {
        let home = valid_home_dir()?;
        Some(
            home.join("Library")
                .join("Application Support")
                .join(APP_SUPPORT_DIR_NAME)
                .join("preferences.json"),
        )
    }

    pub fn load() -> Self {
        Self::default_path()
            .and_then(|p| Self::load_from(&p).ok())
            .unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let meta = file.metadata()?;
        let euid = unsafe { libc::geteuid() };
        if !meta.is_file() || meta.uid() != euid || meta.len() > MAX_PREFS_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Preferences file failed regular-file/ownership/size check",
            ));
        }

        let mut raw = String::new();
        file.take(MAX_PREFS_BYTES + 1).read_to_string(&mut raw)?;
        if raw.len() as u64 > MAX_PREFS_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Preferences file exceeds 64 KiB limit",
            ));
        }

        let mut prefs: Self = serde_json::from_str(&raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        prefs.fps = match prefs.fps {
            15 | 24 | 30 | 60 | 120 => prefs.fps,
            _ => 60,
        };
        if prefs
            .export_dir
            .as_ref()
            .is_some_and(|p| !is_usable_export_dir(p))
        {
            prefs.export_dir = None;
        }
        Ok(prefs)
    }

    pub fn save(&self) -> io::Result<()> {
        if let Some(path) = Self::default_path() {
            if let Some(parent) = path.parent() {
                ensure_private_dir(parent).map_err(io::Error::other)?;
            }
            self.save_to(&path)?;
        }
        Ok(())
    }

    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        let body = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        atomic_write_private(path, body.as_bytes())
    }

    pub fn effective_audio_mode(&self) -> AudioCaptureMode {
        AudioCaptureMode::from_split(self.capture_system_audio, self.mic_input.is_enabled())
    }

    /// Applies only capture-overlay fields so concurrent Menu Bar / Editor updates (such as `export_dir` or `recording_codec`) are preserved.
    pub fn merge_capture_selection(&mut self, overlay_prefs: &Self) {
        self.fps = overlay_prefs.fps;
        self.show_cursor = overlay_prefs.show_cursor;
        self.highlight_clicks = overlay_prefs.highlight_clicks;
        self.capture_system_audio = overlay_prefs.capture_system_audio;
        self.mic_input = overlay_prefs.mic_input.clone();
        self.audio_mode = self.effective_audio_mode();
        self.aspect_ratio = overlay_prefs.aspect_ratio;
        self.last_region = overlay_prefs.last_region;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_round_trip_and_rejects_symlinks() {
        let dir = std::env::temp_dir().join(format!(
            "qsr-prefs-dir-{}",
            crate::fs_safety::random_hex_token()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let tmp = dir.join("prefs.json");
        let prefs = Preferences {
            fps: 30,
            show_cursor: false,
            highlight_clicks: true,
            audio_mode: AudioCaptureMode::SystemAudio,
            capture_system_audio: true,
            mic_input: MicInputPreference::Off,
            aspect_ratio: AspectRatio::R16x9,
            last_region: Rect::new(200.0, 120.0, 1280.0, 720.0),
            recording_codec: RecordingCodec::Hevc,
            default_export_format: ExportFormat::Gif,
            export_max_width: Some(800),
            export_dir: None,
            onboarding_completed: true,
        };
        prefs.save_to(&tmp).expect("save preferences");
        let raw = std::fs::read_to_string(&tmp).expect("read raw json");
        assert!(!raw.contains("/Users/"));
        let loaded = Preferences::load_from(&tmp).expect("load preferences");
        assert_eq!(loaded, prefs);
        assert_eq!(loaded.resolved_export_dir(), default_movies_export_dir());

        let custom_dir = dir.join("exports");
        std::fs::create_dir_all(&custom_dir).expect("mkdir exports");
        std::os::unix::fs::PermissionsExt::set_mode(
            &mut std::fs::metadata(&custom_dir).unwrap().permissions(),
            0o700,
        );
        std::fs::set_permissions(
            &custom_dir,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("chmod exports 0700");
        let mut with_dir = loaded;
        with_dir.export_dir = Some(custom_dir.clone());
        with_dir.save_to(&tmp).expect("save custom export_dir");
        let loaded_custom = Preferences::load_from(&tmp).expect("load custom export_dir");
        assert_eq!(loaded_custom.resolved_export_dir(), Some(custom_dir));

        let link = dir.join("prefs-link.json");
        std::os::unix::fs::symlink(&tmp, &link).expect("symlink");
        assert!(Preferences::load_from(&link).is_err());

        // Overlay snapshot merge must preserve concurrent export_dir, recording_codec, and onboarding_completed updates
        let stale_overlay_snapshot = Preferences {
            fps: 30,
            export_dir: None,
            recording_codec: RecordingCodec::H264,
            onboarding_completed: false,
            ..Preferences::default()
        };
        let mut live = Preferences {
            fps: 60,
            export_dir: Some(PathBuf::from("/tmp/preserved-dir")),
            recording_codec: RecordingCodec::Hevc,
            onboarding_completed: true,
            ..Preferences::default()
        };
        live.merge_capture_selection(&stale_overlay_snapshot);
        assert_eq!(live.fps, 30);
        assert_eq!(live.export_dir, Some(PathBuf::from("/tmp/preserved-dir")));
        assert_eq!(live.recording_codec, RecordingCodec::Hevc);
        assert!(live.onboarding_completed);

        // Legacy v0.3 preferences JSON lacking split audio fields defaults to system audio ON + SystemDefault mic
        let legacy_json = r#"{
            "fps": 60,
            "show_cursor": true,
            "highlight_clicks": false,
            "audio_mode": "none",
            "aspect_ratio": "free",
            "last_region": {"x": 100.0, "y": 100.0, "width": 800.0, "height": 600.0},
            "recording_codec": "h264",
            "default_export_format": "mp4",
            "export_max_width": null
        }"#;
        let legacy_path = dir.join("legacy.json");
        atomic_write_private(&legacy_path, legacy_json.as_bytes()).expect("write legacy");
        let migrated = Preferences::load_from(&legacy_path).expect("load legacy");
        assert!(migrated.capture_system_audio);
        assert_eq!(migrated.mic_input, MicInputPreference::SystemDefault);
        assert_eq!(migrated.effective_audio_mode(), AudioCaptureMode::Both);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
