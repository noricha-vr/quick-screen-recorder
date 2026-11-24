use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use quick_screen_recorder::capture::recorder::is_valid_salvageable_mp4;
use quick_screen_recorder::capture::{RecordingCodec, TIMELAPSE_PLAYBACK_FPS};
use quick_screen_recorder::export::av_writer::{StreamingMp4Writer, write_rgba_frames_to_mp4};
use quick_screen_recorder::export::avfoundation::{RgbaFrame, probe_video_metadata};
use quick_screen_recorder::export::{ExportFormat, ExportRequest, run_export};
use quick_screen_recorder::fs_safety::{ExclusiveStagingDir, next_unique_token};
use quick_screen_recorder::staging::commit_or_recover_staging_with_policy;

fn make_temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qsr-it-{name}-{}", next_unique_token()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp test dir");
    dir
}

fn generate_30fps_fixture(out_path: &Path, frame_count: u32) {
    let w = 320_u32;
    let h = 180_u32;
    let frames: Vec<RgbaFrame> = (0..frame_count)
        .map(|f| {
            let mut pixels = vec![0_u8; (w * h * 4) as usize];
            for px in pixels.chunks_exact_mut(4) {
                px[0] = ((f * 5) % 255) as u8;
                px[1] = 110;
                px[2] = 200;
                px[3] = 255;
            }
            RgbaFrame {
                width: w,
                height: h,
                pixels,
            }
        })
        .collect();
    write_rgba_frames_to_mp4(&frames, 30, RecordingCodec::H264, out_path)
        .expect("write 30fps fixture");
}

