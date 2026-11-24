# Changelog

All notable changes to this project will be documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.4.0] - 2026-11-24

### Added

- **High-contrast 2-row floating capture toolbar with smart target following**:
  - Automatically positions directly below the active Area or Window selection (`12pt` gap), flips above when near the macOS Dock (`NSScreen::visibleFrame()`), and floats cleanly inside fullscreen targets.
  - Uses an elevated slate surface with an outer dark rim, bright inner highlight rim, and native window shadow so it remains clearly legible over both dark and bright screens.
- **Split audio controls & persisted defaults**:
  - Separated capture audio into an independent **`System Audio`** checkbox (default **ON**) and a **`Mic`** device dropdown (`Off`, `System (<default mic>)` default **ON**, plus individual connected microphones via `AVCaptureDeviceDiscoverySession`), with selections persisted across launches and graceful fallback if a saved microphone is unplugged.
  - Requests microphone permission natively via async `AVCaptureDevice` prompt only when starting a mic-enabled capture (keeping first-run onboarding focused solely on Screen Recording), with a helpful `Record Without Mic` / `Open Microphone Settings` alert if previously denied.
- **Post-capture `Mute audio` export option**:
  - Added a **`Mute audio`** checkbox to the Trim & Export editor (disabled automatically for silent sources or GIF/APNG) and a `--mute` flag to `quick-screen-recorder convert` to strip audio tracks cleanly on MP4, HEVC, WebM, and AV1 exports.

### Changed

- Rounded selection overlay corners to a subtle previous-gen macOS **10pt** radius (with `EvenOdd` outer dim cutout, clipped thirds grid, and arc-seated handles; kept square in full-screen mode).
- Moved the onboarding **`Open Settings →`** action button directly inside the top amber Screen Recording status banner and replaced the duplicate bottom-right button with a quiet `"Waiting for Screen Recording permission..."` status line during the permission step.

### Fixed

- Fixed **`Recording Error: Timed out stopping capture stream`** when finishing recordings with System Audio + Microphone enabled by replacing synchronous main-thread blocking teardown with non-blocking `RecordingSession::begin_stop()` + `NSTimer` polling so CoreAudio and ScreenCaptureKit flush final frames cleanly without stalling the AppKit runloop.

## [0.3.0] - 2026-11-24

### Added

- **First-run Screen Recording onboarding & interactive tutorial** (`OnboardingWindow` + `QSRTutorialVisualView`):
  - Opens automatically on first launch (and anytime via **`◉ Rec` → `Quick Start Guide...`** or when attempting to record without permission).
  - Live permission status card (amber **Not Enabled** -> vivid green **Enabled** with live polling and instant update after macOS **Quit & Reopen**) plus a one-click **`Open Screen Recording Settings`** button linking directly to **System Settings > Privacy & Security > Screen & System Audio Recording**.
  - Animated CoreGraphics walkthrough illustrating the top-right `◉ Rec` menu bar dropdown -> `New Recording...` (`⇧⌘6`) -> `Stop Recording` -> export flow, concluding with a green completion card and **`Show in Finder`** action.
- **Editable `Name` field & live extension badge** in the Trim & Export editor, pre-filled with chronological, Spotlight-friendly names (`YYYY-MM-DD_HH-MM-SS_Quick-Screen-Recorder`, plus sanitized target app name on Window captures) with path-traversal, dotfile, bidi, duplicate-extension, and UTF-8 byte-length sanitization.

### Changed

- Changed the default export directory from `~/Desktop` to **`~/Movies/Quick Screen Recorder`** (auto-created privately if absent) and placed it first in both the editor `Save to` dropdown and the menu bar `Save Folder` submenu.
- Preserved numeric suffixes in custom export filenames during atomic collision numbering (e.g., `demo-2026.mp4` collides to `demo-2026-2.mp4` instead of stripping `-2026`).

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
