use std::path::Path;

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSCalibratedRGBColorSpace, NSImage, NSPasteboard,
    NSPasteboardWriting, NSWindow,
};
use objc2_foundation::{NSArray, NSDictionary, NSSize, NSString, NSURL};

use crate::export::avfoundation::RgbaFrame;

pub fn copy_file_to_clipboard(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err("Exported file does not exist for clipboard copy".to_string());
    }
    let s = path.to_str().ok_or("Clipboard path is not valid UTF-8")?;
    let ns_str = NSString::from_str(s);
    let url = NSURL::fileURLWithPath(&ns_str);
    let proto: Retained<ProtocolObject<dyn NSPasteboardWriting>> =
        ProtocolObject::from_retained(url);
    let items = NSArray::from_retained_slice(&[proto]);

    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    let ok = pb.writeObjects(&items);
    if ok {
        Ok(())
    } else {
        Err("macOS NSPasteboard refused file URL".to_string())
    }
}

pub fn rgba_to_nsimage(frame: &RgbaFrame) -> Option<Retained<NSImage>> {
    let w = frame.width as isize;
    let h = frame.height as isize;
    if w <= 0 || h <= 0 || frame.pixels.len() < (frame.width * frame.height * 4) as usize {
        return None;
    }

    // Allocate an owned NSBitmapImageRep (`planes = NULL`) and copy RGBA pixels into `bitmapData`
    // so NSImage never holds a dangling pointer after `frame` drops.
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            w,
            h,
            8,
            4,
            true,
            false,
            NSCalibratedRGBColorSpace,
            w * 4,
            32,
        )?
    };

    let dst = rep.bitmapData();
    if dst.is_null() {
        return None;
    }
    let byte_len = (frame.width * frame.height * 4) as usize;
    // SAFETY: `rep` allocated `w * 4 * h` bytes of owned bitmap storage.
    unsafe {
        std::ptr::copy_nonoverlapping(frame.pixels.as_ptr(), dst, byte_len);
    }

    let size = NSSize::new(frame.width as f64, frame.height as f64);
    let image = NSImage::initWithSize(NSImage::alloc(), size);
    image.addRepresentation(&rep);
    Some(image)
}

/// Renders only the target `NSWindow`'s own view hierarchy into a standalone PNG file,
/// guaranteeing zero desktop wallpaper, notifications, or third-party window leakage.
pub fn capture_window_view_to_png(window: &NSWindow, output_path: &Path) -> Result<(), String> {
    let view = window
        .contentView()
        .ok_or("Window has no contentView to capture")?;
    let bounds = view.bounds();
    let rep = view
        .bitmapImageRepForCachingDisplayInRect(bounds)
        .ok_or("Failed to create offscreen bitmap representation")?;
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);

    let empty_props = NSDictionary::new();
    let png_data =
        unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &empty_props) }
            .ok_or("Failed to encode window bitmap as PNG")?;

    let bytes = png_data.to_vec();
    crate::fs_safety::atomic_write_private(output_path, &bytes)
        .map_err(|e| format!("Failed writing PNG screenshot: {e}"))
}
