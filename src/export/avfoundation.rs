use std::path::Path;
use std::ptr;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAsset, AVAssetExportPresetHEVCHighestQuality, AVAssetExportPresetHighestQuality,
    AVAssetExportPresetPassthrough, AVAssetExportSession, AVAssetExportSessionStatus,
    AVAssetImageGenerator, AVAssetReferenceRestrictions, AVFileTypeMPEG4, AVMediaTypeAudio,
    AVMediaTypeVideo, AVMetadataItemFilter, AVMutableVideoComposition,
    AVMutableVideoCompositionInstruction, AVMutableVideoCompositionLayerInstruction, AVURLAsset,
    AVURLAssetReferenceRestrictionsKey, AVVideoCompositionInstructionProtocol,
};
use objc2_core_foundation::{CGAffineTransform, CGSize};
use objc2_core_media::{CMFormatDescription, CMTime, CMTimeFlags, CMTimeRange};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};

use crate::capture::RecordingCodec;
use crate::export::av_decode::cgimage_to_rgba_sized;
pub use crate::export::av_decode::{RgbaFrame, extract_rgba_frame, stream_rgba_sequence};
use crate::export::av_writer::StreamingMp4Writer;
use crate::export::{ExportFormat, ExportRequest, SharedExportProgress, set_progress_permille};
use crate::geometry::scaled_even_dimensions;

pub const MAX_ANIMATED_FRAMES: usize = 1800;
const FOURCC_AVC1: u32 = u32::from_be_bytes(*b"avc1");
const FOURCC_HVC1: u32 = u32::from_be_bytes(*b"hvc1");
const FOURCC_HEV1: u32 = u32::from_be_bytes(*b"hev1");

#[derive(Debug, Clone, Copy)]
pub struct VideoMetadata {
    pub duration_sec: f64,
    pub width: u32,
    pub height: u32,
    pub nominal_fps: f32,
    pub codec_fourcc: u32,
    pub has_video: bool,
    pub is_playable: bool,
    pub has_audio: bool,
    pub has_extra_tracks: bool,
    pub common_metadata_count: usize,
    pub reference_restrictions: usize,
}

impl VideoMetadata {
    pub fn is_h264(&self) -> bool {
        self.codec_fourcc == FOURCC_AVC1
    }

    pub fn is_hevc(&self) -> bool {
        self.codec_fourcc == FOURCC_HVC1 || self.codec_fourcc == FOURCC_HEV1
    }
}

/// Opens a local media file with `AVAssetReferenceRestrictions::ForbidAll` so container reference
/// tracks can never trigger remote HTTP/TCP fetches or external local-file reads.
pub fn open_restricted_asset(path: &Path) -> Result<Retained<AVAsset>, String> {
    let url = file_url(path)?;
    let key: &NSString = unsafe { AVURLAssetReferenceRestrictionsKey };
    let val = NSNumber::new_usize(AVAssetReferenceRestrictions::ForbidAll.0);
    let keys: [&NSString; 1] = [key];
    let vals: [&AnyObject; 1] = [val.as_ref()];
    let opts: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(&keys, &vals);
    let url_asset = unsafe { AVURLAsset::URLAssetWithURL_options(&url, Some(&opts)) };
    Ok(Retained::into_super(url_asset))
}

