# Quick Screen Recorder

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform: macOS 15+](https://img.shields.io/badge/platform-macOS%2015%2B-lightgrey.svg)](https://www.apple.com/macos/)
[![Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org/)

[日本語ドキュメント (Japanese)](README.ja.md)

A fast, lightweight native screen recorder for macOS built in pure Rust on top of Apple's **ScreenCaptureKit**, **AVFoundation / VideoToolbox**, and **AppKit**.

Unlike Electron-based recorders that keep a full Chromium runtime resident in memory and rely on single-threaded software post-processing, Quick Screen Recorder runs with a ~20 MB native footprint, encodes H.264 / HEVC directly on Apple Silicon hardware during capture, and exports GIF / APNG in parallel across CPU cores with zero external runtime dependencies.

## Screenshots

| Region / Window / Screen Selector | Trim & Multi-Format Export Editor |
|---|---|
| ![Selection Overlay](docs/images/overlay.png) | ![Trim and Export Editor](docs/images/editor.png) |

## Highlights

- **Zero Electron / Zero WebView** — Pure Rust + native AppKit status-bar app and borderless overlay. Instant startup and minimal idle memory.
- **Hardware-Direct Recording (`SCRecordingOutput`)** — Streams selected regions, individual windows, or full displays directly to hardware H.264 / HEVC MP4 with near-zero CPU overhead.
- **Flexible Capture Selector**
  - **Area mode**: Drag to create, grab inside to reposition, 8-way resize handles, aspect-ratio lock (`Free`, `16:9`, `16:10`, `4:3`, `3:2`, `1:1`, `9:16`), and exact `W × H` pixel input.
  - **Window mode**: Hover and click any on-screen window to switch the capture target on the fly (overlapping windows are excluded automatically during capture).
  - **Screen mode**: Full-display capture across multi-monitor setups.
  - Toggle **cursor visibility**, **native click highlights**, **system audio**, and **microphone** right from the floating bar.
- **Instant Trim & Multi-Format Export Editor**
  - **Dual-bracket `IN` / `OUT` timeline**: Drag rectangular start/end trim brackets or scrub the playhead with live hardware-decoded frame preview.
  - **Zero-dependency native exports**: **MP4 (H.264)**, **MP4 (HEVC)**, **GIF** (multithreaded NeuQuant global palette + frame-delta bounding-box optimization), and **APNG**.
  - **Optional FFmpeg exports**: Automatically unlocks **WebM (VP9)** and **MP4 (AV1)** when `ffmpeg` is installed on your Mac.
  - **Instant Desktop Save & Real-Time Progress**: Defaults to `~/Desktop` (customizable ahead of time via **`Folder...`**) so clicking **`Save`** writes a non-colliding timestamped file immediately with a live progress bar—or click **`Copy`** to place the file directly on the macOS Pasteboard.
- **Privacy by Design**
  - 100% local execution with **zero network access** and **zero external telemetry**.
  - Container metadata is stripped automatically on export (`setMetadata(None)` / `-map_metadata -1`).
  - Local performance stats (`stats` command) record only format, duration, and encode latency—never file paths, window titles, or personal identifiers.

## Requirements

- **macOS 15.0 (Sequoia) or later** (Apple Silicon or Intel)
- **Rust 1.85+** (`rustup` recommended) for building from source
- Optional: `ffmpeg` (`brew install ffmpeg`) only if you need **WebM (VP9)** or **AV1** export

## Build & Install

```bash
git clone https://github.com/noricha-vr/quick-screen-recorder.git
cd quick-screen-recorder

# Build release .app bundle into dist/QuickScreenRecorder.app
make bundle

# Launch the menu-bar app
open dist/QuickScreenRecorder.app

# Or move to /Applications
cp -R dist/QuickScreenRecorder.app /Applications/
```

On first capture, macOS will prompt for **Screen & System Audio Recording** permission under **System Settings > Privacy & Security**.

## Usage

### Menu Bar & Shortcut (GUI)

1. Launch `QuickScreenRecorder.app` (or run `cargo run --release`). Look for **`◉ Rec`** in the macOS menu bar.
2. Click **`◉ Rec` → `New Recording...`** or press **`Cmd + Shift + 6`** anywhere to open the selection overlay (`Esc` cancels).
3. Choose **Area**, **Window**, or **Screen**, adjust aspect ratio / dimensions / FPS (`30` or `60`) / audio / cursor / click highlights, and click **`● Record`**.
4. During recording, the menu bar displays elapsed time (`● 00:08`). Click **`Stop Recording`** in the menu bar or press **`Cmd + Shift + 6`** again to finish.
5. In the **Trim & Export** window, drag the **`IN` / `OUT`** trim brackets if needed, pick your format (**MP4**, **HEVC**, **GIF**, **APNG**, **WebM**, **AV1**) and resolution preset, optionally change the destination folder via **`Folder...`** (defaults to `~/Desktop`), then click **`Copy`** or **`Save`**.

### Command-Line Interface (CLI)

The same binary also provides scriptable CLI subcommands:

```bash
# Check Screen Recording permission and codec availability
quick-screen-recorder doctor

# Open the region selector immediately from terminal
quick-screen-recorder --select

# Open an existing video in the Trim & Export editor
quick-screen-recorder --edit ./demo.mp4

# Headless timed capture (e.g. 5s of a 1280x720 region at 60fps)
quick-screen-recorder record --output ./capture.mp4 --duration 5 --region 100,100,1280,720 --fps 60

# Fast format conversion & trimming (e.g. MP4 -> optimized 30fps GIF, max width 800px)
quick-screen-recorder convert --input ./capture.mp4 --output ./clip.gif --format gif --fps 30 --start 0.5 --end 4.0 --max-width 800

# View local-only export/capture latency percentiles
quick-screen-recorder stats
```

## Development

```bash
make check    # cargo fmt --check + cargo clippy (-D warnings) + unit & integration tests
make bundle   # Build release binary and package dist/QuickScreenRecorder.app
```

## License

Released under the [MIT License](LICENSE).
