use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use quick_screen_recorder::capture::RecordingCodec;
use quick_screen_recorder::export::av_writer::write_rgba_frames_to_mp4;
use quick_screen_recorder::export::avfoundation::{
    RgbaFrame, extract_rgba_frame, probe_video_metadata,
};
use quick_screen_recorder::export::ffmpeg::detect_ffmpeg_binary;
use quick_screen_recorder::export::{
    ExportFormat, ExportRequest, run_export, run_export_with_progress,
};
use quick_screen_recorder::fs_safety::next_unique_token;
use quick_screen_recorder::observe::summarize_perf_log;

fn make_temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qsr-it-{name}-{}", next_unique_token()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp test dir");
    dir
}

/// Generates a deterministic 640x360 @ 30fps (1.5s = 45 frames) test MP4 using pure native AVAssetWriter,
/// so CI runs every native test even when `ffmpeg` is absent.
fn generate_native_mp4_fixture(out_path: &Path) {
    let w = 640_u32;
    let h = 360_u32;
    let frames: Vec<RgbaFrame> = (0..45_u32)
        .map(|f| {
            let mut pixels = vec![0_u8; (w * h * 4) as usize];
            let bar_x = ((f * 12) % (w - 80)) as usize;
            for y in 0..(h as usize) {
                for x in 0..(w as usize) {
                    let idx = (y * (w as usize) + x) * 4;
                    let in_box = (bar_x..bar_x + 80).contains(&x) && (100..260).contains(&y);
                    pixels[idx] = if in_box { 245 } else { 28 };
                    pixels[idx + 1] = if in_box {
                        90
                    } else {
                        ((y * 200) / (h as usize)) as u8
                    };
                    pixels[idx + 2] = if in_box { 70 } else { 140 };
                    pixels[idx + 3] = 255;
                }
            }
            RgbaFrame {
                width: w,
                height: h,
                pixels,
            }
        })
        .collect();

    write_rgba_frames_to_mp4(&frames, 30, RecordingCodec::H264, out_path)
        .expect("write native MP4 fixture");
}

