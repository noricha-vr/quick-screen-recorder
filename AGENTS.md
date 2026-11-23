# AGENTS.md

Development guide for contributors and coding agents working in `quick-screen-recorder`.

## Overview

`quick-screen-recorder` is a macOS 15+ native screen recorder and GIF/MP4/HEVC/APNG/WebM/AV1 exporter written in Rust. It uses `objc2-screen-capture-kit`, `objc2-av-foundation`, and `objc2-app-kit` directly without Electron or WebViews.

## Repository Layout

| Path | Responsibility |
|------|----------------|
| `src/main.rs`, `src/cli.rs` | Binary entry point and zero-dependency CLI flag parser |
| `src/geometry.rs` | Pure coordinate conversion (AppKit Y-up <-> CoreGraphics Y-down), aspect-ratio locking, even-pixel alignment, and 8-way resize hit-testing |
| `src/config.rs` | Local user preferences serialization (`~/Library/Application Support/QuickScreenRecorder/preferences.json`) |
| `src/export_name.rs` | Chronological Spotlight-friendly export filename generation and user filename sanitization |
| `src/observe.rs` | Local-only latency/size stats (`perf.jsonl`) with zero personal paths or titles |
| `src/permissions.rs` | CoreGraphics Screen Recording permission checks, System Settings deep link, Finder reveal, and `doctor` diagnostics |
| `src/capture/` | `SCShareableContent` enumeration and `SCStream` + `SCRecordingOutput` hardware capture session |
| `src/export/` | `AVAssetExportSession` MP4/HEVC trimming, `AVAssetImageGenerator` frame extraction, parallel Rust GIF/APNG encoders, and optional `ffmpeg` WebM/AV1 runner |
| `src/ui/` | Native AppKit status-bar controller, first-run onboarding & animated tutorial, selection overlay, and Trim & Export editor window |
| `tests/export_pipeline.rs` | End-to-end export and privacy-invariant integration tests |
| `scripts/bundle-app.sh` | Packages `dist/QuickScreenRecorder.app` with generated `.icns` icon and ad-hoc signature |

## Verification Commands

```bash
make check    # Runs cargo fmt --check, cargo clippy --locked -- -D warnings, and cargo test
make bundle   # Builds release binary and packages dist/QuickScreenRecorder.app
```

## Design & Security Invariants

- **Keep files focused (< 500 lines)** and keep pure geometry/quantization/CLI logic unit-testable without requiring TCC GUI permissions.
- **Permissive licenses only**: Every crate dependency must be compatible with MIT distribution (MIT / Apache-2.0 / BSD / Zlib). Never add GPL/AGPL crates.
- **Privacy**: Never add network telemetry, never embed machine-specific paths or personal certificate identities, never log file paths or window titles in `perf.jsonl`, and always strip container metadata on export.
- **Subprocess safety**: When invoking optional `ffmpeg`, resolve executable paths directly and pass argument arrays (`std::process::Command`) without shell interpolation; guard relative paths starting with `-`.
