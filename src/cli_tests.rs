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

#[test]
fn parse_record_supports_low_fps_and_timelapse_interval() {
    let tl = parse_args([
        "record",
        "-o",
        "/tmp/tl.mp4",
        "--interval",
        "60",
        "-d",
        "180",
    ])
    .expect("parse timelapse");
    assert!(matches!(
        tl,
        Command::Record {
            cadence: CaptureCadence::Timelapse { interval_secs: 60 },
            ..
        }
    ));
    assert!(
        parse_args([
            "record",
            "-o",
            "/tmp/a.mp4",
            "--fps",
            "1",
            "--interval",
            "5"
        ])
        .is_err()
    );
    assert!(
        parse_args([
            "record",
            "-o",
            "/tmp/a.mp4",
            "--interval",
            "5",
            "--audio",
            "system"
        ])
        .is_err()
    );
}
