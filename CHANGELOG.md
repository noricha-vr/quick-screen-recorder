# Changelog

All notable changes to this project will be documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.1.0] - 2026-11-21

### Added

- Native macOS menu-bar recorder built with Rust, AppKit, and ScreenCaptureKit (`SCRecordingOutput`) for low-overhead hardware H.264 / HEVC capture.
- Interactive Area, Window, and Fullscreen selection overlay with aspect-ratio lock (`Free`, `16:9`, `16:10`, `4:3`, `3:2`, `1:1`, `9:16`), exact pixel size fields, cursor/click-highlight toggles, and system/microphone audio options.
- Post-recording Trim & Export editor window with frame scrubbing, resolution presets, one-click clipboard copy (`NSPasteboard`), and disk save (`NSSavePanel`).
- Zero-dependency multithreaded GIF (global NeuQuant palette + frame-delta bounding box optimization) and APNG encoders, hardware-accelerated MP4 / HEVC trimming via `AVAssetExportSession`, and optional WebM (VP9) / AV1 export when `ffmpeg` is installed.
- CLI subcommands (`record`, `convert`, `doctor`, `stats`) and privacy-preserving metadata stripping on exported media.
