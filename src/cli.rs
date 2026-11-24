use std::path::PathBuf;

use crate::capture::{AudioCaptureMode, RecordingCodec};
use crate::export::ExportFormat;
use crate::geometry::Rect;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Gui {
        open_selector: bool,
        open_editor: Option<PathBuf>,
        demo_mode: Option<DemoMode>,
    },
    Convert {
        input: PathBuf,
        output: PathBuf,
        format: ExportFormat,
        fps: u32,
        start_sec: Option<f64>,
        end_sec: Option<f64>,
        max_width: Option<u32>,
        mute_audio: bool,
    },
    Record {
        output: PathBuf,
        duration_sec: f64,
        region: Option<Rect>,
        fps: u32,
        codec: RecordingCodec,
        show_cursor: bool,
        highlight_clicks: bool,
        audio: AudioCaptureMode,
    },
    Stats,
    Doctor,
    Help,
    Version,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DemoMode {
    Overlay {
        screenshot_out: Option<PathBuf>,
    },
    Editor {
        input: PathBuf,
        screenshot_out: Option<PathBuf>,
    },
    Onboarding {
        granted: bool,
        completed: bool,
        screenshot_out: Option<PathBuf>,
    },
}

pub fn parse_args<I, S>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let raw: Vec<String> = args
        .into_iter()
        .map(Into::into)
        // Filter macOS Finder ProcessSerialNumber flag (-psn_0_...) when launched from .app
        .filter(|a| !a.starts_with("-psn_"))
        .collect();

    if raw.is_empty() {
        return Ok(Command::Gui {
            open_selector: false,
            open_editor: None,
            demo_mode: None,
        });
    }

    match raw[0].as_str() {
        "-h" | "--help" | "help" => Ok(Command::Help),
        "-V" | "--version" | "version" => Ok(Command::Version),
        "stats" => Ok(Command::Stats),
        "doctor" => Ok(Command::Doctor),
        "gui" => parse_gui_flags(&raw[1..]),
        "convert" => parse_convert_flags(&raw[1..]),
        "record" => parse_record_flags(&raw[1..]),
        other if other.starts_with("--") => parse_gui_flags(&raw),
        other => Err(format!(
            "Unknown subcommand '{other}'. Run with --help for usage."
        )),
    }
}

fn parse_gui_flags(args: &[String]) -> Result<Command, String> {
    let mut open_selector = false;
    let mut open_editor = None;
    let mut demo_mode = None;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--select" => open_selector = true,
            "--edit" => {
                i += 1;
                let p = args.get(i).ok_or("--edit requires a video file path")?;
                open_editor = Some(PathBuf::from(p));
            }
            "--demo-overlay" => {
                let mut shot = None;
                if args.get(i + 1).is_some_and(|s| !s.starts_with("--")) {
                    i += 1;
                    shot = Some(PathBuf::from(&args[i]));
                }
                demo_mode = Some(DemoMode::Overlay {
                    screenshot_out: shot,
                });
            }
            "--demo-editor" => {
                i += 1;
                let input = args
                    .get(i)
                    .ok_or("--demo-editor requires an input video path")?;
                let mut shot = None;
                if args.get(i + 1).is_some_and(|s| !s.starts_with("--")) {
                    i += 1;
                    shot = Some(PathBuf::from(&args[i]));
                }
                demo_mode = Some(DemoMode::Editor {
                    input: PathBuf::from(input),
                    screenshot_out: shot,
                });
            }
            "--demo-onboarding" | "--demo-onboarding-ready" | "--demo-onboarding-done" => {
                let flag = args[i].as_str();
                let mut shot = None;
                if args.get(i + 1).is_some_and(|s| !s.starts_with("--")) {
                    i += 1;
                    shot = Some(PathBuf::from(&args[i]));
                }
                demo_mode = Some(DemoMode::Onboarding {
                    granted: flag != "--demo-onboarding",
                    completed: flag == "--demo-onboarding-done",
                    screenshot_out: shot,
                });
            }
            flag => return Err(format!("Unknown GUI option '{flag}'")),
        }
        i += 1;
    }

    Ok(Command::Gui {
        open_selector,
        open_editor,
        demo_mode,
    })
}

