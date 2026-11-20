use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterInputPixelBufferAdaptor, AVAssetWriterStatus,
    AVFileTypeMPEG4, AVMediaTypeVideo, AVVideoCodecKey, AVVideoCodecTypeH264, AVVideoCodecTypeHEVC,
    AVVideoHeightKey, AVVideoWidthKey,
};
use objc2_core_media::{CMTime, CMTimeFlags};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};

use crate::capture::RecordingCodec;
use crate::export::avfoundation::RgbaFrame;

/// Streams generated RGBA frames one-by-one into a metadata-free H.264 or HEVC `.mp4` via `AVAssetWriter`.
/// Keeps peak memory bounded to a single frame regardless of clip duration or 4K resolution.
pub fn stream_rgba_frames_to_mp4<F>(
    width: u32,
    height: u32,
    frame_count: usize,
    fps: u32,
    codec: RecordingCodec,
    output_path: &Path,
    mut next_frame: F,
) -> Result<(), String>
where
    F: FnMut(usize) -> Result<RgbaFrame, String>,
{
    if frame_count == 0 {
        return Err("Cannot write MP4 with zero frames".to_string());
    }
    let w = (width.max(2) & !1) as usize;
    let h = (height.max(2) & !1) as usize;

    if output_path.exists() {
        std::fs::remove_file(output_path)
            .map_err(|e| format!("Failed removing existing staged target: {e}"))?;
    }

    let path_str = output_path
        .to_str()
        .ok_or("Output path contains invalid UTF-8")?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(path_str));
    let mp4_type = unsafe { AVFileTypeMPEG4 }.ok_or("AVFileTypeMPEG4 unavailable")?;
    let video_media = unsafe { AVMediaTypeVideo }.ok_or("AVMediaTypeVideo unavailable")?;

    let writer = unsafe {
        AVAssetWriter::assetWriterWithURL_fileType_error(&url, mp4_type)
            .map_err(|e| format!("AVAssetWriter init failed: {}", e.localizedDescription()))?
    };

    let codec_key = unsafe { AVVideoCodecKey }.ok_or("AVVideoCodecKey unavailable")?;
    let width_key = unsafe { AVVideoWidthKey }.ok_or("AVVideoWidthKey unavailable")?;
    let height_key = unsafe { AVVideoHeightKey }.ok_or("AVVideoHeightKey unavailable")?;
    let codec_val = match codec {
        RecordingCodec::H264 => unsafe { AVVideoCodecTypeH264 }.ok_or("H264 unavailable")?,
        RecordingCodec::Hevc => unsafe { AVVideoCodecTypeHEVC }.ok_or("HEVC unavailable")?,
    };

    let keys: [&NSString; 3] = [codec_key, width_key, height_key];
    let w_num = NSNumber::new_usize(w);
    let h_num = NSNumber::new_usize(h);
    let vals: [&AnyObject; 3] = [codec_val.as_ref(), w_num.as_ref(), h_num.as_ref()];
    let settings: Retained<NSDictionary<NSString, AnyObject>> =
        NSDictionary::from_slices(&keys, &vals);

    let input = unsafe {
        AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
            video_media,
            Some(&settings),
        )
    };
    let adaptor = unsafe {
        AVAssetWriterInputPixelBufferAdaptor::assetWriterInputPixelBufferAdaptorWithAssetWriterInput_sourcePixelBufferAttributes(
            &input,
            None,
        )
    };

    let timescale = fps.clamp(1, 120) as i32;
    unsafe {
        let empty_meta = NSArray::new();
        writer.setMetadata(&empty_meta);
        writer.setShouldOptimizeForNetworkUse(true);
        if !writer.canAddInput(&input) {
            return Err("AVAssetWriter rejected video input settings".to_string());
        }
        writer.addInput(&input);
        if !writer.startWriting() {
            let detail = writer
                .error()
                .map(|e| e.localizedDescription().to_string())
                .unwrap_or_else(|| "unknown error".to_string());
            return Err(format!("AVAssetWriter failed starting: {detail}"));
        }
        writer.startSessionAtSourceTime(CMTime {
            value: 0,
            timescale,
            flags: CMTimeFlags(1),
            epoch: 0,
        });
    }

    for idx in 0..frame_count {
        let frame = next_frame(idx)?;
        let mut spins = 0_u32;
        while !unsafe { input.isReadyForMoreMediaData() } {
            thread::sleep(Duration::from_millis(2));
            spins += 1;
            if spins > 2500 {
                return Err("Timed out waiting for hardware video encoder input queue".to_string());
            }
        }
        let pb = rgba_to_bgra_pixel_buffer(&frame, w, h)?;
        let pts = CMTime {
            value: idx as i64,
            timescale,
            flags: CMTimeFlags(1),
            epoch: 0,
        };
        let ok = unsafe { adaptor.appendPixelBuffer_withPresentationTime(&pb, pts) };
        if !ok {
            let detail = unsafe {
                writer
                    .error()
                    .map(|e| e.localizedDescription().to_string())
                    .unwrap_or_else(|| "appendPixelBuffer failed".to_string())
            };
            return Err(format!("AVAssetWriter frame {idx} error: {detail}"));
        }
    }

    let (tx, rx) = mpsc::sync_channel::<()>(1);
    let block = RcBlock::new(move || {
        let _ = tx.send(());
    });
    unsafe {
        input.markAsFinished();
        writer.finishWritingWithCompletionHandler(&block);
    }
    rx.recv_timeout(Duration::from_secs(60))
        .map_err(|_| "Timed out finalizing MP4 with AVAssetWriter".to_string())?;

    let status = unsafe { writer.status() };
    if status == AVAssetWriterStatus::Completed {
        Ok(())
    } else {
        let detail = unsafe {
            writer
                .error()
                .map(|e| e.localizedDescription().to_string())
                .unwrap_or_else(|| format!("status={}", status.0))
        };
        Err(format!("AVAssetWriter finalization failed: {detail}"))
    }
}