#[test]
fn end_to_end_native_exports_codec_resize_metadata_and_telemetry() {
    let work = make_temp_dir("pipeline");
    let perf_log = work.join("perf.jsonl");
    unsafe {
        std::env::set_var("QSR_PERF_LOG", &perf_log);
    }

    let src_mp4 = work.join("source.mp4");
    generate_native_mp4_fixture(&src_mp4);

    let src_meta = probe_video_metadata(&src_mp4).expect("probe source mp4");
    assert_eq!(src_meta.width, 640);
    assert_eq!(src_meta.height, 360);
    assert!(src_meta.is_h264(), "expected source fixture to be H.264");
    assert_eq!(
        src_meta.reference_restrictions, 0xFFFF,
        "AVURLAsset must enforce ForbidAll external reference restrictions"
    );
    assert!(src_meta.duration_sec >= 1.3 && src_meta.duration_sec <= 1.7);

    // Reject overwriting input via hardlink or identical path
    let hardlink = work.join("same-inode.mp4");
    fs::hard_link(&src_mp4, &hardlink).expect("create hardlink");
    let same_err = run_export(&ExportRequest {
        input: src_mp4.clone(),
        output: hardlink,
        format: ExportFormat::Mp4,
        fps: 30,
        start_sec: None,
        end_sec: None,
        max_width: None,
    });
    assert!(same_err.is_err(), "must block overwriting same inode");
    assert!(src_mp4.exists(), "original source must remain intact");

    let frame = extract_rgba_frame(&src_mp4, 0.4, Some(320)).expect("extract scaled frame");
    assert_eq!(frame.width, 320);
    assert_eq!(frame.height, 180);

    // 1. Trimmed MP4 (Passthrough + metadata strip)
    let out_mp4 = work.join("trimmed.mp4");
    let mp4_res = run_export(&ExportRequest {
        input: src_mp4.clone(),
        output: out_mp4.clone(),
        format: ExportFormat::Mp4,
        fps: 30,
        start_sec: Some(0.2),
        end_sec: Some(1.1),
        max_width: None,
    })
    .expect("export trimmed mp4");
    assert!(mp4_res.output_bytes > 500);
    let trimmed_meta = probe_video_metadata(&out_mp4).expect("probe trimmed mp4");
    assert!(trimmed_meta.is_h264());
    assert_eq!(trimmed_meta.common_metadata_count, 0);

    // 2. Native HEVC transcode with custom max_width=400 (640x360 -> 400x224 exact) and 15fps
    let out_hevc = work.join("resized_hevc.mp4");
    let hevc_res = run_export(&ExportRequest {
        input: src_mp4.clone(),
        output: out_hevc.clone(),
        format: ExportFormat::Hevc,
        fps: 15,
        start_sec: Some(0.0),
        end_sec: Some(1.0),
        max_width: Some(400),
    })
    .expect("export custom-width hevc");
    assert!(hevc_res.output_bytes > 500);
    let hevc_meta = probe_video_metadata(&out_hevc).expect("probe hevc");
    assert!(hevc_meta.is_hevc(), "expected HEVC codec FourCC");
    assert_eq!(hevc_meta.width, 400);
    assert_eq!(hevc_meta.height, 224);
    assert!((hevc_meta.nominal_fps - 15.0).abs() <= 1.5);
    assert_eq!(hevc_meta.common_metadata_count, 0);

    // 3. HEVC -> H.264 reverse transcode verifies H.264 codec is actually produced (not left as HEVC)
    let back_h264 = work.join("from_hevc_to_h264.mp4");
    run_export(&ExportRequest {
        input: out_hevc.clone(),
        output: back_h264.clone(),
        format: ExportFormat::Mp4,
        fps: 15,
        start_sec: None,
        end_sec: None,
        max_width: None,
    })
    .expect("transcode hevc back to h264");
    let back_meta = probe_video_metadata(&back_h264).expect("probe back_h264");
    assert!(
        back_meta.is_h264(),
        "expected H.264 after converting from HEVC"
    );

    // 4. Native GIF export (also verifies real-time progress reaches 1000 = 100%)
    let out_gif = work.join("out.gif");
    let gif_prog = Arc::new(AtomicU32::new(0));
    let gif_res = run_export_with_progress(
        &ExportRequest {
            input: src_mp4.clone(),
            output: out_gif.clone(),
            format: ExportFormat::Gif,
            fps: 15,
            start_sec: Some(0.1),
            end_sec: Some(1.0),
            max_width: Some(320),
        },
        Some(&gif_prog),
    )
    .expect("export gif");
    assert_eq!(gif_prog.load(Ordering::Relaxed), 1000);
    assert!(gif_res.output_bytes > 500);
    assert!(fs::read(&out_gif).expect("read gif").starts_with(b"GIF89a"));

    // 5. Native APNG export
    let out_apng = work.join("out.apng");
    let apng_res = run_export(&ExportRequest {
        input: src_mp4.clone(),
        output: out_apng.clone(),
        format: ExportFormat::Apng,
        fps: 12,
        start_sec: Some(0.1),
        end_sec: Some(0.9),
        max_width: Some(320),
    })
    .expect("export apng");
    assert!(apng_res.output_bytes > 500);
    assert!(
        fs::read(&out_apng)
            .expect("read apng")
            .starts_with(b"\x89PNG\r\n\x1a\n")
    );

    let mut expected_events = 5_usize;

    // 6. Optional FFmpeg metadata stripping, rotated+audio composition, & WebM verification when installed
    if Path::new("/opt/homebrew/bin/ffmpeg").exists() {
        assert!(
            detect_ffmpeg_binary().is_some(),
            "Homebrew ffmpeg at /opt/homebrew/bin/ffmpeg must be accepted by trusted binary verifier"
        );
    }
    if let Some(ffmpeg) = detect_ffmpeg_binary() {
        // Create an MP4 injected with author/title metadata and verify native passthrough strips it!
        let tagged_mp4 = work.join("tagged.mp4");
        let st = Command::new(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&src_mp4)
            .args([
                "-metadata",
                "title=SecretWindowTitle",
                "-metadata",
                "artist=SecretAuthor",
                "-c",
                "copy",
            ])
            .arg(&tagged_mp4)
            .status()
            .expect("tag mp4");
        assert!(st.success());
        let tagged_meta = probe_video_metadata(&tagged_mp4).expect("probe tagged");
        assert!(tagged_meta.common_metadata_count >= 1);

        let stripped_mp4 = work.join("stripped.mp4");
        run_export(&ExportRequest {
            input: tagged_mp4,
            output: stripped_mp4.clone(),
            format: ExportFormat::Mp4,
            fps: 30,
            start_sec: None,
            end_sec: None,
            max_width: None,
        })
        .expect("strip metadata on full-range mp4");
        let stripped_meta = probe_video_metadata(&stripped_mp4).expect("probe stripped");
        assert_eq!(
            stripped_meta.common_metadata_count, 0,
            "full-range MP4 export must strip source metadata"
        );
        expected_events += 1;

        // Verify audio-bearing + 90-degree rotated video preserves orientation and audio on resize
        let rotated_audio_mp4 = work.join("rotated_audio.mp4");
        let rot_st = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-display_rotation:v:0",
                "90",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=640x360:rate=30:duration=1.2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1.2",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
            ])
            .arg(&rotated_audio_mp4)
            .status()
            .expect("generate rotated audio mp4");
        assert!(rot_st.success());

        let rot_out = work.join("rotated_resized.mp4");
        run_export(&ExportRequest {
            input: rotated_audio_mp4,
            output: rot_out.clone(),
            format: ExportFormat::Mp4,
            fps: 24,
            start_sec: Some(0.1),
            end_sec: Some(1.0),
            max_width: Some(180),
        })
        .expect("export rotated audio mp4");
        let rot_meta = probe_video_metadata(&rot_out).expect("probe rotated out");
        assert!(rot_meta.has_audio, "audio track must be preserved");
        assert!(
            rot_meta.height > rot_meta.width,
            "portrait aspect after 90deg rotation must be preserved (got {}x{})",
            rot_meta.width,
            rot_meta.height
        );
        expected_events += 1;

        let out_webm = work.join("out.webm");
        let webm_res = run_export(&ExportRequest {
            input: src_mp4.clone(),
            output: out_webm,
            format: ExportFormat::Webm,
            fps: 24,
            start_sec: Some(0.0),
            end_sec: Some(0.8),
            max_width: Some(320),
        })
        .expect("export webm");
        assert!(webm_res.output_bytes > 500);
        expected_events += 1;
    }

    let out_webm_or_same_in_src = src_mp4.clone();
    let same_path_res = quick_screen_recorder::export::run_export_with_options(
        &ExportRequest {
            input: src_mp4.clone(),
            output: out_webm_or_same_in_src,
            format: ExportFormat::Mp4,
            fps: 24,
            start_sec: Some(0.0),
            end_sec: Some(0.6),
            max_width: None,
        },
        None,
        true,
    )
    .expect("prevent_overwrite export colliding with input path should write -2");
    assert_eq!(
        same_path_res.output_path.file_name().unwrap(),
        "source-2.mp4"
    );
    assert!(src_mp4.exists());
    expected_events += 1;

    let raw_log = fs::read_to_string(&perf_log).expect("read perf.jsonl");
    assert!(!raw_log.contains("source.mp4"));
    assert!(!raw_log.contains("/Users/"));
    let summary = summarize_perf_log(&perf_log).expect("summarize perf log");
    assert_eq!(summary.total_events, expected_events);
    assert_eq!(summary.success_events, expected_events);

    let _ = fs::remove_dir_all(&work);
}
