use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBezierPath, NSBox, NSBoxType, NSButton, NSColor, NSImageScaling, NSImageView, NSPopUpButton,
    NSTextField, NSView,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer};

use crate::config::Preferences;
use crate::export::avfoundation::{VideoMetadata, extract_rgba_frame};
use crate::export::{ExportFormat, ExportRequest, ExportResult, run_export_with_options};
use crate::fs_safety::{format_tilde_path, sanitize_terminal_text};
use crate::ui::clipboard::{copy_file_to_clipboard, rgba_to_nsimage};

pub const SIZE_PRESETS: [(&str, Option<u32>); 5] = [
    ("Original size", None),
    ("Max 1920 px", Some(1920)),
    ("Max 1280 px", Some(1280)),
    ("Max 800 px", Some(800)),
    ("Max 640 px", Some(640)),
];

pub const FPS_PRESETS: [u32; 3] = [15, 30, 60];

pub struct ExportPopups {
    pub format_popup: Retained<NSPopUpButton>,
    pub fps_popup: Retained<NSPopUpButton>,
    pub size_popup: Retained<NSPopUpButton>,
    pub initial_fmt: ExportFormat,
    pub initial_fps: u32,
    pub initial_max_width: Option<u32>,
}

pub fn build_export_popups(
    mtm: MainThreadMarker,
    root: &NSView,
    prefs: &Preferences,
    demo_showcase: bool,
) -> ExportPopups {
    let format_popup = NSPopUpButton::new(mtm);
    for fmt in ExportFormat::ALL {
        let title = if fmt.is_available() {
            fmt.label().to_string()
        } else {
            format!("{} (needs ffmpeg)", fmt.label())
        };
        format_popup.addItemWithTitle(&NSString::from_str(&title));
    }
    let initial_fmt = if demo_showcase {
        ExportFormat::Gif
    } else if prefs.default_export_format.is_available() {
        prefs.default_export_format
    } else {
        ExportFormat::Mp4
    };
    if let Some(idx) = ExportFormat::ALL.iter().position(|&f| f == initial_fmt) {
        format_popup.selectItemAtIndex(idx as isize);
    }
    format_popup.setFrame(NSRect::new(
        NSPoint::new(20.0, 88.0),
        NSSize::new(158.0, 28.0),
    ));
    root.addSubview(&format_popup);

    let fps_popup = NSPopUpButton::new(mtm);
    for fps in FPS_PRESETS {
        fps_popup.addItemWithTitle(&NSString::from_str(&format!("{fps} fps")));
    }
    let initial_fps = if matches!(initial_fmt, ExportFormat::Gif | ExportFormat::Apng) {
        30
    } else {
        60
    };
    let fps_idx = FPS_PRESETS
        .iter()
        .position(|&f| f == initial_fps)
        .unwrap_or(1);
    fps_popup.selectItemAtIndex(fps_idx as isize);
    fps_popup.setFrame(NSRect::new(
        NSPoint::new(184.0, 88.0),
        NSSize::new(88.0, 28.0),
    ));
    root.addSubview(&fps_popup);

    let size_popup = NSPopUpButton::new(mtm);
    for (title, _) in SIZE_PRESETS {
        size_popup.addItemWithTitle(&NSString::from_str(title));
    }
    let size_idx = SIZE_PRESETS
        .iter()
        .position(|(_, w)| *w == prefs.export_max_width)
        .unwrap_or(0);
    size_popup.selectItemAtIndex(size_idx as isize);
    size_popup.setFrame(NSRect::new(
        NSPoint::new(278.0, 88.0),
        NSSize::new(134.0, 28.0),
    ));
    root.addSubview(&size_popup);

    ExportPopups {
        format_popup,
        fps_popup,
        size_popup,
        initial_fmt,
        initial_fps,
        initial_max_width: SIZE_PRESETS[size_idx].1,
    }
}

pub struct EditorState {
    pub input_path: PathBuf,
    pub meta: VideoMetadata,
    pub start_sec: f64,
    pub end_sec: f64,
    pub format: ExportFormat,
    pub fps: u32,
    pub max_width: Option<u32>,
    pub export_dir: Option<PathBuf>,
    pub busy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressVisualState {
    Idle,
    Running,
    Succeeded,
    Failed,
}

pub struct ProgressIvars {
    permille: Cell<u32>,
    visual: Cell<ProgressVisualState>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "QSRExportProgressView"]
    #[ivars = ProgressIvars]
    pub struct ExportProgressView;

    unsafe impl NSObjectProtocol for ExportProgressView {}

    impl ExportProgressView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let bounds = self.bounds();
            let r = (bounds.size.height * 0.5).max(2.0);
            let track_bg = NSColor::colorWithSRGBRed_green_blue_alpha(0.14, 0.17, 0.24, 1.0);
            track_bg.setFill();
            let track_path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(bounds, r, r);
            track_path.fill();

            let border = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.11);
            border.setStroke();
            track_path.setLineWidth(1.0);
            track_path.stroke();

            let visual = self.ivars().visual.get();
            let pm = self.ivars().permille.get().min(1000);
            if visual == ProgressVisualState::Idle || pm == 0 {
                return;
            }

            let frac = (f64::from(pm) / 1000.0).clamp(0.03, 1.0);
            let fill_w = (bounds.size.width * frac).max(bounds.size.height);
            let fill_rect = NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(fill_w.min(bounds.size.width), bounds.size.height),
            );
            let fill_color = match visual {
                ProgressVisualState::Running => {
                    NSColor::colorWithSRGBRed_green_blue_alpha(0.22, 0.60, 1.0, 1.0)
                }
                ProgressVisualState::Succeeded => {
                    NSColor::colorWithSRGBRed_green_blue_alpha(0.19, 0.82, 0.35, 1.0)
                }
                ProgressVisualState::Failed => {
                    NSColor::colorWithSRGBRed_green_blue_alpha(0.96, 0.33, 0.31, 1.0)
                }
                ProgressVisualState::Idle => return,
            };
            fill_color.setFill();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(fill_rect, r, r).fill();
        }
    }
);

