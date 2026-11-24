use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::capture::{AudioCaptureMode, CaptureCadence, MicInputPreference, RecordingCodec};
use crate::export::ExportFormat;
use crate::fs_safety::{
    atomic_write_private, default_movies_export_dir, ensure_private_dir, is_usable_export_dir,
    valid_home_dir,
};
use crate::geometry::{AspectRatio, Rect};

pub const APP_SUPPORT_DIR_NAME: &str = "AirRec";
const LEGACY_APP_SUPPORT_DIR_NAME: &str = "QuickScreenRecorder";
const MAX_PREFS_BYTES: u64 = 65_536;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Preferences {
    pub fps: u32,
    pub capture_cadence: CaptureCadence,
    pub show_cursor: bool,
    pub highlight_clicks: bool,
    pub audio_mode: AudioCaptureMode,
    pub capture_system_audio: bool,
    pub mic_input: MicInputPreference,
    pub aspect_ratio: AspectRatio,
    pub last_region: Rect,
    pub recording_codec: RecordingCodec,
    pub default_export_format: ExportFormat,
    pub export_max_width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub export_dir: Option<PathBuf>,
    pub onboarding_completed: bool,
}

#[derive(Debug, Deserialize)]
struct RawPreferences {
    fps: u32,
    #[serde(default)]
    capture_cadence: Option<CaptureCadence>,
    show_cursor: bool,
    highlight_clicks: bool,
    audio_mode: AudioCaptureMode,
    #[serde(default)]
    capture_system_audio: Option<bool>,
    #[serde(default)]
    mic_input: Option<MicInputPreference>,
    aspect_ratio: AspectRatio,
    last_region: Rect,
    recording_codec: RecordingCodec,
    default_export_format: ExportFormat,
    export_max_width: Option<u32>,
    #[serde(default)]
    export_dir: Option<PathBuf>,
    #[serde(default)]
    onboarding_completed: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            fps: 60,
            capture_cadence: CaptureCadence::RealTime { fps: 60 },
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
    /// Fail-closed defaults used when an existing preferences file cannot be read or parsed safely,
    /// ensuring corrupted/tampered config never silently enables system audio or microphone capture.
    pub fn fail_closed_defaults() -> Self {
        Self {
            audio_mode: AudioCaptureMode::None,
            capture_system_audio: false,
            mic_input: MicInputPreference::Off,
            ..Self::default()
        }
    }

    /// Resolves the pre-selected persistent export destination directory, defaulting to `~/Movies/AirRec`.
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

    pub fn legacy_path() -> Option<PathBuf> {
        let home = valid_home_dir()?;
        Some(
            home.join("Library")
                .join("Application Support")
                .join(LEGACY_APP_SUPPORT_DIR_NAME)
                .join("preferences.json"),
        )
    }

    pub fn load() -> Self {
        match Self::default_path() {
            Some(path) => Self::load_or_migrate(&path, Self::legacy_path().as_deref()),
            None => Self::fail_closed_defaults(),
        }
    }

    pub fn load_or_default_at(path: &Path) -> Self {
        Self::load_or_migrate(path, None)
    }

