use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBox, NSBoxType, NSButton, NSColor, NSImageScaling, NSImageView, NSTextField, NSView,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSTimer};

use crate::export::avfoundation::{VideoMetadata, extract_rgba_frame};
use crate::export::{ExportFormat, ExportRequest, ExportResult, run_export};
use crate::ui::clipboard::{copy_file_to_clipboard, rgba_to_nsimage};

pub struct EditorState {
    pub input_path: PathBuf,
    pub meta: VideoMetadata,
    pub start_sec: f64,
    pub end_sec: f64,
    pub format: ExportFormat,
    pub fps: u32,
    pub max_width: Option<u32>,
    pub busy: bool,
}

pub fn build_preview_stage(
    mtm: MainThreadMarker,
    root: &NSView,
    win_w: f64,
    win_h: f64,
    input_path: &std::path::Path,
) -> Retained<NSImageView> {
    let full_bg = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(win_w, win_h)),
    );
    full_bg.setBoxType(NSBoxType::Custom);
    full_bg.setBorderWidth(0.0);
    full_bg.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.09, 0.10, 0.14, 1.0,
    ));
    root.addSubview(&full_bg);

    let stage_box = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        NSRect::new(NSPoint::new(20.0, 174.0), NSSize::new(win_w - 40.0, 346.0)),
    );
    stage_box.setBoxType(NSBoxType::Custom);
    stage_box.setCornerRadius(10.0);
    stage_box.setBorderWidth(1.0);
    stage_box.setBorderColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        1.0, 1.0, 1.0, 0.12,
    ));
    stage_box.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.04, 0.05, 0.07, 1.0,
    ));
    root.addSubview(&stage_box);

    let image_view = NSImageView::initWithFrame(
        NSImageView::alloc(mtm),
        NSRect::new(NSPoint::new(28.0, 182.0), NSSize::new(win_w - 56.0, 330.0)),
    );
    image_view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
    root.addSubview(&image_view);

    if let Ok(frame) = extract_rgba_frame(input_path, 0.0, Some(960)) {
        if let Some(img) = rgba_to_nsimage(&frame) {
            image_view.setImage(Some(&img));
        }
    }
    image_view
}

pub fn spawn_async_export(
    req: ExportRequest,
    copy_to_pb: bool,
    state: Rc<RefCell<EditorState>>,
    status_label: Retained<NSTextField>,
    copy_btn: Retained<NSButton>,
    save_btn: Retained<NSButton>,
) {
    status_label.setTextColor(Some(&NSColor::systemYellowColor()));
    status_label.setStringValue(&NSString::from_str(&format!(
        "Exporting {}...",
        req.format.label()
    )));
    copy_btn.setEnabled(false);
    save_btn.setEnabled(false);

    let (tx, rx) = mpsc::channel::<Result<ExportResult, String>>();
    thread::spawn(move || {
        let _ = tx.send(run_export(&req));
    });

    let poll_block = RcBlock::new(move |timer: std::ptr::NonNull<NSTimer>| {
        if let Ok(result) = rx.try_recv() {
            unsafe { timer.as_ref().invalidate() };
            state.borrow_mut().busy = false;
            copy_btn.setEnabled(true);
            save_btn.setEnabled(true);

            match result {
                Ok(res) => {
                    let kb = (res.output_bytes / 1024).max(1);
                    if copy_to_pb {
                        match copy_file_to_clipboard(&res.output_path) {
                            Ok(()) => {
                                status_label.setTextColor(Some(&NSColor::systemGreenColor()));
                                status_label.setStringValue(&NSString::from_str(&format!(
                                    "Copied to clipboard ({} ms · {} KB)",
                                    res.elapsed_ms, kb
                                )));
                            }
                            Err(e) => {
                                status_label.setTextColor(Some(&NSColor::systemRedColor()));
                                status_label.setStringValue(&NSString::from_str(&e));
                            }
                        }
                    } else {
                        status_label.setTextColor(Some(&NSColor::systemGreenColor()));
                        status_label.setStringValue(&NSString::from_str(&format!(
                            "Saved ({} ms · {} KB)",
                            res.elapsed_ms, kb
                        )));
                    }
                }
                Err(err) => {
                    status_label.setTextColor(Some(&NSColor::systemRedColor()));
                    status_label.setStringValue(&NSString::from_str(&err));
                }
            }
        }
    });

    unsafe {
        let _ = NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.05, true, &poll_block);
    }
}
