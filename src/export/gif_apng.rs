use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use color_quant::NeuQuant;
use gif::{DisposalMethod, Encoder as GifEncoder, Frame as GifFrame, Repeat};
use rayon::prelude::*;

use crate::export::avfoundation::{
    MAX_ANIMATED_FRAMES, RgbaFrame, VideoMetadata, extract_rgba_frame, stream_rgba_sequence,
    transcode_frame_count,
};
use crate::export::{ExportRequest, SharedExportProgress, set_progress_permille};
use crate::geometry::scaled_even_dimensions;

const TRANSPARENT_INDEX: u8 = 255;

/// Streams GIF export with O(1 frame) memory: samples up to 12 keyframes for a global NeuQuant palette,
/// then quantizes and delta-encodes one frame at a time while explicitly checking trailer flush + fsync.
pub fn export_gif(
    req: &ExportRequest,
    meta: &VideoMetadata,
    progress: Option<&SharedExportProgress>,
) -> Result<(), String> {
    let fps = req.fps.clamp(1, 50);
    let start = req.start_sec.unwrap_or(0.0);
    let end = req.end_sec.unwrap_or(meta.duration_sec);
    let capped_max = Some(req.max_width.unwrap_or(1920).min(1920));
    let (target_w, target_h) = scaled_even_dimensions(meta.width, meta.height, capped_max);

    let duration = (end - start).max(0.02);
    let total_est = transcode_frame_count(duration, fps);
    if total_est > MAX_ANIMATED_FRAMES {
        return Err(format!(
            "Clip has {total_est} frames at {fps} fps (maximum is {MAX_ANIMATED_FRAMES} frames). Trim a shorter clip, lower FPS, or export as MP4/HEVC."
        ));
    }

    // Build global 255-color palette from at most 12 evenly spaced sample frames (keeps memory under ~20 MB)
    let sample_count = total_est.clamp(1, 12);
    let mut sample_rgba = Vec::with_capacity(65_536 * 4);
    for s in 0..sample_count {
        let ratio = if sample_count <= 1 {
            0.0
        } else {
            (s as f64) / ((sample_count - 1) as f64)
        };
        let t = start + (end - start) * ratio;
        if let Ok(frame) = extract_rgba_frame(&req.input, t, capped_max) {
            append_subsampled_pixels(&frame, &mut sample_rgba, 5_500);
        }
        let sp = 50 + (((s + 1) * 110) / sample_count.max(1)) as u32;
        set_progress_permille(progress, sp);
    }
    if sample_rgba.is_empty() {
        sample_rgba.extend_from_slice(&[0, 0, 0, 255]);
    }

    let nq = NeuQuant::new(10, 255, &sample_rgba);
    drop(sample_rgba);
    set_progress_permille(progress, 180);

    let mut palette_rgb = vec![0_u8; 256 * 3];
    let cmap = nq.color_map_rgb();
    let copy_len = cmap.len().min(255 * 3);
    palette_rgb[..copy_len].copy_from_slice(&cmap[..copy_len]);

    let width = u16::try_from(target_w).map_err(|_| "GIF width exceeds 65535")?;
    let height = u16::try_from(target_h).map_err(|_| "GIF height exceeds 65535")?;

    let file = File::create(&req.output).map_err(|e| format!("Failed to create GIF file: {e}"))?;
    let writer = BufWriter::new(file);
    let mut encoder = GifEncoder::new(writer, width, height, &palette_rgb)
        .map_err(|e| format!("GIF header error: {e}"))?;
    encoder
        .set_repeat(Repeat::Infinite)
        .map_err(|e| format!("GIF repeat setting error: {e}"))?;

    let mut prev_indexed: Option<Vec<u8>> = None;
    stream_rgba_sequence(
        &req.input,
        start,
        end,
        fps,
        capped_max,
        |idx, total, frame| {
            let curr_indexed: Vec<u8> = frame
                .pixels
                .par_chunks_exact(4)
                .map(|px| (nq.index_of(px) as u8).min(254))
                .collect();
            let delay_cs = frame_delay_centiseconds_for_duration(idx, total, duration, fps);
            let gif_frame = match &prev_indexed {
                None => {
                    let mut f =
                        GifFrame::from_indexed_pixels(width, height, curr_indexed.clone(), None);
                    f.dispose = DisposalMethod::Keep;
                    f.delay = delay_cs;
                    f
                }
                Some(prev) => build_delta_gif_frame(
                    prev,
                    &curr_indexed,
                    target_w as usize,
                    target_h as usize,
                    delay_cs,
                ),
            };
            encoder
                .write_frame(&gif_frame)
                .map_err(|e| format!("Failed writing GIF frame {idx}: {e}"))?;
            prev_indexed = Some(curr_indexed);
            let fp = 180 + (((idx + 1) * 780) / total.max(1)) as u32;
            set_progress_permille(progress, fp);
            Ok(())
        },
    )?;

    set_progress_permille(progress, 975);
    finalize_gif_encoder(encoder)
}

