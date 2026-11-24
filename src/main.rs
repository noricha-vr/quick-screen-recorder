use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use quick_screen_recorder::capture::recorder::RecordingSession;
use quick_screen_recorder::capture::shareable::query_shareable_content;
use quick_screen_recorder::capture::{CaptureConfig, CaptureTarget};
use quick_screen_recorder::cli::{Command, parse_args, usage_text};
use quick_screen_recorder::export::{ExportRequest, run_export};
use quick_screen_recorder::fs_safety::sanitize_terminal_text;
use quick_screen_recorder::observe::{
    default_perf_log_path, format_perf_summary, summarize_perf_log,
};
use quick_screen_recorder::permissions::doctor_report;
use quick_screen_recorder::ui::run_gui;

fn main() -> ExitCode {
    let args = std::env::args().skip(1);
    let command = match parse_args(args) {
        Ok(c) => c,
        Err(err) => {
            let safe = sanitize_terminal_text(&err, false);
            eprintln!("error: {safe}\n\n{}", usage_text());
            return ExitCode::from(2);
        }
    };

    if let Err(err) = execute(command) {
        let safe = sanitize_terminal_text(&err, false);
        eprintln!("error: {safe}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn execute(command: Command) -> Result<(), String> {
    match command {
        Command::Help => {
            print!("{}", usage_text());
            Ok(())
        }
        Command::Version => {
            println!("quick-screen-recorder {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Doctor => {
            println!("{}", doctor_report());
            Ok(())
        }
        Command::Stats => {
            let path = default_perf_log_path().ok_or("HOME is not set")?;
            let summary =
                summarize_perf_log(&path).map_err(|e| format!("Failed reading perf log: {e}"))?;
            print!("{}", format_perf_summary(&summary));
            Ok(())
        }
        Command::Convert {
            input,
            output,
            format,
            fps,
            start_sec,
            end_sec,
            max_width,
            mute_audio,
        } => {
            let res = run_export(&ExportRequest {
                input,
                output,
                format,
                fps,
                start_sec,
                end_sec,
                max_width,
                mute_audio,
            })?;
            let safe_out = sanitize_terminal_text(&res.output_path.display().to_string(), false);
            println!(
                "Exported {} ({} bytes) in {} ms -> {}",
                format.label(),
                res.output_bytes,
                res.elapsed_ms,
                safe_out
            );
            Ok(())
        }
        Command::Record {
            output,
            duration_sec,
            region,
            fps,
            codec,
            show_cursor,
            highlight_clicks,
            audio,
        } => {
            let snap = query_shareable_content()?;
            let primary = snap
                .displays
                .first()
                .ok_or("No active display found for recording")?;
            let target = match region {
                Some(rect) => CaptureTarget::Region {
                    display_id: primary.display_id,
                    rect_points: rect,
                    // 0.0 delegates to ScreenCaptureKit's native `pointPixelScale` (1x on SDR/non-Retina, 2x on Retina)
                    scale_factor: 0.0,
                },
                None => CaptureTarget::Display {
                    display_id: primary.display_id,
                    scale_factor: 0.0,
                },
            };
            let cfg = CaptureConfig {
                target,
                fps,
                codec,
                show_cursor,
                highlight_clicks,
                audio_mode: audio,
                microphone_device_id: None,
                excluded_window_ids: Vec::new(),
                source_app_name: None,
            };
            let session = RecordingSession::start(&cfg, &output)?;
            thread::sleep(Duration::from_secs_f64(duration_sec));
            let saved = session.stop()?;
            let safe_saved = sanitize_terminal_text(&saved.display().to_string(), false);
            println!("Recorded -> {safe_saved}");
            Ok(())
        }
        Command::Gui {
            open_selector,
            open_editor,
            demo_mode,
        } => run_gui(open_selector, open_editor, demo_mode),
    }
}