fn parse_convert_flags(args: &[String]) -> Result<Command, String> {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut format: Option<ExportFormat> = None;
    let mut fps: u32 = 30;
    let mut start_sec: Option<f64> = None;
    let mut end_sec: Option<f64> = None;
    let mut max_width: Option<u32> = None;
    let mut mute_audio = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-i" | "--input" => {
                i += 1;
                input = Some(PathBuf::from(args.get(i).ok_or("--input requires a path")?));
            }
            "-o" | "--output" => {
                i += 1;
                output = Some(PathBuf::from(
                    args.get(i).ok_or("--output requires a path")?,
                ));
            }
            "-f" | "--format" => {
                i += 1;
                let val = args.get(i).ok_or("--format requires a value")?;
                format = Some(ExportFormat::parse(val)?);
            }
            "--fps" => {
                i += 1;
                fps = args
                    .get(i)
                    .ok_or("--fps requires a number")?
                    .parse::<u32>()
                    .map_err(|_| "Invalid --fps value")?;
                if !(1..=120).contains(&fps) {
                    return Err("--fps must be between 1 and 120".to_string());
                }
            }
            "--start" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or("--start requires seconds")?
                    .parse::<f64>()
                    .map_err(|_| "Invalid --start value")?;
                if !v.is_finite() || v < 0.0 {
                    return Err("--start must be >= 0".to_string());
                }
                start_sec = Some(v);
            }
            "--end" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or("--end requires seconds")?
                    .parse::<f64>()
                    .map_err(|_| "Invalid --end value")?;
                if !v.is_finite() || v <= 0.0 {
                    return Err("--end must be > 0".to_string());
                }
                end_sec = Some(v);
            }
            "--max-width" => {
                i += 1;
                let w = args
                    .get(i)
                    .ok_or("--max-width requires pixels")?
                    .parse::<u32>()
                    .map_err(|_| "Invalid --max-width value")?;
                if w < 32 {
                    return Err("--max-width must be >= 32".to_string());
                }
                max_width = Some(w);
            }
            "--mute" => mute_audio = true,
            other => return Err(format!("Unknown convert option '{other}'")),
        }
        i += 1;
    }

    let input = input.ok_or("Missing --input <path>")?;
    let output = output.ok_or("Missing --output <path>")?;
    let inferred_format = match format {
        Some(f) => f,
        None => output
            .extension()
            .and_then(|e| e.to_str())
            .ok_or_else(|| {
                "Specify --format or use an output extension (.mp4, .gif, .apng, .webm)".to_string()
            })
            .and_then(ExportFormat::parse)?,
    };

    if let (Some(s), Some(e)) = (start_sec, end_sec) {
        if e <= s {
            return Err("--end must be greater than --start".to_string());
        }
    }

    Ok(Command::Convert {
        input,
        output,
        format: inferred_format,
        fps,
        start_sec,
        end_sec,
        max_width,
        mute_audio,
    })
}

