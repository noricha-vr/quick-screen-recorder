use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSBackingStoreType,
    NSButton, NSColor, NSFont, NSModalResponseOK, NSPopUpButton, NSSavePanel, NSScreen, NSSlider,
    NSTextField, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use crate::config::Preferences;
use crate::export::avfoundation::{extract_rgba_frame, probe_video_metadata};
use crate::export::{ExportFormat, ExportRequest};
use crate::ui::clipboard::rgba_to_nsimage;
use crate::ui::editor_worker::{EditorState, build_preview_stage, spawn_async_export};

const SIZE_PRESETS: [(&str, Option<u32>); 5] = [
    ("Original size", None),
    ("Max 1920 px", Some(1920)),
    ("Max 1280 px", Some(1280)),
    ("Max 800 px", Some(800)),
    ("Max 640 px", Some(640)),
];

const FPS_PRESETS: [u32; 3] = [15, 30, 60];

struct EditorActionIvars {
    on_start_scrub: Box<dyn Fn(f64)>,
    on_end_scrub: Box<dyn Fn(f64)>,
    on_format_change: Box<dyn Fn(isize)>,
    on_fps_change: Box<dyn Fn(isize)>,
    on_size_change: Box<dyn Fn(isize)>,
    on_copy: Box<dyn Fn()>,
    on_save: Box<dyn Fn()>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "QSREditorActions"]
    #[ivars = EditorActionIvars]
    struct EditorActions;

    unsafe impl NSObjectProtocol for EditorActions {}

    impl EditorActions {
        #[unsafe(method(startScrubbed:))]
        fn start_scrubbed(&self, sender: &NSSlider) {
            (self.ivars().on_start_scrub)(sender.doubleValue());
        }
        #[unsafe(method(endScrubbed:))]
        fn end_scrubbed(&self, sender: &NSSlider) {
            (self.ivars().on_end_scrub)(sender.doubleValue());
        }
        #[unsafe(method(formatChanged:))]
        fn format_changed(&self, sender: &NSPopUpButton) {
            (self.ivars().on_format_change)(sender.indexOfSelectedItem());
        }
        #[unsafe(method(fpsChanged:))]
        fn fps_changed(&self, sender: &NSPopUpButton) {
            (self.ivars().on_fps_change)(sender.indexOfSelectedItem());
        }
        #[unsafe(method(sizeChanged:))]
        fn size_changed(&self, sender: &NSPopUpButton) {
            (self.ivars().on_size_change)(sender.indexOfSelectedItem());
        }
        #[unsafe(method(copyClicked:))]
        fn copy_clicked(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_copy)();
        }
        #[unsafe(method(saveClicked:))]
        fn save_clicked(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_save)();
        }
    }
);

pub struct EditorWindow {
    pub window: Retained<NSWindow>,
    _actions: Retained<EditorActions>,
}

