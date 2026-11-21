use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSBackingStoreType, NSBox,
    NSBoxType, NSButton, NSColor, NSFont, NSPopUpButton, NSScreen, NSTextAlignment, NSTextField,
    NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use crate::config::Preferences;
use crate::export::avfoundation::{extract_rgba_frame, probe_video_metadata};
use crate::export::{ExportFormat, ExportRequest};
use crate::fs_safety::{
    format_tilde_path, is_usable_export_dir, next_recording_output_path, unique_private_file_path,
};
use crate::ui::clipboard::rgba_to_nsimage;
use crate::ui::editor_worker::{
    EditorState, ExportProgressView, FPS_PRESETS, ProgressVisualState, SIZE_PRESETS,
    build_export_popups, build_preview_stage, spawn_async_export,
};
use crate::ui::folder_picker::{
    FolderMenuItem, handle_editor_folder_popup_selection, populate_folder_popup,
};
use crate::ui::trim_view::TrimTimelineView;

struct EditorActionIvars {
    on_format_change: Box<dyn Fn(isize)>,
    on_fps_change: Box<dyn Fn(isize)>,
    on_size_change: Box<dyn Fn(isize)>,
    on_folder_change: Box<dyn Fn(isize)>,
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
        #[unsafe(method(folderChanged:))]
        fn folder_changed(&self, sender: &NSPopUpButton) {
            (self.ivars().on_folder_change)(sender.indexOfSelectedItem());
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
    state: Rc<RefCell<EditorState>>,
    folder_popup: Retained<NSPopUpButton>,
    folder_items: Rc<RefCell<Vec<FolderMenuItem>>>,
    status_label: Retained<NSTextField>,
    demo_showcase: bool,
    _actions: Retained<EditorActions>,
}

impl EditorWindow {
    pub fn open(
        mtm: MainThreadMarker,
        input_path: &Path,
        prefs: &Preferences,
        demo_showcase: bool,
        on_export_dir_change: Rc<dyn Fn(PathBuf)>,
    ) -> Result<Self, String> {
        let meta = probe_video_metadata(input_path)?;
        let (win_w, win_h) = (760.0, 568.0);
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
        window.setAcceptsMouseMovedEvents(true);
        window.setTitle(&NSString::from_str("Quick Screen Recorder — Trim & Export"));
        window.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            0.09, 0.10, 0.14, 1.0,
        )));

        let root = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(win_w, win_h)),
        );
        window.setContentView(Some(&root));

        let (init_start, init_end, init_playhead) = if demo_showcase && meta.duration_sec >= 0.6 {
            let s = (meta.duration_sec * 0.15).min(0.30);
            let e = (meta.duration_sec * 0.85).max(s + 0.15);
            (s, e, (s + e) * 0.5)
        } else {
            (0.0, meta.duration_sec, 0.0)
        };

        let image_view = build_preview_stage(mtm, &root, win_w, win_h, input_path, init_playhead);

        let divider = NSBox::initWithFrame(
            NSBox::alloc(mtm),
            NSRect::new(NSPoint::new(20.0, 72.0), NSSize::new(win_w - 40.0, 1.0)),
        );
        divider.setBoxType(NSBoxType::Custom);
        divider.setBorderWidth(0.0);
        divider.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
            1.0, 1.0, 1.0, 0.08,
        ));
        root.addSubview(&divider);

        let popups = build_export_popups(mtm, &root, prefs, demo_showcase);
        let format_popup = popups.format_popup;
        let fps_popup = popups.fps_popup;
        let size_popup = popups.size_popup;
        let initial_fmt = popups.initial_fmt;
        let initial_fps = popups.initial_fps;

        let save_to_lbl = NSTextField::labelWithString(&NSString::from_str("Save to"), mtm);
        save_to_lbl.setFont(Some(&NSFont::systemFontOfSize_weight(12.0, 0.2)));
        save_to_lbl.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            0.74, 0.79, 0.89, 1.0,
        )));
        save_to_lbl.setAlignment(NSTextAlignment::Right);
        save_to_lbl.setFrame(NSRect::new(
            NSPoint::new(420.0, 92.0),
            NSSize::new(54.0, 20.0),
        ));
        root.addSubview(&save_to_lbl);

        let initial_export_dir = prefs.resolved_export_dir();
        let folder_popup = NSPopUpButton::new(mtm);
        folder_popup.setFrame(NSRect::new(
            NSPoint::new(480.0, 88.0),
            NSSize::new(260.0, 28.0),
        ));
        let init_items = populate_folder_popup(
            mtm,
            &folder_popup,
            initial_export_dir.as_deref(),
            demo_showcase,
        );
        let folder_items = Rc::new(RefCell::new(init_items));
        root.addSubview(&folder_popup);

        let ready_text = match &initial_export_dir {
            Some(dir) => format!("Ready · Saves to {}", format_tilde_path(dir)),
            None => "Choose an export folder from Save to before saving".to_string(),
        };
        let status_label = NSTextField::labelWithString(&NSString::from_str(&ready_text), mtm);
        status_label.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(
            12.0, 0.15,
        )));
        status_label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            0.68, 0.74, 0.84, 1.0,
        )));
        status_label.setFrame(NSRect::new(
            NSPoint::new(22.0, 36.0),
            NSSize::new(498.0, 20.0),
        ));
        root.addSubview(&status_label);

        let progress_view = ExportProgressView::new(
            mtm,
            NSRect::new(NSPoint::new(22.0, 18.0), NSSize::new(498.0, 10.0)),
        );
        if demo_showcase {
            progress_view.set_progress(680, ProgressVisualState::Running);
            status_label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
                0.58, 0.78, 1.0, 1.0,
            )));
            status_label.setStringValue(&NSString::from_str("Exporting GIF...  68%"));
        }
        root.addSubview(&progress_view);

        let copy_btn = unsafe {
            NSButton::buttonWithTitle_target_action(&NSString::from_str("Copy"), None, None, mtm)
        };
        copy_btn.setFrame(NSRect::new(
            NSPoint::new(536.0, 20.0),
            NSSize::new(96.0, 34.0),
        ));
        root.addSubview(&copy_btn);

        let save_btn = unsafe {
            NSButton::buttonWithTitle_target_action(&NSString::from_str("Save"), None, None, mtm)
        };
        save_btn.setBezelColor(Some(&NSColor::systemBlueColor()));
        save_btn.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        save_btn.setKeyEquivalent(&NSString::from_str("\r"));
        save_btn.setFrame(NSRect::new(
            NSPoint::new(640.0, 20.0),
            NSSize::new(100.0, 34.0),
        ));
        root.addSubview(&save_btn);

        let state = Rc::new(RefCell::new(EditorState {
            input_path: input_path.to_path_buf(),
            meta,
            start_sec: init_start,
            end_sec: init_end,
            format: initial_fmt,
            fps: initial_fps,
            max_width: popups.initial_max_width,
            export_dir: initial_export_dir,
            busy: false,
        }));

        let st_trim = Rc::clone(&state);
        let iv_trim = image_view.clone();
        let trim_view = TrimTimelineView::new(
            mtm,
            NSRect::new(NSPoint::new(20.0, 134.0), NSSize::new(win_w - 40.0, 58.0)),
            meta.duration_sec,
            meta.width,
            meta.height,
            Rc::new(move |start_sec, end_sec, preview_sec| {
                let path = {
                    let mut st = st_trim.borrow_mut();
                    st.start_sec = start_sec;
                    st.end_sec = end_sec;
                    st.input_path.clone()
                };
                if let Ok(frame) = extract_rgba_frame(&path, preview_sec, Some(960)) {
                    if let Some(img) = rgba_to_nsimage(&frame) {
                        iv_trim.setImage(Some(&img));
                    }
                }
            }),
        );
        trim_view.set_trim_range(init_start, init_end, init_playhead);
        root.addSubview(&trim_view);

        let st_f = Rc::clone(&state);
        let fps_pop_f = fps_popup.clone();
        let st_fps = Rc::clone(&state);
        let st_sz = Rc::clone(&state);

        let st_dir = Rc::clone(&state);
        let fld_pop_dir = folder_popup.clone();
        let fld_items_dir = Rc::clone(&folder_items);
        let status_dir = status_label.clone();

        let st_copy = Rc::clone(&state);
        let status_copy = status_label.clone();
        let prog_copy = progress_view.clone();
        let copy_c = copy_btn.clone();
        let save_c = save_btn.clone();
        let folder_c = folder_popup.clone();

        let st_save = Rc::clone(&state);
        let status_save = status_label.clone();
        let prog_save = progress_view.clone();
        let copy_s = copy_btn.clone();
        let save_s = save_btn.clone();
        let folder_s = folder_popup.clone();

        let actions: Retained<EditorActions> = {
            let ivars = EditorActionIvars {
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
                on_folder_change: Box::new(move |idx| {
                    if let Some(m) = MainThreadMarker::new() {
                        handle_editor_folder_popup_selection(
                            m,
                            idx,
                            &st_dir,
                            &fld_pop_dir,
                            &fld_items_dir,
                            &status_dir,
                            demo_showcase,
                            on_export_dir_change.as_ref(),
                        );
                    }
                }),
                on_copy: Box::new(move || {
                    let req = {
                        let mut st = st_copy.borrow_mut();
                        if st.busy {
                            return;
                        }
                        let Ok(out) =
                            unique_private_file_path("ClipboardRecording", st.format.extension())
                        else {
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
                        prog_copy.clone(),
                        copy_c.clone(),
                        save_c.clone(),
                        folder_c.clone(),
                    );
                }),
                on_save: Box::new(move || {
                    let req = {
                        let mut st = st_save.borrow_mut();
                        if st.busy {
                            return;
                        }
                        let Some(export_dir) = st.export_dir.clone() else {
                            status_save.setTextColor(Some(&NSColor::systemRedColor()));
                            status_save.setStringValue(&NSString::from_str(
                                "No persistent export folder set; select one from Save to",
                            ));
                            return;
                        };
                        if !is_usable_export_dir(&export_dir) {
                            status_save.setTextColor(Some(&NSColor::systemRedColor()));
                            status_save.setStringValue(&NSString::from_str(
                                "Export folder is unavailable or unsafe; choose another from Save to",
                            ));
                            return;
                        }
                        let out_path =
                            next_recording_output_path(&export_dir, st.format.extension());
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
                        prog_save.clone(),
                        copy_s.clone(),
                        save_s.clone(),
                        folder_s.clone(),
                    );
                }),
            };
            let this = EditorActions::alloc(mtm).set_ivars(ivars);
            unsafe { msg_send![super(this), init] }
        };

        unsafe {
            format_popup.setTarget(Some(&actions));
            format_popup.setAction(Some(sel!(formatChanged:)));
            fps_popup.setTarget(Some(&actions));
            fps_popup.setAction(Some(sel!(fpsChanged:)));
            size_popup.setTarget(Some(&actions));
            size_popup.setAction(Some(sel!(sizeChanged:)));
            folder_popup.setTarget(Some(&actions));
            folder_popup.setAction(Some(sel!(folderChanged:)));
            copy_btn.setTarget(Some(&actions));
            copy_btn.setAction(Some(sel!(copyClicked:)));
            save_btn.setTarget(Some(&actions));
            save_btn.setAction(Some(sel!(saveClicked:)));
        }

        window.makeKeyAndOrderFront(None);
        Ok(Self {
            window,
            state,
            folder_popup,
            folder_items,
            status_label,
            demo_showcase,
            _actions: actions,
        })
    }

    pub fn set_export_dir(&self, mtm: MainThreadMarker, dir: PathBuf) {
        let is_busy = {
            let mut st = self.state.borrow_mut();
            st.export_dir = Some(dir.clone());
            st.busy
        };
        let refreshed =
            populate_folder_popup(mtm, &self.folder_popup, Some(&dir), self.demo_showcase);
        *self.folder_items.borrow_mut() = refreshed;
        if !is_busy {
            self.status_label
                .setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
                    0.68, 0.74, 0.84, 1.0,
                )));
            self.status_label
                .setStringValue(&NSString::from_str(&format!(
                    "Ready · Saves to {}",
                    format_tilde_path(&dir)
                )));
        }
    }
}
