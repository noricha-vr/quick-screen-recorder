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
use objc2_app_kit::{NSBezierPath, NSButton, NSColor, NSPopUpButton, NSTextField, NSView};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer};

use crate::export::avfoundation::VideoMetadata;
use crate::export::{ExportFormat, ExportRequest, ExportResult, run_export_with_options};
use crate::fs_safety::{format_tilde_path, sanitize_terminal_text};
use crate::ui::clipboard::copy_file_to_clipboard;

pub const SIZE_PRESETS: [(&str, Option<u32>); 5] = [
    ("Original size", None),
    ("Max 1920 px", Some(1920)),
    ("Max 1280 px", Some(1280)),
    ("Max 800 px", Some(800)),
    ("Max 640 px", Some(640)),
];

pub const FPS_PRESETS: [u32; 3] = [15, 30, 60];

pub struct EditorState {
    pub input_path: PathBuf,
    pub meta: VideoMetadata,
    pub start_sec: f64,
    pub end_sec: f64,
    pub format: ExportFormat,
    pub fps: u32,
    pub max_width: Option<u32>,
    pub export_dir: Option<PathBuf>,
    pub default_stem: String,
    pub mute_audio: bool,
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

pub struct ExportUiControls {
    pub status_label: Retained<NSTextField>,
    pub progress_view: Retained<ExportProgressView>,
    pub copy_btn: Retained<NSButton>,
    pub save_btn: Retained<NSButton>,
    pub folder_popup: Retained<NSPopUpButton>,
    pub name_field: Retained<NSTextField>,
    pub mute_checkbox: Retained<NSButton>,
}

fn can_toggle_mute(state: &EditorState) -> bool {
    state.meta.has_audio && !matches!(state.format, ExportFormat::Gif | ExportFormat::Apng)
}

pub fn spawn_async_export(
    req: ExportRequest,
    copy_to_pb: bool,
    state: Rc<RefCell<EditorState>>,
    ui: ExportUiControls,
    on_saved: Rc<dyn Fn(PathBuf)>,
) {
    let fmt_label = req.format.label();
    ui.status_label
        .setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            0.58, 0.78, 1.0, 1.0,
        )));
    ui.status_label.setStringValue(&NSString::from_str(&format!(
        "Exporting {fmt_label}...  0%"
    )));
    ui.progress_view
        .set_progress(25, ProgressVisualState::Running);
    ui.copy_btn.setEnabled(false);
    ui.save_btn.setEnabled(false);
    ui.folder_popup.setEnabled(false);
    ui.name_field.setEnabled(false);
    ui.mute_checkbox.setEnabled(false);

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
                ui.progress_view
                    .set_progress(pm.max(25), ProgressVisualState::Running);
                ui.status_label.setStringValue(&NSString::from_str(&format!(
                    "Exporting {fmt_label}...  {pct}%"
                )));
            }
            Ok(result) => {
                unsafe { timer.as_ref().invalidate() };
                let enable_mute = {
                    let mut st = state.borrow_mut();
                    st.busy = false;
                    can_toggle_mute(&st)
                };
                ui.copy_btn.setEnabled(true);
                ui.save_btn.setEnabled(true);
                ui.folder_popup.setEnabled(true);
                ui.name_field.setEnabled(true);
                ui.mute_checkbox.setEnabled(enable_mute);

                match result {
                    Ok(res) => {
                        let kb = (res.output_bytes / 1024).max(1);
                        if copy_to_pb {
                            match copy_file_to_clipboard(&res.output_path) {
                                Ok(()) => {
                                    ui.progress_view
                                        .set_progress(1000, ProgressVisualState::Succeeded);
                                    ui.status_label
                                        .setTextColor(Some(&NSColor::systemGreenColor()));
                                    ui.status_label.setStringValue(&NSString::from_str(&format!(
                                        "Copied to clipboard ({} ms · {} KB)",
                                        res.elapsed_ms, kb
                                    )));
                                }
                                Err(e) => {
                                    ui.progress_view
                                        .set_progress(1000, ProgressVisualState::Failed);
                                    ui.status_label
                                        .setTextColor(Some(&NSColor::systemRedColor()));
                                    let safe = sanitize_terminal_text(&e, false);
                                    ui.status_label.setStringValue(&NSString::from_str(&safe));
                                }
                            }
                        } else {
                            ui.progress_view
                                .set_progress(1000, ProgressVisualState::Succeeded);
                            ui.status_label
                                .setTextColor(Some(&NSColor::systemGreenColor()));
                            let fname = res
                                .output_path
                                .file_name()
                                .map(|n| sanitize_terminal_text(&n.to_string_lossy(), false))
                                .unwrap_or_else(|| "Quick-Screen-Recorder".to_string());
                            let dir_lbl = res
                                .output_path
                                .parent()
                                .map(format_tilde_path)
                                .unwrap_or_else(|| "~/Movies/Quick Screen Recorder".to_string());
                            ui.status_label.setStringValue(&NSString::from_str(&format!(
                                "Saved {fname} to {dir_lbl} ({} ms · {} KB)",
                                res.elapsed_ms, kb
                            )));
                            on_saved(res.output_path);
                        }
                    }
                    Err(err) => {
                        ui.progress_view
                            .set_progress(1000, ProgressVisualState::Failed);
                        ui.status_label
                            .setTextColor(Some(&NSColor::systemRedColor()));
                        let safe = sanitize_terminal_text(&err, false);
                        ui.status_label.setStringValue(&NSString::from_str(&safe));
                    }
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                unsafe { timer.as_ref().invalidate() };
                let enable_mute = {
                    let mut st = state.borrow_mut();
                    st.busy = false;
                    can_toggle_mute(&st)
                };
                ui.copy_btn.setEnabled(true);
                ui.save_btn.setEnabled(true);
                ui.folder_popup.setEnabled(true);
                ui.name_field.setEnabled(true);
                ui.mute_checkbox.setEnabled(enable_mute);
                ui.progress_view
                    .set_progress(1000, ProgressVisualState::Failed);
                ui.status_label
                    .setTextColor(Some(&NSColor::systemRedColor()));
                ui.status_label
                    .setStringValue(&NSString::from_str("Export worker exited unexpectedly"));
            }
        }
    });

    unsafe {
        let _ = NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.035, true, &poll_block);
    }
}