pub fn write_rgba_frames_to_mp4(
    frames: &[RgbaFrame],
    fps: u32,
    codec: RecordingCodec,
    output_path: &Path,
) -> Result<(), String> {
    let first = frames.first().ok_or("Cannot write MP4 with zero frames")?;
    stream_rgba_frames_to_mp4(
        first.width,
        first.height,
        frames.len(),
        fps,
        codec,
        output_path,
        |idx| Ok(frames[idx].clone()),
    )
}

fn rgba_to_bgra_pixel_buffer(
    frame: &RgbaFrame,
    width: usize,
    height: usize,
) -> Result<Retained<CVPixelBuffer>, String> {
    let mut pb_ptr: *mut CVPixelBuffer = ptr::null_mut();
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            width,
            height,
            kCVPixelFormatType_32BGRA,
            None,
            NonNull::from(&mut pb_ptr),
        )
    };
    if status != 0 || pb_ptr.is_null() {
        return Err(format!("CVPixelBufferCreate failed with status {status}"));
    }
    let pb = unsafe { Retained::from_raw(pb_ptr) }.ok_or("Null CVPixelBuffer")?;

    unsafe {
        CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags(0));
    }
    let base = CVPixelBufferGetBaseAddress(&pb) as *mut u8;
    let dst_stride = CVPixelBufferGetBytesPerRow(&pb);
    let copy_w = width.min(frame.width as usize);
    let copy_h = height.min(frame.height as usize);

    if !base.is_null() {
        for y in 0..copy_h {
            for x in 0..copy_w {
                let si = (y * (frame.width as usize) + x) * 4;
                let di = y * dst_stride + x * 4;
                unsafe {
                    *base.add(di) = frame.pixels[si + 2];
                    *base.add(di + 1) = frame.pixels[si + 1];
                    *base.add(di + 2) = frame.pixels[si];
                    *base.add(di + 3) = frame.pixels[si + 3];
                }
            }
        }
    }
    unsafe {
        CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags(0));
    }
    Ok(pb)
}
