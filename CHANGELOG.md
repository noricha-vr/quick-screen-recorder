# Changelog

All notable changes to this project will be documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.2.1] - 2026-11-21

### Added

- User-selectable **`Save to` folder dropdown** in the Trim & Export editor and **`Save Folder` submenu** in the `◉ Rec` menu bar item, with instant switching across `~/Desktop`, `~/Movies`, `~/Downloads`, `~/Documents`, and **`Choose Folder...`** (with Finder **New Folder** creation and live preference sync).

## [0.2.0] - 2026-11-21

### Added

- Click-to-select window picking with live hover preview and window title badge in **Window** capture mode (including click-cycling for overlapping windows).
- Dual-bracket `IN` / `OUT` video trim timeline (`QSRTrimTimelineView`) with time-ruler ticks, dimmed cut zones, live frame playhead scrubbing, and `IN` / `Selected` / `OUT` duration readouts.
- Pre-selected export destination folder workflow defaulting to `~/Desktop`, with a `Folder...` picker persisted in preferences and one-click collision-free `Save` (`Recording-YYYY-MM-DD-HHMMSS.<ext>`).
- Real-time export progress bar and percentage readout across MP4, HEVC, GIF, APNG, WebM, and AV1 exports.

### Fixed

- Fixed interior click-through on macOS borderless transparent overlay windows so **Area** selections can be grabbed anywhere inside the selection box and dragged smoothly across the screen while preserving Retina pixel dimensions.
- Filtered macOS system shell layers (`Dock`, `WindowManager`, `ControlCenter`, `NotificationCenter`) from window hit-testing and clipped off-screen window overhangs accurately.
- Stripped ancillary PNG chunks (`iCCP`, `pHYs`, `cHRM`) when capturing window screenshots so display profile metadata is never embedded.

## [0.1.0] - 2026-11-21

### Added

- Native macOS menu-bar recorder built with Rust, AppKit, and ScreenCaptureKit (`SCRecordingOutput`) for low-overhead hardware H.264 / HEVC capture.
- Interactive Area, Window, and Fullscreen selection overlay with aspect-ratio lock (`Free`, `16:9`, `16:10`, `4:3`, `3:2`, `1:1`, `9:16`), exact pixel size fields, cursor/click-highlight toggles, and system/microphone audio options.
- Post-recording Trim & Export editor window with frame scrubbing, resolution presets, one-click clipboard copy (`NSPasteboard`), and disk save (`NSSavePanel`).
- Zero-dependency multithreaded GIF (global NeuQuant palette + frame-delta bounding box optimization) and APNG encoders, hardware-accelerated MP4 / HEVC trimming via `AVAssetExportSession`, and optional WebM (VP9) / AV1 export when `ffmpeg` is installed.
- CLI subcommands (`record`, `convert`, `doctor`, `stats`) and privacy-preserving metadata stripping on exported media.