fn parse_record_flags(args: &[String]) -> Result<Command, String> {
    let mut output: Option<PathBuf> = None;
    let mut duration_sec: f64 = 5.0;
    let mut region: Option<Rect> = None;
    let mut fps: u32 = 60;
    let mut codec = RecordingCodec::H264;
    let mut show_cursor = true;
    let mut highlight_clicks = true;
    let mut audio = AudioCaptureMode::None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--output" => {
                i += 1;
                output = Some(PathBuf::from(
                    args.get(i).ok_or("--output requires a path")?,
                ));
            }
            "-d" | "--duration" => {
                i += 1;
                duration_sec = args
                    .get(i)
                    .ok_or("--duration requires seconds")?
                    .parse::<f64>()
                    .map_err(|_| "Invalid --duration value")?;
                if !duration_sec.is_finite() || !(0.2..=3600.0).contains(&duration_sec) {
                    return Err("--duration must be between 0.2 and 3600 seconds".to_string());
                }
            }
            "--region" => {
                i += 1;
                region = Some(parse_region(
                    args.get(i).ok_or("--region requires x,y,w,h")?,
                )?);
            }
            "--fps" => {
                i += 1;
                fps = args
                    .get(i)
                    .ok_or("--fps requires a number")?
                    .parse::<u32>()
                    .map_err(|_| "Invalid --fps")?;
                if !(1..=120).contains(&fps) {
                    return Err("--fps must be between 1 and 120".to_string());
                }
            }
            "--codec" => {
                i += 1;
                codec = match args.get(i).map(String::as_str) {
                    Some("h264") => RecordingCodec::H264,
                    Some("hevc" | "h265") => RecordingCodec::Hevc,
                    _ => return Err("--codec must be h264 or hevc".to_string()),
                };
            }
            "--no-cursor" => show_cursor = false,
            "--no-clicks" => highlight_clicks = false,
            "--audio" => {
                i += 1;
                audio = match args.get(i).map(String::as_str) {
                    Some("none") => AudioCaptureMode::None,
                    Some("system") => AudioCaptureMode::SystemAudio,
                    Some("mic") => AudioCaptureMode::Microphone,
                    Some("both") => AudioCaptureMode::Both,
                    _ => return Err("--audio must be none, system, mic, or both".to_string()),
                };
            }
            other => return Err(format!("Unknown record option '{other}'")),
        }
        i += 1;
    }

    let output = output.ok_or("Missing --output <path.mp4>")?;
    Ok(Command::Record {
        output,
        duration_sec,
        region,
        fps,
        codec,
        show_cursor,
        highlight_clicks,
        audio,
    })
}

fn parse_region(spec: &str) -> Result<Rect, String> {
    let parts: Vec<f64> = spec
        .split(',')
        .map(|s| s.trim().parse::<f64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "Invalid --region format; expected x,y,width,height")?;
    if parts.len() != 4 || parts[2] < 32.0 || parts[3] < 32.0 {
        return Err("Region must be x,y,width,height with width/height >= 32".to_string());
    }
    Ok(Rect::new(parts[0], parts[1], parts[2], parts[3]))
}

pub fn usage_text() -> &'static str {
    "\
Quick Screen Recorder — Fast native screen recorder for macOS (ScreenCaptureKit + Rust)

USAGE:
  quick-screen-recorder [--select | --edit <video.mp4>]
  quick-screen-recorder record --output <out.mp4> [--duration <sec>] [--region x,y,w,h] [--fps 60] [--codec h264|hevc] [--audio none|system|mic|both] [--no-cursor] [--no-clicks]
  quick-screen-recorder convert --input <in.mp4> --output <out.ext> [--format mp4|hevc|gif|apng|webm|av1] [--fps 30] [--start <sec>] [--end <sec>] [--max-width <px>] [--mute]
  quick-screen-recorder doctor
  quick-screen-recorder stats
"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_launches_menubar_gui_and_ignores_finder_psn() {
        let cmd = parse_args(["-psn_0_12345"]).expect("parse psn");
        assert_eq!(
            cmd,
            Command::Gui {
                open_selector: false,
                open_editor: None,
                demo_mode: None,
            }
        );
    }

    #[test]
    fn parse_convert_infers_gif_and_validates_trim() {
        let cmd = parse_args([
            "convert",
            "--input",
            "/tmp/in.mp4",
            "--output",
            "/tmp/out.gif",
            "--fps",
            "24",
            "--start",
            "0.5",
            "--end",
            "2.5",
            "--max-width",
            "640",
        ])
        .expect("parse convert");

        assert_eq!(
            cmd,
            Command::Convert {
                input: PathBuf::from("/tmp/in.mp4"),
                output: PathBuf::from("/tmp/out.gif"),
                format: ExportFormat::Gif,
                fps: 24,
                start_sec: Some(0.5),
                end_sec: Some(2.5),
                max_width: Some(640),
                mute_audio: false,
            }
        );

        assert!(
            parse_args([
                "convert",
                "-i",
                "/tmp/in.mp4",
                "-o",
                "/tmp/out.gif",
                "--start",
                "3.0",
                "--end",
                "1.0",
            ])
            .is_err()
        );
    }
}