/// Streams APNG export one frame at a time with O(1 frame) memory and explicit flush + fsync verification.
pub fn export_apng(
    req: &ExportRequest,
    meta: &VideoMetadata,
    progress: Option<&SharedExportProgress>,
) -> Result<(), String> {
    let fps = req.fps.clamp(1, 60);
    let start = req.start_sec.unwrap_or(0.0);
    let end = req.end_sec.unwrap_or(meta.duration_sec);
    let capped_max = Some(req.max_width.unwrap_or(1920).min(1920));
    let (target_w, target_h) = scaled_even_dimensions(meta.width, meta.height, capped_max);

    let duration = (end - start).max(0.02);
    let total_frames = transcode_frame_count(duration, fps);
    if total_frames > MAX_ANIMATED_FRAMES {
        return Err(format!(
            "Clip has {total_frames} frames at {fps} fps (maximum is {MAX_ANIMATED_FRAMES} frames). Trim a shorter clip, lower FPS, or export as MP4/HEVC."
        ));
    }

    let file = File::create(&req.output).map_err(|e| format!("Failed to create APNG file: {e}"))?;
    let writer = BufWriter::new(file);

    let mut encoder = png::Encoder::new(writer, target_w, target_h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    encoder
        .set_animated(total_frames as u32, 0)
        .map_err(|e| format!("APNG animation setup error: {e}"))?;
    let (init_num, init_den) = apng_frame_delay_for_duration(0, total_frames, duration, fps);
    encoder
        .set_frame_delay(init_num, init_den)
        .map_err(|e| format!("APNG delay error: {e}"))?;

    let mut png_writer = encoder
        .write_header()
        .map_err(|e| format!("APNG header error: {e}"))?;

    stream_rgba_sequence(
        &req.input,
        start,
        end,
        fps,
        capped_max,
        |idx, total, frame| {
            let (delay_num, delay_den) = apng_frame_delay_for_duration(idx, total, duration, fps);
            png_writer
                .set_frame_delay(delay_num, delay_den)
                .map_err(|e| format!("APNG delay error on frame {idx}: {e}"))?;
            png_writer
                .write_image_data(&frame.pixels)
                .map_err(|e| format!("Failed writing APNG frame {idx}: {e}"))?;
            let fp = 50 + (((idx + 1) * 910) / total.max(1)) as u32;
            set_progress_permille(progress, fp);
            Ok(())
        },
    )?;

    set_progress_permille(progress, 975);
    png_writer
        .finish()
        .map_err(|e| format!("Failed finalizing APNG stream: {e}"))?;
    Ok(())
}

/// Distributes centisecond delays across frames so cumulative playback time stays exact.
pub fn frame_delay_centiseconds(frame_index: usize, fps: u32) -> u16 {
    let clamped = f64::from(fps.clamp(1, 50));
    let t0 = ((frame_index as f64) * 100.0 / clamped).round() as u64;
    let t1 = (((frame_index + 1) as f64) * 100.0 / clamped).round() as u64;
    t1.saturating_sub(t0).max(2) as u16
}

/// Computes GIF frame delay in centiseconds while capping the final frame to `duration_sec`,
/// preserving exact trimmed playback duration even at low FPS (e.g. 1.5s or 0.7s at 1 fps).
pub fn frame_delay_centiseconds_for_duration(
    frame_index: usize,
    total_frames: usize,
    duration_sec: f64,
    fps: u32,
) -> u16 {
    if frame_index + 1 < total_frames {
        return frame_delay_centiseconds(frame_index, fps);
    }
    let clamped = f64::from(fps.clamp(1, 50));
    let t0 = ((frame_index as f64) * 100.0 / clamped).round() as u64;
    let t_end = (duration_sec.max(0.02) * 100.0).round() as u64;
    t_end.saturating_sub(t0).clamp(2, u64::from(u16::MAX)) as u16
}

/// Computes APNG `(delay_num, delay_den)` rational delay, fitting the final frame to the
/// remaining trimmed duration so non-integer-second low-FPS exports never stretch or shrink.
pub fn apng_frame_delay_for_duration(
    frame_index: usize,
    total_frames: usize,
    duration_sec: f64,
    fps: u32,
) -> (u16, u16) {
    let clamped_fps = fps.clamp(1, 60);
    if frame_index + 1 < total_frames {
        return (1, clamped_fps as u16);
    }
    let frame_step = 1.0 / f64::from(clamped_fps);
    let remaining = (duration_sec.max(0.02) - (frame_index as f64) * frame_step).max(0.001);
    if (remaining - frame_step).abs() <= 0.0005 {
        return (1, clamped_fps as u16);
    }
    let ms = (remaining * 1000.0).round().clamp(1.0, f64::from(u16::MAX)) as u16;
    (ms, 1000)
}

pub fn encode_rgba_frames_to_gif(
    frames: &[RgbaFrame],
    fps: u32,
    output_path: &Path,
) -> Result<(), String> {
    let first = frames.first().ok_or("No frames to encode into GIF")?;
    let width = u16::try_from(first.width).map_err(|_| "GIF width exceeds 65535")?;
    let height = u16::try_from(first.height).map_err(|_| "GIF height exceeds 65535")?;

    let mut sample_rgba = Vec::with_capacity(32_768 * 4);
    let per_frame = (32_768 / frames.len().max(1)).max(256);
    for f in frames {
        append_subsampled_pixels(f, &mut sample_rgba, per_frame);
    }
    let nq = NeuQuant::new(10, 255, &sample_rgba);
    let mut palette_rgb = vec![0_u8; 256 * 3];
    let cmap = nq.color_map_rgb();
    let copy_len = cmap.len().min(255 * 3);
    palette_rgb[..copy_len].copy_from_slice(&cmap[..copy_len]);

    let file = File::create(output_path).map_err(|e| format!("Failed to create GIF file: {e}"))?;
    let writer = BufWriter::new(file);
    let mut encoder = GifEncoder::new(writer, width, height, &palette_rgb)
        .map_err(|e| format!("GIF header error: {e}"))?;
    encoder
        .set_repeat(Repeat::Infinite)
        .map_err(|e| format!("GIF repeat setting error: {e}"))?;

    let mut prev_indexed: Option<Vec<u8>> = None;
    for (i, frame) in frames.iter().enumerate() {
        let current: Vec<u8> = frame
            .pixels
            .par_chunks_exact(4)
            .map(|px| (nq.index_of(px) as u8).min(254))
            .collect();
        let delay_cs = frame_delay_centiseconds(i, fps);
        let gif_frame = match &prev_indexed {
            None => {
                let mut f = GifFrame::from_indexed_pixels(width, height, current.clone(), None);
                f.dispose = DisposalMethod::Keep;
                f.delay = delay_cs;
                f
            }
            Some(prev) => build_delta_gif_frame(
                prev,
                &current,
                first.width as usize,
                first.height as usize,
                delay_cs,
            ),
        };
        encoder
            .write_frame(&gif_frame)
            .map_err(|e| format!("Failed writing GIF frame {i}: {e}"))?;
        prev_indexed = Some(current);
    }

    finalize_gif_encoder(encoder)
}

pub fn encode_rgba_frames_to_apng(
    frames: &[RgbaFrame],
    fps: u32,
    output_path: &Path,
) -> Result<(), String> {
    let first = frames.first().ok_or("No frames to encode into APNG")?;
    let file = File::create(output_path).map_err(|e| format!("Failed to create APNG file: {e}"))?;
    let writer = BufWriter::new(file);

    let mut encoder = png::Encoder::new(writer, first.width, first.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    encoder
        .set_animated(frames.len() as u32, 0)
        .map_err(|e| format!("APNG animation setup error: {e}"))?;
    encoder
        .set_frame_delay(1, fps.clamp(1, 120) as u16)
        .map_err(|e| format!("APNG delay error: {e}"))?;

    let mut png_writer = encoder
        .write_header()
        .map_err(|e| format!("APNG header error: {e}"))?;

    for (idx, frame) in frames.iter().enumerate() {
        png_writer
            .write_image_data(&frame.pixels)
            .map_err(|e| format!("Failed writing APNG frame {idx}: {e}"))?;
    }
    png_writer
        .finish()
        .map_err(|e| format!("Failed finalizing APNG stream: {e}"))
}

/// Writes the GIF `0x3B` trailer via `into_inner()`, flushes `BufWriter`, and `sync_all()`s the underlying file.
fn finalize_gif_encoder(encoder: GifEncoder<BufWriter<File>>) -> Result<(), String> {
    let mut buf_writer = encoder
        .into_inner()
        .map_err(|e| format!("Failed writing GIF trailer: {e}"))?;
    buf_writer
        .flush()
        .map_err(|e| format!("Failed flushing GIF output buffer: {e}"))?;
    let file = buf_writer
        .into_inner()
        .map_err(|e| format!("Failed unwrapping GIF output file: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("Failed syncing GIF file to disk: {e}"))
}

fn append_subsampled_pixels(frame: &RgbaFrame, out: &mut Vec<u8>, target_samples: usize) {
    let px_count = frame.pixels.len() / 4;
    let stride = (px_count / target_samples.max(1)).max(1);
    for (idx, px) in frame.pixels.chunks_exact(4).enumerate() {
        if idx % stride == 0 {
            out.extend_from_slice(px);
        }
    }
}

fn build_delta_gif_frame<'a>(
    prev: &[u8],
    curr: &[u8],
    w: usize,
    h: usize,
    delay_cs: u16,
) -> GifFrame<'a> {
    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = 0_usize;
    let mut max_y = 0_usize;

    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            if prev[row + x] != curr[row + x] {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }

    if min_x > max_x || min_y > max_y {
        let mut f =
            GifFrame::from_indexed_pixels(1, 1, vec![TRANSPARENT_INDEX], Some(TRANSPARENT_INDEX));
        f.dispose = DisposalMethod::Keep;
        f.delay = delay_cs;
        return f;
    }

    let sub_w = max_x - min_x + 1;
    let sub_h = max_y - min_y + 1;
    let mut sub_pixels = Vec::with_capacity(sub_w * sub_h);

    for y in min_y..=max_y {
        let row = y * w;
        for x in min_x..=max_x {
            let p = prev[row + x];
            let c = curr[row + x];
            sub_pixels.push(if p == c { TRANSPARENT_INDEX } else { c });
        }
    }

    let mut f = GifFrame::from_indexed_pixels(
        sub_w as u16,
        sub_h as u16,
        sub_pixels,
        Some(TRANSPARENT_INDEX),
    );
    f.left = min_x as u16;
    f.top = min_y as u16;
    f.dispose = DisposalMethod::Keep;
    f.delay = delay_cs;
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_frames() -> Vec<RgbaFrame> {
        let w = 64_u32;
        let h = 48_u32;
        (0..6_u8)
            .map(|step| {
                let mut pixels = vec![0_u8; (w * h * 4) as usize];
                for y in 0..h {
                    for x in 0..w {
                        let i = ((y * w + x) * 4) as usize;
                        pixels[i] = (x * 4) as u8;
                        pixels[i + 1] = (y * 5) as u8;
                        pixels[i + 2] = step * 35;
                        pixels[i + 3] = 255;
                    }
                }
                RgbaFrame {
                    width: w,
                    height: h,
                    pixels,
                }
            })
            .collect()
    }

    #[test]
    fn encodes_valid_gif_and_apng_with_explicit_trailer() {
        let frames = synthetic_frames();
        let gif_out = std::env::temp_dir().join(format!("qsr-unit-{}.gif", std::process::id()));
        let apng_out = std::env::temp_dir().join(format!("qsr-unit-{}.apng", std::process::id()));

        encode_rgba_frames_to_gif(&frames, 30, &gif_out).expect("encode gif");
        encode_rgba_frames_to_apng(&frames, 30, &apng_out).expect("encode apng");

        let gif_bytes = std::fs::read(&gif_out).expect("read gif");
        assert!(gif_bytes.starts_with(b"GIF89a"));
        assert_eq!(*gif_bytes.last().expect("non-empty"), 0x3B);

        let apng_bytes = std::fs::read(&apng_out).expect("read apng");
        assert!(apng_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(apng_bytes.windows(4).any(|w| w == b"acTL"));

        let _ = std::fs::remove_file(&gif_out);
        let _ = std::fs::remove_file(&apng_out);
    }

    #[test]
    fn gif_30fps_cumulative_duration_has_zero_drift() {
        let total_cs: u32 = (0..300)
            .map(|i| u32::from(frame_delay_centiseconds(i, 30)))
            .sum();
        assert_eq!(total_cs, 1000);
    }

    #[test]
    fn low_fps_non_integer_durations_match_trimmed_length_in_gif_and_apng() {
        // 1.5s @ 1 fps => 2 frames: 1.0s + 0.5s = 1.5s
        assert_eq!(frame_delay_centiseconds_for_duration(0, 2, 1.5, 1), 100);
        assert_eq!(frame_delay_centiseconds_for_duration(1, 2, 1.5, 1), 50);
        assert_eq!(apng_frame_delay_for_duration(0, 2, 1.5, 1), (1, 1));
        assert_eq!(apng_frame_delay_for_duration(1, 2, 1.5, 1), (500, 1000));

        // 0.7s @ 1 fps => 1 frame: 0.7s
        assert_eq!(frame_delay_centiseconds_for_duration(0, 1, 0.7, 1), 70);
        assert_eq!(apng_frame_delay_for_duration(0, 1, 0.7, 1), (700, 1000));
    }
}