#[test]
fn streaming_timelapse_and_low_fps_exports_preserve_duration_and_cadence() {
    let work = make_temp_dir("timelapse-low-fps");

    // 1. StreamingMp4Writer timelapse: 30 frames @ 30 fps = 1.0s, silent, zero metadata
    let tl_mp4 = work.join("timelapse_30fps.mp4");
    let mut tl_writer = StreamingMp4Writer::new(
        320,
        180,
        TIMELAPSE_PLAYBACK_FPS,
        RecordingCodec::H264,
        &tl_mp4,
    )
    .expect("create streaming timelapse writer");
    for f in 0..30_u32 {
        let mut pixels = vec![0_u8; 320 * 180 * 4];
        for px in pixels.chunks_exact_mut(4) {
            px[0] = (f * 8) as u8;
            px[1] = 110;
            px[2] = 200;
            px[3] = 255;
        }
        tl_writer
            .append_rgba_frame(&RgbaFrame {
                width: 320,
                height: 180,
                pixels,
            })
            .expect("append timelapse frame");
    }
    assert_eq!(tl_writer.frames_written(), 30);
    tl_writer.finish().expect("finish timelapse mp4");
    let tl_meta = probe_video_metadata(&tl_mp4).expect("probe timelapse mp4");
    assert!(!tl_meta.has_audio, "timelapse MP4 must be audio-free");
    assert_eq!(tl_meta.common_metadata_count, 0);
    assert!(
        (tl_meta.nominal_fps - 30.0).abs() <= 1.5,
        "expected 30 fps timelapse playback, got {}",
        tl_meta.nominal_fps
    );
    assert!(
        (tl_meta.duration_sec - 1.0).abs() <= 0.15,
        "30 frames @ 30fps should be ~1.0s, got {}",
        tl_meta.duration_sec
    );

    // 2. Low-FPS real-time transcodes preserve trimmed duration accurately
    let src_mp4 = work.join("source_2s.mp4");
    generate_30fps_fixture(&src_mp4, 60); // 2.0s @ 30 fps

    let one_fps_trim = work.join("realtime_1fps_1_5s.mp4");
    run_export(&ExportRequest {
        input: src_mp4.clone(),
        output: one_fps_trim.clone(),
        format: ExportFormat::Mp4,
        fps: 1,
        start_sec: Some(0.0),
        end_sec: Some(1.5),
        max_width: Some(320),
        mute_audio: false,
    })
    .expect("export 1.5s 1fps mp4");
    let trim_meta = probe_video_metadata(&one_fps_trim).expect("probe 1.5s 1fps mp4");
    assert!(
        (trim_meta.duration_sec - 1.5).abs() <= 0.12,
        "1.5s trimmed export at 1 fps must preserve 1.5s duration (not round to 2.0s), got {}",
        trim_meta.duration_sec
    );
    assert!(
        trim_meta.nominal_fps >= 0.8 && trim_meta.nominal_fps <= 1.5,
        "expected low ~1 fps nominal rate for 1.5s 1fps export, got {}",
        trim_meta.nominal_fps
    );

    let one_fps_exact = work.join("realtime_1fps_2_0s.mp4");
    run_export(&ExportRequest {
        input: src_mp4.clone(),
        output: one_fps_exact.clone(),
        format: ExportFormat::Mp4,
        fps: 1,
        start_sec: Some(0.0),
        end_sec: Some(2.0),
        max_width: Some(320),
        mute_audio: false,
    })
    .expect("export 2.0s 1fps mp4");
    let exact_meta = probe_video_metadata(&one_fps_exact).expect("probe 2.0s 1fps mp4");
    assert!(
        (exact_meta.duration_sec - 2.0).abs() <= 0.12,
        "2.0s export at 1 fps must preserve 2.0s duration, got {}",
        exact_meta.duration_sec
    );
    assert!(
        (exact_meta.nominal_fps - 1.0).abs() <= 0.25,
        "expected ~1.0 fps output on 2.0s export, got {}",
        exact_meta.nominal_fps
    );

    let short_one_fps = work.join("realtime_1fps_0_7s.mp4");
    run_export(&ExportRequest {
        input: src_mp4.clone(),
        output: short_one_fps.clone(),
        format: ExportFormat::Mp4,
        fps: 1,
        start_sec: Some(0.2),
        end_sec: Some(0.9),
        max_width: Some(320),
        mute_audio: false,
    })
    .expect("export 0.7s 1fps mp4");
    let short_meta = probe_video_metadata(&short_one_fps).expect("probe 0.7s 1fps mp4");
    assert!(
        (short_meta.duration_sec - 0.7).abs() <= 0.12,
        "0.7s trimmed export at 1 fps must preserve 0.7s duration, got {}",
        short_meta.duration_sec
    );

    let five_fps = work.join("realtime_5fps_1_0s.mp4");
    run_export(&ExportRequest {
        input: src_mp4.clone(),
        output: five_fps.clone(),
        format: ExportFormat::Mp4,
        fps: 5,
        start_sec: Some(0.0),
        end_sec: Some(1.0),
        max_width: Some(320),
        mute_audio: false,
    })
    .expect("export 1.0s 5fps mp4");
    let five_meta = probe_video_metadata(&five_fps).expect("probe 5fps mp4");
    assert!(
        (five_meta.duration_sec - 1.0).abs() <= 0.12,
        "1.0s export at 5 fps must preserve 1.0s duration, got {}",
        five_meta.duration_sec
    );
    assert!(
        (five_meta.nominal_fps - 5.0).abs() <= 0.5,
        "expected ~5 fps output, got {}",
        five_meta.nominal_fps
    );

    // 3. Low-FPS GIF & APNG exports preserve non-integer trimmed durations (1.5s & 0.7s @ 1 fps)
    for (label, start, end, expected_frames, expected_secs) in [
        ("1_5s", 0.0_f64, 1.5_f64, 2_usize, 1.5_f64),
        ("0_7s", 0.2_f64, 0.9_f64, 1_usize, 0.7_f64),
    ] {
        let gif_out = work.join(format!("low_fps_{label}.gif"));
        run_export(&ExportRequest {
            input: src_mp4.clone(),
            output: gif_out.clone(),
            format: ExportFormat::Gif,
            fps: 1,
            start_sec: Some(start),
            end_sec: Some(end),
            max_width: Some(160),
            mute_audio: false,
        })
        .expect("export low-fps gif");
        let (gif_frames, gif_secs) = read_gif_frame_count_and_duration(&gif_out);
        assert_eq!(gif_frames, expected_frames, "GIF {label} frame count");
        assert!(
            (gif_secs - expected_secs).abs() <= 0.02,
            "GIF {label} @ 1 fps should play for {expected_secs}s, got {gif_secs}s"
        );

        let apng_out = work.join(format!("low_fps_{label}.apng"));
        run_export(&ExportRequest {
            input: src_mp4.clone(),
            output: apng_out.clone(),
            format: ExportFormat::Apng,
            fps: 1,
            start_sec: Some(start),
            end_sec: Some(end),
            max_width: Some(160),
            mute_audio: false,
        })
        .expect("export low-fps apng");
        let (apng_frames, apng_secs) = read_apng_frame_count_and_duration(&apng_out);
        assert_eq!(apng_frames, expected_frames, "APNG {label} frame count");
        assert!(
            (apng_secs - expected_secs).abs() <= 0.01,
            "APNG {label} @ 1 fps should play for {expected_secs}s, got {apng_secs}s"
        );
    }

    // 4. Salvage validator rejects non-video files and truncated/corrupt MP4s, accepts valid MP4,
    //    and non-overwriting salvage commit never replaces an existing healthy video at output_path.
    assert!(
        !is_valid_salvageable_mp4(Path::new("Cargo.toml")),
        "plain text non-video file must never pass salvage check"
    );
    let truncated = work.join("truncated_unfinalized.mp4");
    let valid_bytes = fs::read(&src_mp4).expect("read valid mp4");
    fs::write(&truncated, &valid_bytes[..valid_bytes.len().min(512)]).expect("write truncated");
    assert!(
        !is_valid_salvageable_mp4(&truncated),
        "truncated/moov-less MP4 must be rejected by salvage check"
    );
    assert!(
        is_valid_salvageable_mp4(&src_mp4),
        "valid playable MP4 with decodable video track must pass salvage check"
    );

    let existing_good = work.join("existing_good.mp4");
    fs::write(&existing_good, b"pre-existing-healthy-video").expect("write pre-existing");
    let mut salvage_stage =
        ExclusiveStagingDir::create_for_target_with_policy(&existing_good, "mp4", true)
            .expect("stage salvage");
    fs::copy(&src_mp4, salvage_stage.staged_file()).expect("copy valid clip to stage");
    let (salvaged_path, _, _) = commit_or_recover_staging_with_policy(
        &mut salvage_stage,
        existing_good.clone(),
        Some("ended early".to_string()),
        true,
        Ok(work.join("rec")),
    )
    .expect("commit salvaged without overwriting");
    assert_ne!(salvaged_path, existing_good);
    assert_eq!(
        fs::read(&existing_good).unwrap(),
        b"pre-existing-healthy-video",
        "salvaged commit must never overwrite an existing file at target path"
    );

    let _ = fs::remove_dir_all(&work);
}