impl ExportProgressView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ProgressIvars {
            permille: Cell::new(0),
            visual: Cell::new(ProgressVisualState::Idle),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    pub fn set_progress(&self, permille: u32, visual: ProgressVisualState) {
        self.ivars().permille.set(permille.min(1000));
        self.ivars().visual.set(visual);
        self.setNeedsDisplay(true);
    }
}

pub fn build_preview_stage(
    mtm: MainThreadMarker,
    root: &NSView,
    win_w: f64,
    win_h: f64,
    input_path: &std::path::Path,
    initial_preview_sec: f64,
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
        NSRect::new(NSPoint::new(20.0, 204.0), NSSize::new(win_w - 40.0, 346.0)),
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
        NSRect::new(NSPoint::new(28.0, 212.0), NSSize::new(win_w - 56.0, 330.0)),
    );
    image_view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
    root.addSubview(&image_view);

    if let Ok(frame) = extract_rgba_frame(input_path, initial_preview_sec, Some(960)) {
        if let Some(img) = rgba_to_nsimage(&frame) {
            image_view.setImage(Some(&img));
        }
    }
    image_view
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_async_export(
    req: ExportRequest,
    copy_to_pb: bool,
    state: Rc<RefCell<EditorState>>,
    status_label: Retained<NSTextField>,
    progress_view: Retained<ExportProgressView>,
    copy_btn: Retained<NSButton>,
    save_btn: Retained<NSButton>,
    folder_popup: Retained<NSPopUpButton>,
) {
    let fmt_label = req.format.label();
    status_label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.58, 0.78, 1.0, 1.0,
    )));
    status_label.setStringValue(&NSString::from_str(&format!(
        "Exporting {fmt_label}...  0%"
    )));
    progress_view.set_progress(25, ProgressVisualState::Running);
    copy_btn.setEnabled(false);
    save_btn.setEnabled(false);
    folder_popup.setEnabled(false);

    let progress = Arc::new(AtomicU32::new(25));
    let progress_bg = Arc::clone(&progress);
    let (tx, rx) = mpsc::channel::<Result<ExportResult, String>>();
    thread::spawn(move || {
        let _ = tx.send(run_export_with_options(&req, Some(&progress_bg), true));
    });

    let poll_block = RcBlock::new(move |timer: std::ptr::NonNull<NSTimer>| {
        let pm = progress.load(Ordering::Relaxed).min(1000);
        match rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {
                let pct = (pm / 10).min(99);
                progress_view.set_progress(pm.max(25), ProgressVisualState::Running);
                status_label.setStringValue(&NSString::from_str(&format!(
                    "Exporting {fmt_label}...  {pct}%"
                )));
            }
            Ok(result) => {
                unsafe { timer.as_ref().invalidate() };
                state.borrow_mut().busy = false;
                copy_btn.setEnabled(true);
                save_btn.setEnabled(true);
                folder_popup.setEnabled(true);

                match result {
                    Ok(res) => {
                        let kb = (res.output_bytes / 1024).max(1);
                        if copy_to_pb {
                            match copy_file_to_clipboard(&res.output_path) {
                                Ok(()) => {
                                    progress_view
                                        .set_progress(1000, ProgressVisualState::Succeeded);
                                    status_label.setTextColor(Some(&NSColor::systemGreenColor()));
                                    status_label.setStringValue(&NSString::from_str(&format!(
                                        "Copied to clipboard ({} ms · {} KB)",
                                        res.elapsed_ms, kb
                                    )));
                                }
                                Err(e) => {
                                    progress_view.set_progress(1000, ProgressVisualState::Failed);
                                    status_label.setTextColor(Some(&NSColor::systemRedColor()));
                                    let safe = sanitize_terminal_text(&e, false);
                                    status_label.setStringValue(&NSString::from_str(&safe));
                                }
                            }
                        } else {
                            progress_view.set_progress(1000, ProgressVisualState::Succeeded);
                            status_label.setTextColor(Some(&NSColor::systemGreenColor()));
                            let fname = res
                                .output_path
                                .file_name()
                                .map(|n| sanitize_terminal_text(&n.to_string_lossy(), false))
                                .unwrap_or_else(|| "Recording".to_string());
                            let dir_lbl = res
                                .output_path
                                .parent()
                                .map(format_tilde_path)
                                .unwrap_or_else(|| "~/Desktop".to_string());
                            status_label.setStringValue(&NSString::from_str(&format!(
                                "Saved {fname} to {dir_lbl} ({} ms · {} KB)",
                                res.elapsed_ms, kb
                            )));
                        }
                    }
                    Err(err) => {
                        progress_view.set_progress(1000, ProgressVisualState::Failed);
                        status_label.setTextColor(Some(&NSColor::systemRedColor()));
                        let safe = sanitize_terminal_text(&err, false);
                        status_label.setStringValue(&NSString::from_str(&safe));
                    }
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                unsafe { timer.as_ref().invalidate() };
                state.borrow_mut().busy = false;
                copy_btn.setEnabled(true);
                save_btn.setEnabled(true);
                folder_popup.setEnabled(true);
                progress_view.set_progress(1000, ProgressVisualState::Failed);
                status_label.setTextColor(Some(&NSColor::systemRedColor()));
                status_label
                    .setStringValue(&NSString::from_str("Export worker exited unexpectedly"));
            }
        }
    });

    unsafe {
        let _ = NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.035, true, &poll_block);
    }
}