pub fn probe_video_metadata(path: &Path) -> Result<VideoMetadata, String> {
    let asset = open_restricted_asset(path)?;
    let dur_cm = unsafe { asset.duration() };
    let duration_sec = cmtime_to_secs(dur_cm).max(0.05);
    let reference_restrictions = unsafe { asset.referenceRestrictions().0 };

    let mut width = 0_u32;
    let mut height = 0_u32;
    let mut nominal_fps = 60.0_f32;
    let mut codec_fourcc = 0_u32;
    let mut has_video = false;
    let mut is_playable = unsafe { asset.isPlayable() };
    let mut has_audio = false;
    let mut av_track_count = 0_usize;
    #[allow(deprecated)]
    let total_tracks = unsafe { asset.tracks().count() };
    let common_metadata_count = unsafe { asset.commonMetadata().count() };

    unsafe {
        if let Some(audio_type) = AVMediaTypeAudio {
            #[allow(deprecated)]
            let audio_tracks = asset.tracksWithMediaType(audio_type);
            av_track_count += audio_tracks.count();
            has_audio = audio_tracks.count() > 0;
        }
        if let Some(video_type) = AVMediaTypeVideo {
            #[allow(deprecated)]
            let tracks = asset.tracksWithMediaType(video_type);
            av_track_count += tracks.count();
            if tracks.count() > 0 {
                let track = tracks.objectAtIndex(0);
                has_video = true;
                is_playable = is_playable && track.isPlayable();
                let size = track.naturalSize();
                let tf = track.preferredTransform();
                let transformed_w = (size.width * tf.a + size.height * tf.c).abs();
                let transformed_h = (size.width * tf.b + size.height * tf.d).abs();
                width = transformed_w.round().max(2.0) as u32;
                height = transformed_h.round().max(2.0) as u32;
                let fps = track.nominalFrameRate();
                if fps > 0.5 {
                    nominal_fps = fps;
                }
                let descs = track.formatDescriptions();
                if descs.count() > 0 {
                    let raw_obj = descs.objectAtIndex(0);
                    let desc_ptr = Retained::as_ptr(&raw_obj) as *const CMFormatDescription;
                    if !desc_ptr.is_null() {
                        codec_fourcc = CMFormatDescription::media_sub_type(&*desc_ptr);
                    }
                }
            }
        }
        if width == 0 || height == 0 {
            #[allow(deprecated)]
            let size = asset.naturalSize();
            width = size.width.round().max(2.0) as u32;
            height = size.height.round().max(2.0) as u32;
        }
    }

    Ok(VideoMetadata {
        duration_sec,
        width,
        height,
        nominal_fps,
        codec_fourcc,
        has_video,
        is_playable,
        has_audio,
        has_extra_tracks: total_tracks > av_track_count,
        common_metadata_count,
        reference_restrictions,
    })
}

pub fn export_mp4_or_hevc(
    req: &ExportRequest,
    meta: &VideoMetadata,
    progress: Option<&SharedExportProgress>,
) -> Result<(), String> {
    let start = req.start_sec.unwrap_or(0.0);
    let end = req.end_sec.unwrap_or(meta.duration_sec);
    let (target_w, target_h) = scaled_even_dimensions(meta.width, meta.height, req.max_width);
    let clamped_fps = req.fps.clamp(1, 120);

    let codec_matches = match req.format {
        ExportFormat::Mp4 => meta.is_h264(),
        ExportFormat::Hevc => meta.is_hevc(),
        _ => false,
    };
    let size_matches = target_w == (meta.width & !1) && target_h == (meta.height & !1);
    let fps_tol = (f64::from(meta.nominal_fps) * 0.08).clamp(0.25, 1.5);
    let fps_matches = (f64::from(clamped_fps) - f64::from(meta.nominal_fps)).abs() <= fps_tol;

    let needs_mute = req.mute_audio && meta.has_audio;
    if codec_matches && size_matches && fps_matches && !meta.has_extra_tracks && !needs_mute {
        return run_export_session(req, start, end, None, true, progress);
    }

    if !meta.has_audio || req.mute_audio {
        return stream_video_only_transcode(
            req,
            meta,
            start,
            end,
            target_w,
            target_h,
            clamped_fps,
            progress,
        );
    }

    run_export_session(req, start, end, Some((target_w, target_h)), false, progress)
}

#[allow(clippy::too_many_arguments)]
fn stream_video_only_transcode(
    req: &ExportRequest,
    meta: &VideoMetadata,
    start_sec: f64,
    end_sec: f64,
    target_w: u32,
    target_h: u32,
    fps: u32,
    progress: Option<&SharedExportProgress>,
) -> Result<(), String> {
    let frame_step = 1.0 / f64::from(fps);
    let duration = (end_sec - start_sec).max(0.02);
    let total_frames = transcode_frame_count(duration, fps);

    let asset = open_restricted_asset(&req.input)?;
    let generator = unsafe { AVAssetImageGenerator::assetImageGeneratorWithAsset(&asset) };
    let tol = secs_to_cmtime((frame_step * 0.45).max(0.004));
    unsafe {
        generator.setAppliesPreferredTrackTransform(true);
        generator.setMaximumSize(CGSize::new(
            f64::from(target_w),
            f64::from(meta.height.max(target_h)),
        ));
        generator.setRequestedTimeToleranceBefore(tol);
        generator.setRequestedTimeToleranceAfter(tol);
    }

    let target_codec = match req.format {
        ExportFormat::Hevc => RecordingCodec::Hevc,
        _ => RecordingCodec::H264,
    };
    let mut last_good: Option<RgbaFrame> = None;
    let mut writer = StreamingMp4Writer::new(target_w, target_h, fps, target_codec, &req.output)?;

    for idx in 0..total_frames {
        autoreleasepool(|_| {
            let t = (start_sec + (idx as f64) * frame_step).min(meta.duration_sec);
            #[allow(deprecated)]
            let cg_res = unsafe {
                generator.copyCGImageAtTime_actualTime_error(secs_to_cmtime(t), ptr::null_mut())
            };
            let decoded = match cg_res {
                Ok(cg_img) => {
                    let frame =
                        cgimage_to_rgba_sized(&cg_img, target_w as usize, target_h as usize)?;
                    last_good = Some(frame.clone());
                    frame
                }
                Err(_) if last_good.is_some() => last_good.clone().unwrap(),
                Err(e) => {
                    return Err(format!(
                        "Failed decoding video frame {idx}: {}",
                        e.localizedDescription()
                    ));
                }
            };
            writer.append_rgba_frame(&decoded)?;
            Ok::<(), String>(())
        })?;
        let p = 50 + (((idx + 1) * 910) / total_frames.max(1)) as u32;
        set_progress_permille(progress, p);
    }
    autoreleasepool(|_| writer.finish_with_duration(Some(duration)))?;
    set_progress_permille(progress, 975);
    Ok(())
}