fn read_gif_frame_count_and_duration(path: &Path) -> (usize, f64) {
    let file = File::open(path).expect("open gif");
    let mut decoder = gif::DecodeOptions::new()
        .read_info(BufReader::new(file))
        .expect("decode gif header");
    let mut count = 0_usize;
    let mut total_cs = 0_u64;
    while let Some(frame) = decoder.read_next_frame().expect("read gif frame") {
        count += 1;
        total_cs += u64::from(frame.delay);
    }
    (count, (total_cs as f64) / 100.0)
}

fn read_apng_frame_count_and_duration(path: &Path) -> (usize, f64) {
    let file = File::open(path).expect("open apng");
    let mut reader = png::Decoder::new(BufReader::new(file))
        .read_info()
        .expect("decode apng header");
    let anim = reader
        .info()
        .animation_control()
        .expect("APNG must contain acTL chunk");
    let num_frames = anim.num_frames as usize;
    let width = reader.info().width;
    let height = reader.info().height;
    let buf_len = reader
        .output_buffer_size()
        .expect("APNG output buffer size");
    let mut buf = vec![0_u8; buf_len];

    let mut total_secs = 0.0_f64;
    for idx in 0..num_frames {
        let fc = if idx == 0 {
            *reader
                .info()
                .frame_control()
                .expect("APNG frame 0 must carry fcTL")
        } else {
            *reader
                .next_frame_info()
                .expect("APNG subsequent frame must carry fcTL")
        };
        let out_info = reader
            .next_frame(&mut buf)
            .expect("decode APNG frame pixels and CRC");
        assert_eq!(out_info.width, width, "APNG frame {idx} width mismatch");
        assert_eq!(out_info.height, height, "APNG frame {idx} height mismatch");
        assert!(
            out_info.buffer_size() > 0 && buf[..out_info.buffer_size()].iter().any(|&b| b != 0),
            "APNG frame {idx} decoded to empty/zero buffer"
        );
        let den = if fc.delay_den == 0 { 100 } else { fc.delay_den };
        total_secs += f64::from(fc.delay_num) / f64::from(den);
    }
    (num_frames, total_secs)
}
