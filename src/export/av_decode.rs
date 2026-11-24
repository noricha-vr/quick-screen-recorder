use std::path::Path;
use std::ptr;

use objc2_av_foundation::AVAssetImageGenerator;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextGetData, CGColorSpace, CGContext, CGImage,
    CGImageAlphaInfo,
};

use crate::export::avfoundation::{
    MAX_ANIMATED_FRAMES, open_restricted_asset, probe_video_metadata, secs_to_cmtime,
    transcode_frame_count,
};
use crate::geometry::scaled_even_dimensions;

#[derive(Debug, Clone)]
pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

pub fn extract_rgba_frame(
    path: &Path,
    time_sec: f64,
    max_width: Option<u32>,
) -> Result<RgbaFrame, String> {
    let meta = probe_video_metadata(path)?;
    let capped_max = Some(max_width.unwrap_or(3840).min(3840));
    let (target_w, target_h) = scaled_even_dimensions(meta.width, meta.height, capped_max);
    let asset = open_restricted_asset(path)?;
    let generator = unsafe { AVAssetImageGenerator::assetImageGeneratorWithAsset(&asset) };

    let zero_tol = secs_to_cmtime(0.01);
    unsafe {
        generator.setAppliesPreferredTrackTransform(true);
        generator.setMaximumSize(CGSize::new(
            f64::from(target_w),
            f64::from(meta.height.max(target_h)),
        ));
        generator.setRequestedTimeToleranceBefore(zero_tol);
        generator.setRequestedTimeToleranceAfter(zero_tol);
    }

    let req_time = secs_to_cmtime(time_sec.clamp(0.0, meta.duration_sec));
    #[allow(deprecated)]
    let cg_img = unsafe {
        generator
            .copyCGImageAtTime_actualTime_error(req_time, ptr::null_mut())
            .map_err(|e| format!("Frame decode error: {}", e.localizedDescription()))?
    };

    cgimage_to_rgba_sized(&cg_img, target_w as usize, target_h as usize)
}

pub fn stream_rgba_sequence<F>(
    path: &Path,
    start_sec: f64,
    end_sec: f64,
    fps: u32,
    max_width: Option<u32>,
    mut on_frame: F,
) -> Result<(u32, u32, usize), String>
where
    F: FnMut(usize, usize, RgbaFrame) -> Result<(), String>,
{
    let meta = probe_video_metadata(path)?;
    let capped_max = Some(max_width.unwrap_or(1920).min(1920));
    let (target_w, target_h) = scaled_even_dimensions(meta.width, meta.height, capped_max);
    let clamped_fps = fps.clamp(1, 60);
    let frame_step = 1.0 / f64::from(clamped_fps);
    let duration = (end_sec - start_sec).max(0.02);
    let requested_count = transcode_frame_count(duration, clamped_fps);

    if requested_count > MAX_ANIMATED_FRAMES {
        return Err(format!(
            "Clip has {requested_count} frames at {clamped_fps} fps (maximum is {MAX_ANIMATED_FRAMES} frames). Trim a shorter clip, lower FPS, or export as MP4/HEVC."
        ));
    }

    let asset = open_restricted_asset(path)?;
    let generator = unsafe { AVAssetImageGenerator::assetImageGeneratorWithAsset(&asset) };

    let tol = secs_to_cmtime((frame_step * 0.45).max(0.005));
    unsafe {
        generator.setAppliesPreferredTrackTransform(true);
        generator.setMaximumSize(CGSize::new(
            f64::from(target_w),
            f64::from(meta.height.max(target_h)),
        ));
        generator.setRequestedTimeToleranceBefore(tol);
        generator.setRequestedTimeToleranceAfter(tol);
    }

    let mut last_good: Option<RgbaFrame> = None;
    for idx in 0..requested_count {
        let t = (start_sec + (idx as f64) * frame_step).min(meta.duration_sec);
        let req_time = secs_to_cmtime(t);
        #[allow(deprecated)]
        let cg_res =
            unsafe { generator.copyCGImageAtTime_actualTime_error(req_time, ptr::null_mut()) };
        let frame = match cg_res {
            Ok(cg_img) => {
                let decoded = cgimage_to_rgba_sized(&cg_img, target_w as usize, target_h as usize)?;
                last_good = Some(decoded.clone());
                decoded
            }
            Err(_) if last_good.is_some() => last_good.clone().unwrap(),
            Err(e) => {
                return Err(format!(
                    "Failed to decode initial video frame: {}",
                    e.localizedDescription()
                ));
            }
        };
        on_frame(idx, requested_count, frame)?;
    }
    Ok((target_w, target_h, requested_count))
}

pub(crate) fn cgimage_to_rgba_sized(
    image: &CGImage,
    w: usize,
    h: usize,
) -> Result<RgbaFrame, String> {
    if w == 0 || h == 0 || CGImage::width(Some(image)) == 0 || CGImage::height(Some(image)) == 0 {
        return Err("Decoded CGImage has zero dimensions".to_string());
    }

    let color_space = CGColorSpace::new_device_rgb().ok_or("Failed to allocate RGB color space")?;
    let bytes_per_row = w * 4;
    let bitmap_info = CGImageAlphaInfo::PremultipliedLast.0;

    let ctx = unsafe {
        CGBitmapContextCreate(
            ptr::null_mut(),
            w,
            h,
            8,
            bytes_per_row,
            Some(&color_space),
            bitmap_info,
        )
    }
    .ok_or("Failed to create CGBitmapContext")?;

    let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64));
    CGContext::draw_image(Some(&ctx), rect, Some(image));

    let data_ptr = CGBitmapContextGetData(Some(&ctx)) as *const u8;
    if data_ptr.is_null() {
        return Err("CGBitmapContext returned null pixel buffer".to_string());
    }
    let len = bytes_per_row * h;
    let pixels = unsafe { std::slice::from_raw_parts(data_ptr, len).to_vec() };

    Ok(RgbaFrame {
        width: w as u32,
        height: h as u32,
        pixels,
    })
}
