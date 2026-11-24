pub mod audio_devices;
pub mod cadence;
mod filter;
pub mod recorder;
pub mod shareable;
pub mod timelapse;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use cadence::{CaptureCadence, TIMELAPSE_PLAYBACK_FPS};
use recorder::{PendingRecordingStop, RecordingSession, StopPollState};
use timelapse::{PendingTimelapseStop, TimelapseSession};

use crate::geometry::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AudioCaptureMode {
    #[default]
    None,
    SystemAudio,
    Microphone,
    Both,
}

impl AudioCaptureMode {
    pub const ALL: [Self; 4] = [Self::None, Self::SystemAudio, Self::Microphone, Self::Both];

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "Audio: Off",
            Self::SystemAudio => "System Audio",
            Self::Microphone => "Microphone",
            Self::Both => "System + Mic",
        }
    }

    pub fn captures_system(self) -> bool {
        matches!(self, Self::SystemAudio | Self::Both)
    }

    pub fn captures_mic(self) -> bool {
        matches!(self, Self::Microphone | Self::Both)
    }

    pub fn from_index(idx: usize) -> Self {
        Self::ALL.get(idx).copied().unwrap_or(Self::None)
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|&m| m == self).unwrap_or(0)
    }

    pub fn from_split(system_audio: bool, mic_enabled: bool) -> Self {
        match (system_audio, mic_enabled) {
            (false, false) => Self::None,
            (true, false) => Self::SystemAudio,
            (false, true) => Self::Microphone,
            (true, true) => Self::Both,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum MicInputPreference {
    Off,
    #[default]
    SystemDefault,
    Device {
        id: String,
        name: String,
    },
}

impl MicInputPreference {
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Off)
    }

    pub fn explicit_device_id(&self) -> Option<&str> {
        match self {
            Self::Device { id, .. } => Some(id.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RecordingCodec {
    #[default]
    H264,
    Hevc,
}

impl RecordingCodec {
    pub fn label(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CaptureTarget {
    /// Display-local region in top-left (CoreGraphics / ScreenCaptureKit) points.
    Region {
        display_id: u32,
        rect_points: Rect,
        scale_factor: f64,
    },
    /// Single independent window capture.
    Window { window_id: u32 },
    /// Entire display capture.
    Display { display_id: u32, scale_factor: f64 },
}

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub target: CaptureTarget,
    pub cadence: CaptureCadence,
    pub codec: RecordingCodec,
    pub show_cursor: bool,
    pub highlight_clicks: bool,
    pub audio_mode: AudioCaptureMode,
    pub microphone_device_id: Option<String>,
    pub excluded_window_ids: Vec<u32>,
    pub source_app_name: Option<String>,
}

/// Unified handle for either real-time (`SCRecordingOutput`) or interval timelapse capture.
pub enum ActiveRecording {
    RealTime(RecordingSession),
    Timelapse(TimelapseSession),
}

pub enum PendingStop {
    RealTime(PendingRecordingStop),
    Timelapse(PendingTimelapseStop),
}

impl ActiveRecording {
    pub fn start(config: &CaptureConfig, output_path: &Path) -> Result<Self, String> {
        match config.cadence.normalized() {
            CaptureCadence::RealTime { .. } => Ok(Self::RealTime(RecordingSession::start(
                config,
                output_path,
            )?)),
            CaptureCadence::Timelapse { .. } => Ok(Self::Timelapse(TimelapseSession::start(
                config,
                output_path,
            )?)),
        }
    }

    pub fn elapsed(&self) -> Duration {
        match self {
            Self::RealTime(s) => s.elapsed(),
            Self::Timelapse(s) => s.elapsed(),
        }
    }

    pub fn timelapse_frames(&self) -> Option<u64> {
        match self {
            Self::RealTime(_) => None,
            Self::Timelapse(s) => Some(s.captured_frames()),
        }
    }

    pub fn begin_stop(self) -> PendingStop {
        match self {
            Self::RealTime(s) => PendingStop::RealTime(s.begin_stop()),
            Self::Timelapse(s) => PendingStop::Timelapse(s.begin_stop()),
        }
    }

    pub fn stop(self) -> Result<PathBuf, String> {
        match self {
            Self::RealTime(s) => s.stop(),
            Self::Timelapse(s) => s.stop(),
        }
    }
}

impl PendingStop {
    pub fn poll(&mut self) -> StopPollState {
        match self {
            Self::RealTime(p) => p.poll(),
            Self::Timelapse(p) => p.poll(),
        }
    }

    pub fn finish(self) -> Result<PathBuf, String> {
        match self {
            Self::RealTime(p) => p.finish(),
            Self::Timelapse(p) => p.finish(),
        }
    }
}