impl EditorWindow {
    pub fn open(
        mtm: MainThreadMarker,
        input_path: &Path,
        prefs: &Preferences,
    ) -> Result<Self, String> {
        let meta = probe_video_metadata(input_path)?;
        let (win_w, win_h) = (760.0, 540.0);
        let screen_frame = NSScreen::mainScreen(mtm)
            .map(|s| s.frame())
            .unwrap_or_else(|| NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1440.0, 900.0)));

        let rect = NSRect::new(
            NSPoint::new(
                screen_frame.origin.x + (screen_frame.size.width - win_w) * 0.5,
                screen_frame.origin.y + (screen_frame.size.height - win_h) * 0.5,
            ),
            NSSize::new(win_w, win_h),
        );

        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        if let Some(dark) = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }) {
            window.setAppearance(Some(&dark));
        }
        window.setTitle(&NSString::from_str("Quick Screen Recorder — Trim & Export"));
        window.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            0.09, 0.10, 0.14, 1.0,
        )));

        let root = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(win_w, win_h)),
        );
        window.setContentView(Some(&root));
        let image_view = build_preview_stage(mtm, &root, win_w, win_h, input_path);

        let info_label = NSTextField::labelWithString(
            &NSString::from_str(&format!(
                "{}×{}  ·  Clip {:.2}s – {:.2}s ({:.2}s)",
                meta.width, meta.height, 0.0, meta.duration_sec, meta.duration_sec
            )),
            mtm,
        );
        info_label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            0.84, 0.87, 0.93, 1.0,
        )));
        info_label.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(
            12.0, 0.2,
        )));
        info_label.setFrame(NSRect::new(
            NSPoint::new(22.0, 144.0),
            NSSize::new(500.0, 20.0),
        ));
        root.addSubview(&info_label);

        for (title, y) in [("Start", 114.0), ("End", 86.0)] {
            let lbl = NSTextField::labelWithString(&NSString::from_str(title), mtm);
            lbl.setTextColor(Some(&NSColor::lightGrayColor()));
            lbl.setFrame(NSRect::new(NSPoint::new(22.0, y), NSSize::new(42.0, 18.0)));
            root.addSubview(&lbl);
        }

        let format_popup = NSPopUpButton::new(mtm);
        for fmt in ExportFormat::ALL {
            let title = if fmt.is_available() {
                fmt.label().to_string()
            } else {
                format!("{} (needs ffmpeg)", fmt.label())
            };
            format_popup.addItemWithTitle(&NSString::from_str(&title));
        }
        let initial_fmt = if prefs.default_export_format.is_available() {
            prefs.default_export_format
        } else {
            ExportFormat::Mp4
        };
        if let Some(idx) = ExportFormat::ALL.iter().position(|&f| f == initial_fmt) {
            format_popup.selectItemAtIndex(idx as isize);
        }
        format_popup.setFrame(NSRect::new(
            NSPoint::new(20.0, 44.0),
            NSSize::new(172.0, 28.0),
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
            NSPoint::new(200.0, 44.0),
            NSSize::new(94.0, 28.0),
        ));
        root.addSubview(&fps_popup);

        let size_popup = NSPopUpButton::new(mtm);
        for (title, _) in SIZE_PRESETS {
            size_popup.addItemWithTitle(&NSString::from_str(title));
        }
        size_popup.selectItemAtIndex(0);
        size_popup.setFrame(NSRect::new(
            NSPoint::new(302.0, 44.0),
            NSSize::new(146.0, 28.0),
        ));
        root.addSubview(&size_popup);

        let status_label =
            NSTextField::labelWithString(&NSString::from_str("Ready to export"), mtm);
        status_label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            0.66, 0.72, 0.82, 1.0,
        )));
        status_label.setFrame(NSRect::new(
            NSPoint::new(22.0, 14.0),
            NSSize::new(470.0, 20.0),
        ));
        root.addSubview(&status_label);

        let state = Rc::new(RefCell::new(EditorState {
            input_path: input_path.to_path_buf(),
            meta,
            start_sec: 0.0,
            end_sec: meta.duration_sec,
            format: initial_fmt,
            fps: initial_fps,
            max_width: prefs.export_max_width,
            busy: false,
        }));

        let start_slider = unsafe {
            NSSlider::sliderWithValue_minValue_maxValue_target_action(
                0.0,
                0.0,
                meta.duration_sec.max(0.05),
                None,
                None,
                mtm,
            )
        };
        start_slider.setFrame(NSRect::new(
            NSPoint::new(68.0, 114.0),
            NSSize::new(win_w - 92.0, 20.0),
        ));
        root.addSubview(&start_slider);

        let end_slider = unsafe {
            NSSlider::sliderWithValue_minValue_maxValue_target_action(
                meta.duration_sec,
                0.0,
                meta.duration_sec.max(0.05),
                None,
                None,
                mtm,
            )
        };
        end_slider.setFrame(NSRect::new(
            NSPoint::new(68.0, 86.0),
            NSSize::new(win_w - 92.0, 20.0),
        ));
        root.addSubview(&end_slider);

        let copy_btn = unsafe {
            NSButton::buttonWithTitle_target_action(&NSString::from_str("Copy"), None, None, mtm)
        };
        copy_btn.setFrame(NSRect::new(
            NSPoint::new(536.0, 42.0),
            NSSize::new(96.0, 32.0),
        ));
        root.addSubview(&copy_btn);

        let save_btn = unsafe {
            NSButton::buttonWithTitle_target_action(&NSString::from_str("Save..."), None, None, mtm)
        };
        save_btn.setBezelColor(Some(&NSColor::systemBlueColor()));
        save_btn.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        save_btn.setKeyEquivalent(&NSString::from_str("\r"));
        save_btn.setFrame(NSRect::new(
            NSPoint::new(640.0, 42.0),
            NSSize::new(100.0, 32.0),
        ));
        root.addSubview(&save_btn);

        let update_preview = {
            let st_ref = Rc::clone(&state);
            let iv_ref = image_view.clone();
            let info_ref = info_label.clone();
            Rc::new(move |preview_time: f64| {
                let st = st_ref.borrow();
                let clip_len = (st.end_sec - st.start_sec).max(0.0);
                info_ref.setStringValue(&NSString::from_str(&format!(
                    "{}×{}  ·  Clip {:.2}s – {:.2}s ({:.2}s)",
                    st.meta.width, st.meta.height, st.start_sec, st.end_sec, clip_len
                )));
                if let Ok(frame) = extract_rgba_frame(&st.input_path, preview_time, Some(960)) {
                    if let Some(img) = rgba_to_nsimage(&frame) {
                        iv_ref.setImage(Some(&img));
                    }
                }
            })
        };

        let st_s = Rc::clone(&state);
        let sl_s = start_slider.clone();
        let upd_s = Rc::clone(&update_preview);
        let st_e = Rc::clone(&state);
        let sl_e = end_slider.clone();
        let upd_e = Rc::clone(&update_preview);
        let st_f = Rc::clone(&state);
        let fps_pop_f = fps_popup.clone();
        let st_fps = Rc::clone(&state);
        let st_sz = Rc::clone(&state);
        let st_copy = Rc::clone(&state);
        let status_copy = status_label.clone();
        let copy_c = copy_btn.clone();
        let save_c = save_btn.clone();
        let st_save = Rc::clone(&state);
        let status_save = status_label.clone();
        let copy_s = copy_btn.clone();
        let save_s = save_btn.clone();

        let actions: Retained<EditorActions> = {
            let ivars = EditorActionIvars {
                on_start_scrub: Box::new(move |v| {
                    let mut st = st_s.borrow_mut();
                    st.start_sec = v.clamp(0.0, (st.end_sec - 0.05).max(0.0));
                    let cur = st.start_sec;
                    if (v - cur).abs() > 0.001 {
                        sl_s.setDoubleValue(cur);
                    }
                    drop(st);
                    upd_s(cur);
                }),
                on_end_scrub: Box::new(move |v| {
                    let mut st = st_e.borrow_mut();
                    st.end_sec = v.clamp(
                        (st.start_sec + 0.05).min(st.meta.duration_sec),
                        st.meta.duration_sec,
                    );
                    let cur = st.end_sec;
                    if (v - cur).abs() > 0.001 {
                        sl_e.setDoubleValue(cur);
                    }
                    drop(st);
                    upd_e(cur);
                }),
                on_format_change: Box::new(move |idx| {
                    let fmt = ExportFormat::ALL
                        .get(idx.max(0) as usize)
                        .copied()
                        .unwrap_or(ExportFormat::Mp4);
                    let mut st = st_f.borrow_mut();
                    st.format = fmt;
                    if matches!(fmt, ExportFormat::Gif | ExportFormat::Apng) && st.fps > 30 {
                        st.fps = 30;
                        fps_pop_f.selectItemAtIndex(1);
                    }
                }),
                on_fps_change: Box::new(move |idx| {
                    st_fps.borrow_mut().fps =
                        FPS_PRESETS.get(idx.max(0) as usize).copied().unwrap_or(30);
                }),
                on_size_change: Box::new(move |idx| {
                    st_sz.borrow_mut().max_width =
                        SIZE_PRESETS.get(idx.max(0) as usize).and_then(|(_, w)| *w);
                }),
                on_copy: Box::new(move || {
                    let req = {
                        let mut st = st_copy.borrow_mut();
                        if st.busy {
                            return;
                        }
                        let Ok(out) = crate::fs_safety::unique_private_file_path(
                            "ClipboardRecording",
                            st.format.extension(),
                        ) else {
                            status_copy.setTextColor(Some(&NSColor::systemRedColor()));
                            status_copy.setStringValue(&NSString::from_str(
                                "Failed creating private cache file",
                            ));
                            return;
                        };
                        st.busy = true;
                        ExportRequest {
                            input: st.input_path.clone(),
                            output: out,
                            format: st.format,
                            fps: st.fps,
                            start_sec: Some(st.start_sec),
                            end_sec: Some(st.end_sec),
                            max_width: st.max_width,
                        }
                    };
                    spawn_async_export(
                        req,
                        true,
                        Rc::clone(&st_copy),
                        status_copy.clone(),
                        copy_c.clone(),
                        save_c.clone(),
                    );
                }),
                on_save: Box::new(move || {
                    let Some(m) = MainThreadMarker::new() else {
                        return;
                    };
                    let (fmt, busy) = {
                        let st = st_save.borrow();
                        (st.format, st.busy)
                    };
                    if busy {
                        return;
                    }
                    let panel = NSSavePanel::savePanel(m);
                    panel.setNameFieldStringValue(&NSString::from_str(&format!(
                        "Recording-{}.{}",
                        crate::observe::now_unix_secs(),
                        fmt.extension()
                    )));
                    if panel.runModal() != NSModalResponseOK {
                        return;
                    }
                    let Some(out_path) = panel
                        .URL()
                        .and_then(|u| u.path())
                        .map(|p| PathBuf::from(p.to_string()))
                    else {
                        return;
                    };
                    let req = {
                        let mut st = st_save.borrow_mut();
                        st.busy = true;
                        ExportRequest {
                            input: st.input_path.clone(),
                            output: out_path,
                            format: st.format,
                            fps: st.fps,
                            start_sec: Some(st.start_sec),
                            end_sec: Some(st.end_sec),
                            max_width: st.max_width,
                        }
                    };
                    spawn_async_export(
                        req,
                        false,
                        Rc::clone(&st_save),
                        status_save.clone(),
                        copy_s.clone(),
                        save_s.clone(),
                    );
                }),
            };
            let this = EditorActions::alloc(mtm).set_ivars(ivars);
            unsafe { msg_send![super(this), init] }
        };

        unsafe {
            start_slider.setTarget(Some(&actions));
            start_slider.setAction(Some(sel!(startScrubbed:)));
            end_slider.setTarget(Some(&actions));
            end_slider.setAction(Some(sel!(endScrubbed:)));
            format_popup.setTarget(Some(&actions));
            format_popup.setAction(Some(sel!(formatChanged:)));
            fps_popup.setTarget(Some(&actions));
            fps_popup.setAction(Some(sel!(fpsChanged:)));
            size_popup.setTarget(Some(&actions));
            size_popup.setAction(Some(sel!(sizeChanged:)));
            copy_btn.setTarget(Some(&actions));
            copy_btn.setAction(Some(sel!(copyClicked:)));
            save_btn.setTarget(Some(&actions));
            save_btn.setAction(Some(sel!(saveClicked:)));
        }

        window.makeKeyAndOrderFront(None);
        Ok(Self {
            window,
            _actions: actions,
        })
    }
}
