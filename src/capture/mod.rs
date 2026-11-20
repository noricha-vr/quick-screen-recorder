pub mod recorder;
pub mod shareable;

use serde::{Deserialize, Serialize};

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
    pub fps: u32,
    pub codec: RecordingCodec,
    pub show_cursor: bool,
    pub highlight_clicks: bool,
    pub audio_mode: AudioCaptureMode,
    pub excluded_window_ids: Vec<u32>,
}
