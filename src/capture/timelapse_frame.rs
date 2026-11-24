use std::ptr;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextGetData, CGColorSpace, CGContext, CGImage,
    CGImageAlphaInfo,
};
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{SCContentFilter, SCScreenshotManager, SCStreamConfiguration};

use crate::capture::cadence::aspect_fit_rect;
use crate::export::avfoundation::RgbaFrame;

pub(crate) const FRAME_CAPTURE_TIMEOUT: Duration = Duration::from_secs(6);

pub(crate) fn capture_screenshot_frame(
    filter: &SCContentFilter,
    stream_cfg: &SCStreamConfiguration,
    target_w: usize,
    target_h: usize,
) -> Result<RgbaFrame, String> {
    let (tx, rx) = mpsc::sync_channel::<Result<RgbaFrame, String>>(1);
    let block = RcBlock::new(move |img_ptr: *mut CGImage, err_ptr: *mut NSError| {
        if !err_ptr.is_null() {
            let msg = unsafe { (&*err_ptr).localizedDescription().to_string() };
            let _ = tx.send(Err(format!("Timelapse frame capture failed: {msg}")));
            return;
        }
        if img_ptr.is_null() {
            let _ = tx.send(Err("Timelapse frame capture returned no image".to_string()));
            return;
        }
        // Render immediately while the borrowed CGImageRef is guaranteed alive inside this callback.
        let image = unsafe { &*img_ptr };
        let res = render_cgimage_aspect_fit_rgba(image, target_w, target_h);
        let _ = tx.send(res);
    });

    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            filter,
            stream_cfg,
            Some(&block),
        );
    }

    rx.recv_timeout(FRAME_CAPTURE_TIMEOUT)
        .map_err(|_| "Timed out capturing timelapse frame".to_string())?
}

fn render_cgimage_aspect_fit_rgba(
    image: &CGImage,
    dst_w: usize,
    dst_h: usize,
) -> Result<RgbaFrame, String> {
    let src_w = CGImage::width(Some(image));
    let src_h = CGImage::height(Some(image));
    if dst_w == 0 || dst_h == 0 || src_w == 0 || src_h == 0 {
        return Err("Captured timelapse frame has zero dimensions".to_string());
    }

    let color_space = CGColorSpace::new_device_rgb().ok_or("Failed to allocate RGB color space")?;
    let bytes_per_row = dst_w * 4;
    let bitmap_info = CGImageAlphaInfo::PremultipliedLast.0;

    let ctx = unsafe {
        CGBitmapContextCreate(
            ptr::null_mut(),
            dst_w,
            dst_h,
            8,
            bytes_per_row,
            Some(&color_space),
            bitmap_info,
        )
    }
    .ok_or("Failed to create CGBitmapContext for timelapse frame")?;

    let full = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(dst_w as f64, dst_h as f64),
    );
    CGContext::set_rgb_fill_color(Some(&ctx), 0.0, 0.0, 0.0, 1.0);
    CGContext::fill_rect(Some(&ctx), full);

    let (dx, dy, dw, dh) = aspect_fit_rect(src_w as f64, src_h as f64, dst_w as f64, dst_h as f64);
    let draw_rect = CGRect::new(CGPoint::new(dx, dy), CGSize::new(dw, dh));
    CGContext::draw_image(Some(&ctx), draw_rect, Some(image));

    let data_ptr = CGBitmapContextGetData(Some(&ctx)) as *const u8;
    if data_ptr.is_null() {
        return Err("CGBitmapContext returned null pixel buffer".to_string());
    }
    let pixels = unsafe { std::slice::from_raw_parts(data_ptr, bytes_per_row * dst_h).to_vec() };
    Ok(RgbaFrame {
        width: dst_w as u32,
        height: dst_h as u32,
        pixels,
    })
}
