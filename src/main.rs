use std::process::ExitCode;
use std::time::{Duration, Instant};

use quick_screen_recorder::capture::shareable::query_shareable_content;
use quick_screen_recorder::capture::{
    ActiveRecording, CaptureConfig, CaptureTarget, SystemInterruptMonitor, pump_current_runloop,
};
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
            cadence,
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
                cadence,
                codec,
                show_cursor,
                highlight_clicks,
                audio_mode: cadence.effective_audio_mode(audio),
                microphone_device_id: None,
                excluded_window_ids: Vec::new(),
                source_app_name: None,
            };
            // Arm system-interrupt monitoring BEFORE startup so lock/sleep events during
            // initialization are captured rather than missed.
            let monitor = SystemInterruptMonitor::start_headless();
            let session =
                ActiveRecording::start_with_interrupt(&cfg, &output, Some(monitor.latch()))?;
            monitor.attach_stop_hook(session.stop_hook());
            let deadline = Instant::now() + Duration::from_secs_f64(duration_sec);
            let mut interrupted_by = monitor.poll_interrupt();
            while interrupted_by.is_none()
                && Instant::now() < deadline
                && !session.has_ended_unexpectedly()
            {
                let remaining = deadline.saturating_duration_since(Instant::now());
                pump_current_runloop(remaining.min(Duration::from_millis(250)));
                if let Some(reason) = monitor.poll_interrupt() {
                    interrupted_by = Some(reason);
                    break;
                }
            }
            if interrupted_by.is_none() {
                interrupted_by = monitor.poll_interrupt();
            }
            drop(monitor);
            let outcome = session.stop()?;
            let safe_saved =
                sanitize_terminal_text(&outcome.output_path.display().to_string(), false);
            if let Some(partial_err) = outcome.partial_error {
                println!("Saved recording -> {safe_saved}");
                return Err(partial_err);
            }
            if let Some(reason) = interrupted_by {
                println!("Auto-stopped ({}) -> {safe_saved}", reason.short_label());
            } else {
                println!("Recorded -> {safe_saved}");
            }
            Ok(())
        }
        Command::Gui {
            open_selector,
            open_editor,
            demo_mode,
        } => run_gui(open_selector, open_editor, demo_mode),
    }
}