    pub fn load_or_migrate(primary: &Path, legacy: Option<&Path>) -> Self {
        match Self::load_from(primary) {
            Ok(prefs) => prefs,
            Err(err) if err.kind() == io::ErrorKind::NotFound => match legacy {
                Some(legacy_path) => match Self::load_from(legacy_path) {
                    Ok(prefs) => prefs,
                    Err(le) if le.kind() == io::ErrorKind::NotFound => Self::default(),
                    Err(_) => Self::fail_closed_defaults(),
                },
                None => Self::default(),
            },
            Err(_) => Self::fail_closed_defaults(),
        }
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

        let parsed: RawPreferences = serde_json::from_str(&raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Migrate legacy v0.3 preferences cleanly from `audio_mode` so previously disabled audio stays OFF.
        let capture_system_audio = parsed
            .capture_system_audio
            .unwrap_or_else(|| parsed.audio_mode.captures_system());
        let mic_input = parsed.mic_input.unwrap_or_else(|| {
            if parsed.audio_mode.captures_mic() {
                MicInputPreference::SystemDefault
            } else {
                MicInputPreference::Off
            }
        });
        let audio_mode = AudioCaptureMode::from_split(capture_system_audio, mic_input.is_enabled());
        let fps = if (1..=120).contains(&parsed.fps) {
            parsed.fps
        } else {
            60
        };
        let capture_cadence = parsed
            .capture_cadence
            .map(CaptureCadence::normalized)
            .unwrap_or(CaptureCadence::RealTime { fps });
        let export_dir = parsed.export_dir.filter(|dir| is_usable_export_dir(dir));

        Ok(Self {
            fps,
            capture_cadence,
            show_cursor: parsed.show_cursor,
            highlight_clicks: parsed.highlight_clicks,
            audio_mode,
            capture_system_audio,
            mic_input,
            aspect_ratio: parsed.aspect_ratio,
            last_region: parsed.last_region,
            recording_codec: parsed.recording_codec,
            default_export_format: parsed.default_export_format,
            export_max_width: parsed.export_max_width,
            export_dir,
            onboarding_completed: parsed.onboarding_completed,
        })
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
        self.capture_cadence = overlay_prefs.capture_cadence.normalized();
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

    fn legacy_json_with_audio(audio_mode: &str) -> String {
        format!(
            r#"{{
                "fps": 60,
                "show_cursor": true,
                "highlight_clicks": false,
                "audio_mode": "{audio_mode}",
                "aspect_ratio": "free",
                "last_region": {{"x": 100.0, "y": 100.0, "width": 800.0, "height": 600.0}},
                "recording_codec": "h264",
                "default_export_format": "mp4",
                "export_max_width": null
            }}"#
        )
    }

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
            capture_cadence: CaptureCadence::Timelapse { interval_secs: 60 },
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

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_audio_migration_preserves_off_and_corrupt_file_fails_closed() {
        let dir = std::env::temp_dir().join(format!(
            "qsr-prefs-mig-{}",
            crate::fs_safety::random_hex_token()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");

        // Brand-new install (missing file) gets new v0.4.0 defaults (System Audio ON + System Default Mic)
        let missing = dir.join("does-not-exist.json");
        let fresh = Preferences::load_or_default_at(&missing);
        assert!(fresh.capture_system_audio);
        assert_eq!(fresh.mic_input, MicInputPreference::SystemDefault);
        assert_eq!(fresh.audio_mode, AudioCaptureMode::Both);

        // Legacy v0.3 preferences preserve each historical audio_mode without turning audio back ON
        let cases = [
            (
                "none",
                false,
                MicInputPreference::Off,
                AudioCaptureMode::None,
            ),
            (
                "system_audio",
                true,
                MicInputPreference::Off,
                AudioCaptureMode::SystemAudio,
            ),
            (
                "microphone",
                false,
                MicInputPreference::SystemDefault,
                AudioCaptureMode::Microphone,
            ),
            (
                "both",
                true,
                MicInputPreference::SystemDefault,
                AudioCaptureMode::Both,
            ),
        ];
        for (mode_str, exp_sys, exp_mic, exp_mode) in cases {
            let p = dir.join(format!("legacy-{mode_str}.json"));
            atomic_write_private(&p, legacy_json_with_audio(mode_str).as_bytes()).expect("write");
            let loaded = Preferences::load_from(&p).expect("load legacy");
            assert_eq!(loaded.capture_cadence, CaptureCadence::RealTime { fps: 60 });
            assert_eq!(loaded.capture_system_audio, exp_sys, "mode={mode_str}");
            assert_eq!(loaded.mic_input, exp_mic, "mode={mode_str}");
            assert_eq!(loaded.audio_mode, exp_mode, "mode={mode_str}");
        }

        // Corrupted existing config fails closed with audio OFF
        let corrupt = dir.join("corrupt.json");
        atomic_write_private(&corrupt, b"{not-valid-json").expect("write corrupt");
        let recovered = Preferences::load_or_default_at(&corrupt);
        assert!(!recovered.capture_system_audio);
        assert_eq!(recovered.mic_input, MicInputPreference::Off);
        assert_eq!(recovered.audio_mode, AudioCaptureMode::None);

        // Symlinked config also fails closed with audio OFF
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&corrupt, &link).expect("symlink");
        let from_link = Preferences::load_or_default_at(&link);
        assert!(!from_link.capture_system_audio);
        assert_eq!(from_link.mic_input, MicInputPreference::Off);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_or_migrate_inherits_legacy_preferences_and_fails_closed_on_corrupt_legacy() {
        let base = std::env::temp_dir().join(format!(
            "qsr-migrate-{}",
            crate::fs_safety::next_unique_token()
        ));
        let primary = base.join("AirRec").join("preferences.json");
        let legacy_dir = base.join("QuickScreenRecorder");
        let legacy = legacy_dir.join("preferences.json");
        std::fs::create_dir_all(&legacy_dir).expect("mkdir legacy");

        // 1. Legacy prefs with system audio OFF + specific mic device must be preserved when primary is absent.
        let legacy_prefs = Preferences {
            capture_system_audio: false,
            mic_input: MicInputPreference::Device {
                id: "usb-mic-01".to_string(),
                name: "USB Mic".to_string(),
            },
            audio_mode: AudioCaptureMode::Microphone,
            ..Preferences::default()
        };
        legacy_prefs.save_to(&legacy).expect("save legacy");
        let loaded = Preferences::load_or_migrate(&primary, Some(&legacy));
        assert!(!loaded.capture_system_audio);
        assert_eq!(loaded.mic_input, legacy_prefs.mic_input);
        assert_eq!(loaded.effective_audio_mode(), AudioCaptureMode::Microphone);

        // 2. Corrupt legacy prefs must fail closed with both audio channels OFF.
        atomic_write_private(&legacy, b"{not valid json").expect("write corrupt legacy");
        let corrupt = Preferences::load_or_migrate(&primary, Some(&legacy));
        assert!(!corrupt.capture_system_audio);
        assert_eq!(corrupt.mic_input, MicInputPreference::Off);
        assert_eq!(corrupt.effective_audio_mode(), AudioCaptureMode::None);

        // 3. Existing primary prefs always win over legacy.
        let primary_dir = primary.parent().expect("primary parent");
        std::fs::create_dir_all(primary_dir).expect("mkdir primary");
        let primary_prefs = Preferences {
            capture_system_audio: true,
            mic_input: MicInputPreference::Off,
            audio_mode: AudioCaptureMode::SystemAudio,
            ..Preferences::default()
        };
        primary_prefs.save_to(&primary).expect("save primary");
        let loaded_primary = Preferences::load_or_migrate(&primary, Some(&legacy));
        assert!(loaded_primary.capture_system_audio);
        assert_eq!(loaded_primary.mic_input, MicInputPreference::Off);

        let _ = std::fs::remove_dir_all(&base);
    }
}
