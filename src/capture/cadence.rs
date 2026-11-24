use serde::{Deserialize, Serialize};

use crate::capture::AudioCaptureMode;

/// Output playback frame rate for interval timelapse recordings (30 fps for universal QuickTime/web/SNS playback).
pub const TIMELAPSE_PLAYBACK_FPS: u32 = 30;

/// Real-time recording frame rates offered in the overlay toolbar dropdown.
pub const REALTIME_FPS_PRESETS: [u32; 5] = [60, 30, 15, 5, 1];

/// Interval timelapse presets (seconds per captured frame) offered in the overlay toolbar dropdown.
pub const TIMELAPSE_INTERVAL_PRESETS: [u32; 4] = [5, 10, 30, 60];

/// Export FPS choices offered in the Trim & Export editor.
pub const EDITOR_FPS_PRESETS: [u32; 5] = [1, 5, 15, 30, 60];

/// Capture timing strategy:
/// - `RealTime { fps }` (`1..=120` fps): real-time playback speed via `SCRecordingOutput` (supports system & mic audio).
/// - `Timelapse { interval_secs }` (`1..=3600` sec/frame): periodic frame sampling written at 30 fps fast-forward playback (audio automatically muted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureCadence {
    RealTime { fps: u32 },
    Timelapse { interval_secs: u32 },
}

impl Default for CaptureCadence {
    fn default() -> Self {
        Self::RealTime { fps: 60 }
    }
}

impl CaptureCadence {
    pub fn normalized(self) -> Self {
        match self {
            Self::RealTime { fps } => Self::RealTime {
                fps: fps.clamp(1, 120),
            },
            Self::Timelapse { interval_secs } => Self::Timelapse {
                interval_secs: interval_secs.clamp(1, 3600),
            },
        }
    }

    pub fn is_timelapse(self) -> bool {
        matches!(self, Self::Timelapse { .. })
    }

    pub fn realtime_fps(self) -> Option<u32> {
        match self {
            Self::RealTime { fps } => Some(fps.clamp(1, 120)),
            Self::Timelapse { .. } => None,
        }
    }

    pub fn timelapse_interval_secs(self) -> Option<u32> {
        match self {
            Self::Timelapse { interval_secs } => Some(interval_secs.clamp(1, 3600)),
            Self::RealTime { .. } => None,
        }
    }

    /// Forces audio OFF during timelapse capture because real-time audio cannot sync to fast-forward playback.
    pub fn effective_audio_mode(self, requested: AudioCaptureMode) -> AudioCaptureMode {
        if self.is_timelapse() {
            AudioCaptureMode::None
        } else {
            requested
        }
    }

