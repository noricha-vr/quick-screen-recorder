# Changelog

All notable changes to this project will be documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.5.3] - 2026-11-24

### Changed

- **Onboarding completion footer buttons**:
  - Promoted `Show in Finder` to the right-side primary button (`Return`), which reveals the saved clip in Finder and automatically closes the Quick Start guide.
  - Replaced `Done` with `Close` (`Esc`) as the left-side secondary button.

## [0.5.2] - 2026-11-24

### Fixed

- **Screen-lock & dual-audio recording finalization error (`最初のサンプルバッファの処理が出来ないため、うまくいきませんでした` / `-3811`)**:
  - Replaced the fixed 2-second `STOP_FALLBACK_AFTER` timer with a shared mutex-backed `StopControl` and single serialized background stop worker (`qsr-stop-worker`) that waits for `SCRecordingOutput` / `RPAudioMixUtility` offline audio mixing (`System Audio + Mic`) to finish before sending `SCStream::stopCaptureWithCompletionHandler:`, using a dynamic finalization budget scaled to recording duration and effective audio mode (`60–600s` for dual audio, `30–180s` otherwise).
  - Routed delayed `startCapture` post-cancellation teardown through the same serialized stop worker (preventing double `removeRecordingOutput` / `exportAndInvalidateWithHandler:` races and `replayd` crashes).
  - Guaranteed that timeout-forced stops and unconfirmed stream stops always retain the staged MP4 untouched in place (`.qsr-keep`) instead of moving or deleting a file while `replayd` may still be flushing.
- **Window-mode hover leak behind toolbar & inactive toolbar appearance**:
  - Made the floating `toolbar_window` the active key window while keeping the live `canvas_window` non-key (`acceptsFirstMouse` preserves single-click selection and dragging), so toolbar controls render in active colors immediately and `Esc` / `Return` shortcuts work cleanly without closing open popup menus.
  - Added `NSTrackingArea` tracking and canonical coordinate-based toolbar exclusion on both `OverlayCanvasView` and `ToolbarContainerView` so hovering over the floating toolbar immediately clears any background window highlight and never shows a behind-toolbar selection frame.
  - Committed active width/height text edits before borrowing `OverlayState` on canvas click or `● Record` click to eliminate re-entrancy risks.

### Added

- **Local macOS install script with target-scoped ad-hoc TCC auto-reset (`make install` / `scripts/install-local.sh`)**:
  - Safely builds and atomically swaps `/Applications/QuickScreenRecorder.app` (refusing to overwrite a running capture process or a mismatched `CFBundleIdentifier`, and keeping `dist/` strictly ad-hoc signed).
  - Automatically resets only the app's explicit macOS TCC entries (`ScreenCapture`, `Microphone`) when an ad-hoc install's Designated Requirement / `CDHash` changes, and preserves existing grants when locally signed with `--sign-identity <id>`.
  - Added local-only finalization telemetry fields (`finalize_ms`, `audio_mode`, `forced_timeout`) to `perf.jsonl` (never storing paths, window titles, or personal identifiers).

## [0.5.1] - 2026-11-24

### Added

- **Automatic stop & save on macOS screen lock, user switch, and sleep**:
  - Monitors macOS `com.apple.screenIsLocked`, `NSWorkspaceSessionDidResignActiveNotification`, `NSWorkspaceScreensDidSleepNotification`, and `NSWorkspaceWillSleepNotification` alongside an independent 100ms `CGSessionCopyCurrentDictionary` watchdog thread across both real-time and interval timelapse recordings.
  - Stops capture automatically upon lock/sleep detection (with timelapse additionally re-checking session lock state before dispatching and before writing each frame), finalizes the MP4 up to detection, preserves GUI auto-stopped recordings in `~/Library/Application Support/QuickScreenRecorder/Recovered` (exempt from cache pruning and reboot loss), and opens them in the Trim & Export editor with an inline non-modal status notice.
  - Headless CLI `record` sessions also pump the current runloop and stop cleanly on lock/sleep, printing the auto-stop reason and saved file path.

### Fixed

- Added unified local recovery preservation (`~/Library/Application Support/QuickScreenRecorder/Recovered` and cache-prune-exempt staging retention) and early-stop detection (`has_ended_unexpectedly`) to real-time `SCRecordingOutput` recordings, matching timelapse safety guarantees.

## [0.5.0] - 2026-11-24

### Added

- **Low-FPS real-time capture (`1 / 5 / 15 / 30 / 60 fps`)**:
  - Expanded the overlay frame-rate selector to **`60 / 30 / 15 / 5 / 1 fps`** real-time capture (persisted across launches via `capture_cadence` in `preferences.json`) with full System Audio + Microphone support.
  - Expanded Trim & Export editor FPS presets to **`1 / 5 / 15 / 30 / 60 fps`** and defaulted the editor FPS to the closest preset matching the source clip so `1 fps` / `5 fps` recordings preserve instant hardware passthrough without inflating frame rate.
- **Interval Timelapse capture (`1f/5s`, `1f/10s`, `1f/30s`, `1f/1m` -> 30 fps fast-forward MP4)**:
  - Added `TimelapseSession` (`SCScreenshotManager` periodic sampling + incremental `StreamingMp4Writer` hardware H.264/HEVC encoding at **30 fps** playback) for multi-hour recordings with near-zero idle CPU and O(1) memory, aspect-fit handling for resized windows, and non-blocking stop finalization.
  - Automatically mutes and disables `System Audio` / `Mic` toolbar controls while a timelapse preset is selected (restoring the user's saved audio settings immediately when switching back to real-time FPS) and displays elapsed time + frame count (`● TL MM:SS · Nf`) in the menu bar.
- **CLI `--interval <sec>` timelapse flag**:
  - Added `quick-screen-recorder record --interval <1..=3600>` (and extended `--duration` up to `86400` seconds / 24 hours) for scriptable headless timelapse capture.

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