pub(crate) fn transcode_frame_count(duration_sec: f64, fps: u32) -> usize {
    let exact = duration_sec.max(0.02) * f64::from(fps.max(1));
    // Include every frame slot whose PTS `i / fps` falls inside `[0, duration_sec)`,
    // guarding against floating-point overshoot on exact multiples like `1.0 * 30`.
    ((exact - 1e-3).floor().max(0.0) as usize) + 1
}

fn run_export_session(
    req: &ExportRequest,
    start: f64,
    end: f64,
    resize_to: Option<(u32, u32)>,
    passthrough: bool,
    progress: Option<&SharedExportProgress>,
) -> Result<(), String> {
    let asset = open_restricted_asset(&req.input)?;
    let out_url = file_url(&req.output)?;

    let preset: &NSString = unsafe {
        if passthrough {
            AVAssetExportPresetPassthrough
        } else if req.format == ExportFormat::Hevc {
            AVAssetExportPresetHEVCHighestQuality
        } else {
            AVAssetExportPresetHighestQuality
        }
    };

    let session = unsafe {
        AVAssetExportSession::exportSessionWithAsset_presetName(&asset, preset)
            .ok_or_else(|| "Unable to create AVAssetExportSession".to_string())?
    };

    let time_range = CMTimeRange {
        start: secs_to_cmtime(start),
        duration: secs_to_cmtime((end - start).max(0.02)),
    };

    unsafe {
        session.setOutputURL(Some(&out_url));
        let supported = session.supportedFileTypes();
        if let Some(mp4_ft) = AVFileTypeMPEG4 {
            if supported.containsObject(mp4_ft) {
                session.setOutputFileType(Some(mp4_ft));
            } else if supported.count() > 0 {
                let first = supported.objectAtIndex(0);
                session.setOutputFileType(Some(&first));
            }
        }
        session.setTimeRange(time_range);
        session.setShouldOptimizeForNetworkUse(true);

        let empty_meta = NSArray::new();
        session.setMetadata(Some(&empty_meta));
        let sharing_filter = AVMetadataItemFilter::metadataItemFilterForSharing();
        session.setMetadataItemFilter(Some(&sharing_filter));

        if !passthrough {
            if let Some(comp) = build_video_composition(&asset, req.fps, resize_to) {
                session.setVideoComposition(Some(&comp));
            }
        }
    }

    let (tx, rx) = mpsc::sync_channel::<()>(1);
    let block = RcBlock::new(move || {
        let _ = tx.send(());
    });
    unsafe {
        #[allow(deprecated)]
        session.exportAsynchronouslyWithCompletionHandler(&block);
    }

    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match rx.recv_timeout(Duration::from_millis(35)) {
            Ok(()) => break,
            Err(RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    return Err("Timed out waiting for hardware video export".to_string());
                }
                let frac = unsafe { session.progress() }.clamp(0.0, 1.0);
                set_progress_permille(progress, 50 + (frac * 910.0) as u32);
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    set_progress_permille(progress, 975);

    let status = unsafe { session.status() };
    if status == AVAssetExportSessionStatus::Completed {
        Ok(())
    } else {
        let detail = unsafe {
            session
                .error()
                .map(|e| e.localizedDescription().to_string())
                .unwrap_or_else(|| format!("status={}", status.0))
        };
        Err(format!("Hardware video export failed: {detail}"))
    }
}

fn build_video_composition(
    asset: &AVAsset,
    fps: u32,
    resize_to: Option<(u32, u32)>,
) -> Option<Retained<AVMutableVideoComposition>> {
    unsafe {
        let video_type = AVMediaTypeVideo?;
        #[allow(deprecated)]
        let tracks = asset.tracksWithMediaType(video_type);
        if tracks.count() == 0 {
            return None;
        }
        let track = tracks.objectAtIndex(0);
        let natural = track.naturalSize();
        if natural.width <= 1.0 || natural.height <= 1.0 {
            return None;
        }

        let pref = track.preferredTransform();
        let corners = [
            (0.0, 0.0),
            (natural.width, 0.0),
            (0.0, natural.height),
            (natural.width, natural.height),
        ];
        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for (x, y) in corners {
            let tx = x * pref.a + y * pref.c + pref.tx;
            let ty = x * pref.b + y * pref.d + pref.ty;
            min_x = min_x.min(tx);
            min_y = min_y.min(ty);
            max_x = max_x.max(tx);
            max_y = max_y.max(ty);
        }
        let oriented_w = (max_x - min_x).max(2.0);
        let oriented_h = (max_y - min_y).max(2.0);
        let (tw, th) = resize_to.unwrap_or((
            oriented_w.round() as u32 & !1,
            oriented_h.round() as u32 & !1,
        ));
        let sx = f64::from(tw) / oriented_w;
        let sy = f64::from(th) / oriented_h;

        let combined_tf = CGAffineTransform {
            a: pref.a * sx,
            b: pref.b * sy,
            c: pref.c * sx,
            d: pref.d * sy,
            tx: (pref.tx - min_x) * sx,
            ty: (pref.ty - min_y) * sy,
        };

        let comp = AVMutableVideoComposition::videoComposition();
        comp.setRenderSize(CGSize::new(f64::from(tw), f64::from(th)));
        comp.setFrameDuration(CMTime {
            value: 1,
            timescale: fps.clamp(1, 120) as i32,
            flags: CMTimeFlags(1),
            epoch: 0,
        });

        let layer =
            AVMutableVideoCompositionLayerInstruction::videoCompositionLayerInstructionWithAssetTrack(
                &track,
            );
        layer.setTransform_atTime(combined_tf, secs_to_cmtime(0.0));

        let inst = AVMutableVideoCompositionInstruction::videoCompositionInstruction();
        inst.setTimeRange(CMTimeRange {
            start: secs_to_cmtime(0.0),
            duration: asset.duration(),
        });
        let layers = NSArray::from_retained_slice(&[Retained::into_super(layer)]);
        inst.setLayerInstructions(&layers);

        let proto_inst: Retained<
            objc2::runtime::ProtocolObject<dyn AVVideoCompositionInstructionProtocol>,
        > = objc2::runtime::ProtocolObject::from_retained(inst);
        let inst_arr = NSArray::from_retained_slice(&[proto_inst]);
        comp.setInstructions(&inst_arr);
        Some(comp)
    }
}

fn file_url(path: &Path) -> Result<Retained<NSURL>, String> {
    let s = path.to_str().ok_or("File path is not valid UTF-8")?;
    let ns = NSString::from_str(s);
    Ok(NSURL::fileURLWithPath(&ns))
}

pub(crate) fn secs_to_cmtime(secs: f64) -> CMTime {
    let timescale = 6000_i32;
    let value = (secs.max(0.0) * f64::from(timescale)).round() as i64;
    CMTime {
        value,
        timescale,
        flags: CMTimeFlags(1),
        epoch: 0,
    }
}

fn cmtime_to_secs(t: CMTime) -> f64 {
    if t.timescale <= 0 {
        0.0
    } else {
        (t.value as f64) / f64::from(t.timescale)
    }
}

#[cfg(test)]
mod tests {
    use super::transcode_frame_count;

    #[test]
    fn transcode_frame_count_matches_slots_inside_trimmed_duration() {
        assert_eq!(transcode_frame_count(0.7, 1), 1);
        assert_eq!(transcode_frame_count(1.0, 1), 1);
        assert_eq!(transcode_frame_count(1.5, 1), 2);
        assert_eq!(transcode_frame_count(2.0, 1), 2);
        assert_eq!(transcode_frame_count(1.0, 5), 5);
        assert_eq!(transcode_frame_count(1.0, 30), 30);
        assert_eq!(transcode_frame_count(0.6, 24), 15);
    }
}