    pub fn menu_label(self) -> String {
        match self {
            Self::RealTime { fps } => format!("{fps} fps"),
            Self::Timelapse { interval_secs } => {
                if interval_secs >= 60 && interval_secs % 60 == 0 {
                    format!("1f/{}m · TL", interval_secs / 60)
                } else {
                    format!("1f/{interval_secs}s · TL")
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CadenceMenuItem {
    Option {
        cadence: CaptureCadence,
        label: String,
    },
    Separator,
}

/// Builds the overlay toolbar cadence dropdown model:
/// `[60 fps, 30 fps, 15 fps, 5 fps, 1 fps, Separator, 1f/5s · TL, 1f/10s · TL, 1f/30s · TL, 1f/1m · TL]`
/// plus the selected menu index for `current`.
pub fn build_cadence_menu_model(current: CaptureCadence) -> (Vec<CadenceMenuItem>, usize) {
    let norm = current.normalized();
    let mut items =
        Vec::with_capacity(REALTIME_FPS_PRESETS.len() + TIMELAPSE_INTERVAL_PRESETS.len() + 2);

    for fps in REALTIME_FPS_PRESETS {
        let cadence = CaptureCadence::RealTime { fps };
        items.push(CadenceMenuItem::Option {
            cadence,
            label: cadence.menu_label(),
        });
    }
    items.push(CadenceMenuItem::Separator);
    for interval_secs in TIMELAPSE_INTERVAL_PRESETS {
        let cadence = CaptureCadence::Timelapse { interval_secs };
        items.push(CadenceMenuItem::Option {
            cadence,
            label: cadence.menu_label(),
        });
    }

    if let Some(exact) = items
        .iter()
        .position(|it| matches!(it, CadenceMenuItem::Option { cadence, .. } if *cadence == norm))
    {
        return (items, exact);
    }

    // Closest preset fallback if preferences held a custom CLI/manual cadence value.
    let fallback_idx = match norm {
        CaptureCadence::RealTime { fps } => items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| match it {
                CadenceMenuItem::Option {
                    cadence: CaptureCadence::RealTime { fps: p },
                    ..
                } => Some((i, p.abs_diff(fps))),
                _ => None,
            })
            .min_by_key(|&(_, d)| d)
            .map(|(i, _)| i)
            .unwrap_or(0),
        CaptureCadence::Timelapse { interval_secs } => items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| match it {
                CadenceMenuItem::Option {
                    cadence: CaptureCadence::Timelapse { interval_secs: p },
                    ..
                } => Some((i, p.abs_diff(interval_secs))),
                _ => None,
            })
            .min_by_key(|&(_, d)| d)
            .map(|(i, _)| i)
            .unwrap_or(items.len() - 1),
    };
    (items, fallback_idx)
}

/// Picks the closest Trim & Export editor FPS preset to the source video's nominal FPS
/// (capped at 30 fps for GIF/APNG) so low-FPS clips preserve hardware passthrough by default.
pub fn closest_editor_fps_preset(nominal_fps: f32, is_animated_image: bool) -> u32 {
    let max_fps = if is_animated_image { 30 } else { 60 };
    if !nominal_fps.is_finite() || nominal_fps <= 0.0 {
        return if is_animated_image { 30 } else { 60 };
    }
    let clamped = f64::from(nominal_fps).clamp(1.0, f64::from(max_fps));
    EDITOR_FPS_PRESETS
        .iter()
        .copied()
        .filter(|&p| p <= max_fps)
        .min_by(|&a, &b| {
            let da = (f64::from(a) - clamped).abs();
            let db = (f64::from(b) - clamped).abs();
            da.total_cmp(&db)
        })
        .unwrap_or(if is_animated_image { 30 } else { 60 })
}

/// Computes an aspect-preserving centered destination rectangle `(x, y, width, height)` for drawing
/// a source image `(src_w, src_h)` into a fixed canvas `(dst_w, dst_h)` without stretching.
pub fn aspect_fit_rect(src_w: f64, src_h: f64, dst_w: f64, dst_h: f64) -> (f64, f64, f64, f64) {
    if src_w <= 0.0 || src_h <= 0.0 || dst_w <= 0.0 || dst_h <= 0.0 {
        return (0.0, 0.0, dst_w.max(0.0), dst_h.max(0.0));
    }
    let scale = (dst_w / src_w).min(dst_h / src_h);
    let draw_w = (src_w * scale).min(dst_w);
    let draw_h = (src_h * scale).min(dst_h);
    let x = (dst_w - draw_w) * 0.5;
    let y = (dst_h - draw_h) * 0.5;
    (x, y, draw_w, draw_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_menu_model_contains_realtime_separator_and_timelapse_presets() {
        let (items, idx_60) = build_cadence_menu_model(CaptureCadence::RealTime { fps: 60 });
        assert_eq!(idx_60, 0);
        assert_eq!(items.len(), 10); // 5 real-time + 1 separator + 4 timelapse
        assert_eq!(items[5], CadenceMenuItem::Separator);

        let (_, idx_1fps) = build_cadence_menu_model(CaptureCadence::RealTime { fps: 1 });
        assert_eq!(idx_1fps, 4);

        let (_, idx_1m) = build_cadence_menu_model(CaptureCadence::Timelapse { interval_secs: 60 });
        assert_eq!(idx_1m, 9);
        assert_eq!(
            CaptureCadence::Timelapse { interval_secs: 60 }.menu_label(),
            "1f/1m · TL"
        );

        // Unlisted value snaps cleanly to closest preset in same group
        let (_, idx_24) = build_cadence_menu_model(CaptureCadence::RealTime { fps: 24 });
        assert_eq!(idx_24, 1); // 30 fps
        let (_, idx_45s) =
            build_cadence_menu_model(CaptureCadence::Timelapse { interval_secs: 45 });
        assert_eq!(idx_45s, 8); // 30s
    }

    #[test]
    fn timelapse_always_mutes_audio_while_realtime_preserves_it() {
        let rt = CaptureCadence::RealTime { fps: 1 };
        assert_eq!(
            rt.effective_audio_mode(AudioCaptureMode::Both),
            AudioCaptureMode::Both
        );
        let tl = CaptureCadence::Timelapse { interval_secs: 5 };
        assert_eq!(
            tl.effective_audio_mode(AudioCaptureMode::Both),
            AudioCaptureMode::None
        );
        assert_eq!(
            tl.effective_audio_mode(AudioCaptureMode::Microphone),
            AudioCaptureMode::None
        );
    }

    #[test]
    fn editor_fps_preset_matches_low_fps_and_caps_animated_images() {
        assert_eq!(closest_editor_fps_preset(1.0, false), 1);
        assert_eq!(closest_editor_fps_preset(5.0, false), 5);
        assert_eq!(closest_editor_fps_preset(15.0, false), 15);
        assert_eq!(closest_editor_fps_preset(30.0, false), 30);
        assert_eq!(closest_editor_fps_preset(60.0, false), 60);
        assert_eq!(closest_editor_fps_preset(60.0, true), 30);
        assert_eq!(closest_editor_fps_preset(1.0, true), 1);
    }

    #[test]
    fn aspect_fit_centers_resized_window_frames_cleanly() {
        assert_eq!(
            aspect_fit_rect(1000.0, 500.0, 1000.0, 500.0),
            (0.0, 0.0, 1000.0, 500.0)
        );
        assert_eq!(
            aspect_fit_rect(500.0, 500.0, 1000.0, 500.0),
            (250.0, 0.0, 500.0, 500.0)
        );
        assert_eq!(
            aspect_fit_rect(1000.0, 250.0, 1000.0, 500.0),
            (0.0, 125.0, 1000.0, 250.0)
        );
    }
}
