use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSBackingStoreType,
    NSButton, NSColor, NSControlStateValueOn, NSPopUpButton, NSScreen, NSTextField, NSView,
    NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use crate::config::Preferences;
use crate::export::avfoundation::{extract_rgba_frame, probe_video_metadata};
use crate::export::{ExportFormat, ExportRequest};
use crate::export_name::{build_export_output_path, default_export_stem_now};
use crate::fs_safety::{format_tilde_path, is_usable_export_dir, unique_private_clipboard_dir};
use crate::ui::clipboard::rgba_to_nsimage;
use crate::ui::editor_controls::build_editor_controls;
use crate::ui::editor_worker::{
    EditorState, ExportUiControls, FPS_PRESETS, SIZE_PRESETS, spawn_async_export,
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
    on_mute_change: Box<dyn Fn(bool)>,
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
        #[unsafe(method(muteToggled:))]
        fn mute_toggled(&self, sender: &NSButton) {
            (self.ivars().on_mute_change)(sender.state() == NSControlStateValueOn);
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

pub struct EditorOpenOptions {
    pub captured_app_name: Option<String>,
    pub demo_showcase: bool,
    pub tutorial_hint: bool,
    pub initial_notice: Option<String>,
    pub on_export_dir_change: Rc<dyn Fn(PathBuf)>,
    pub on_export_saved: Rc<dyn Fn(PathBuf)>,
}

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
        opts: EditorOpenOptions,
    ) -> Result<Self, String> {
        let demo_showcase = opts.demo_showcase;
        let tutorial_hint = opts.tutorial_hint;
        let initial_notice = opts.initial_notice;
        let on_export_dir_change = opts.on_export_dir_change;
        let on_export_saved = opts.on_export_saved;
        let default_stem = if demo_showcase {
            "2026-11-24_09-30-00_Quick-Screen-Recorder".to_string()
        } else {
            default_export_stem_now(opts.captured_app_name.as_deref())
        };

        let meta = probe_video_metadata(input_path)?;
        let (win_w, win_h) = (760.0, 610.0);
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

        let initial_export_dir = prefs.resolved_export_dir();
        let controls = build_editor_controls(
            mtm,
            &root,
            win_w,
            win_h,
            input_path,
            init_playhead,
            prefs,
            initial_export_dir.as_deref(),
            &default_stem,
            meta.has_audio,
            meta.nominal_fps,
            demo_showcase,
            tutorial_hint,
        );
        let folder_items = Rc::new(RefCell::new(controls.folder_items));
        if let Some(notice) = initial_notice {
            controls
                .status_label
                .setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
                    0.96, 0.78, 0.36, 1.0,
                )));
            controls
                .status_label
                .setStringValue(&NSString::from_str(&notice));
        }

        let state = Rc::new(RefCell::new(EditorState {
            input_path: input_path.to_path_buf(),
            meta,
            start_sec: init_start,
            end_sec: init_end,
            format: controls.initial_fmt,
            fps: controls.initial_fps,
            max_width: controls.initial_max_width,
            export_dir: initial_export_dir,
            default_stem,
            mute_audio: false,
            busy: false,
        }));

        let st_trim = Rc::clone(&state);
        let iv_trim = controls.image_view.clone();
        let trim_view = TrimTimelineView::new(
            mtm,
            NSRect::new(NSPoint::new(20.0, 176.0), NSSize::new(win_w - 40.0, 58.0)),
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
        let fps_pop_f = controls.fps_popup.clone();
        let ext_lbl_f = controls.ext_label.clone();
        let mute_chk_f = controls.mute_checkbox.clone();
        let st_fps = Rc::clone(&state);
        let st_sz = Rc::clone(&state);
        let st_mute = Rc::clone(&state);

        let st_dir = Rc::clone(&state);
        let fld_pop_dir = controls.folder_popup.clone();
        let fld_items_dir = Rc::clone(&folder_items);
        let status_dir = controls.status_label.clone();

        let st_copy = Rc::clone(&state);
        let status_copy = controls.status_label.clone();
        let prog_copy = controls.progress_view.clone();
        let copy_c = controls.copy_btn.clone();
        let save_c = controls.save_btn.clone();
        let folder_c = controls.folder_popup.clone();
        let name_c = controls.name_field.clone();
        let mute_c = controls.mute_checkbox.clone();
        let win_c = window.clone();

        let st_save = Rc::clone(&state);
        let status_save = controls.status_label.clone();
        let prog_save = controls.progress_view.clone();
        let copy_s = controls.copy_btn.clone();
        let save_s = controls.save_btn.clone();
        let folder_s = controls.folder_popup.clone();
        let name_s = controls.name_field.clone();
        let mute_s = controls.mute_checkbox.clone();
        let win_s = window.clone();
        let saved_cb_copy: Rc<dyn Fn(PathBuf)> = Rc::new(|_: PathBuf| {});
        let saved_cb_save = Rc::clone(&on_export_saved);

        let actions: Retained<EditorActions> = {
            let ivars = EditorActionIvars {
                on_format_change: Box::new(move |idx| {
                    let fmt = ExportFormat::ALL
                        .get(idx.max(0) as usize)
                        .copied()
                        .unwrap_or(ExportFormat::Mp4);
                    let mut st = st_f.borrow_mut();
                    st.format = fmt;
                    ext_lbl_f.setStringValue(&NSString::from_str(&format!(".{}", fmt.extension())));
                    if matches!(fmt, ExportFormat::Gif | ExportFormat::Apng) && st.fps > 30 {
                        st.fps = 30;
                        if let Some(idx30) = FPS_PRESETS.iter().position(|&f| f == 30) {
                            fps_pop_f.selectItemAtIndex(idx30 as isize);
                        }
                    }
                    let can_mute =
                        st.meta.has_audio && !matches!(fmt, ExportFormat::Gif | ExportFormat::Apng);
                    mute_chk_f.setEnabled(!st.busy && can_mute);
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
                on_mute_change: Box::new(move |muted| {
                    st_mute.borrow_mut().mute_audio = muted;
                }),
                on_copy: Box::new(move || {
                    let _ = win_c.makeFirstResponder(None);
                    let raw_name = name_c.stringValue().to_string();
                    let req = {
                        let mut st = st_copy.borrow_mut();
                        if st.busy {
                            return;
                        }
                        let Ok(clip_dir) = unique_private_clipboard_dir() else {
                            status_copy.setTextColor(Some(&NSColor::systemRedColor()));
                            status_copy.setStringValue(&NSString::from_str(
                                "Failed creating private cache directory",
                            ));
                            return;
                        };
                        let out = build_export_output_path(
                            &clip_dir,
                            &raw_name,
                            st.format.extension(),
                            &st.default_stem,
                        );
                        st.busy = true;
                        ExportRequest {
                            input: st.input_path.clone(),
                            output: out,
                            format: st.format,
                            fps: st.fps,
                            start_sec: Some(st.start_sec),
                            end_sec: Some(st.end_sec),
                            max_width: st.max_width,
                            mute_audio: st.mute_audio,
                        }
                    };
                    spawn_async_export(
                        req,
                        true,
                        Rc::clone(&st_copy),
                        ExportUiControls {
                            status_label: status_copy.clone(),
                            progress_view: prog_copy.clone(),
                            copy_btn: copy_c.clone(),
                            save_btn: save_c.clone(),
                            folder_popup: folder_c.clone(),
                            name_field: name_c.clone(),
                            mute_checkbox: mute_c.clone(),
                        },
                        Rc::clone(&saved_cb_copy),
                    );
                }),
                on_save: Box::new(move || {
                    let _ = win_s.makeFirstResponder(None);
                    let raw_name = name_s.stringValue().to_string();
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
                        let out_path = build_export_output_path(
                            &export_dir,
                            &raw_name,
                            st.format.extension(),
                            &st.default_stem,
                        );
                        st.busy = true;
                        ExportRequest {
                            input: st.input_path.clone(),
                            output: out_path,
                            format: st.format,
                            fps: st.fps,
                            start_sec: Some(st.start_sec),
                            end_sec: Some(st.end_sec),
                            max_width: st.max_width,
                            mute_audio: st.mute_audio,
                        }
                    };
                    spawn_async_export(
                        req,
                        false,
                        Rc::clone(&st_save),
                        ExportUiControls {
                            status_label: status_save.clone(),
                            progress_view: prog_save.clone(),
                            copy_btn: copy_s.clone(),
                            save_btn: save_s.clone(),
                            folder_popup: folder_s.clone(),
                            name_field: name_s.clone(),
                            mute_checkbox: mute_s.clone(),
                        },
                        Rc::clone(&saved_cb_save),
                    );
                }),
            };
            let this = EditorActions::alloc(mtm).set_ivars(ivars);
            unsafe { msg_send![super(this), init] }
        };

        unsafe {
            controls.format_popup.setTarget(Some(&actions));
            controls.format_popup.setAction(Some(sel!(formatChanged:)));
            controls.fps_popup.setTarget(Some(&actions));
            controls.fps_popup.setAction(Some(sel!(fpsChanged:)));
            controls.size_popup.setTarget(Some(&actions));
            controls.size_popup.setAction(Some(sel!(sizeChanged:)));
            controls.folder_popup.setTarget(Some(&actions));
            controls.folder_popup.setAction(Some(sel!(folderChanged:)));
            controls.mute_checkbox.setTarget(Some(&actions));
            controls.mute_checkbox.setAction(Some(sel!(muteToggled:)));
            controls.copy_btn.setTarget(Some(&actions));
            controls.copy_btn.setAction(Some(sel!(copyClicked:)));
            controls.save_btn.setTarget(Some(&actions));
            controls.save_btn.setAction(Some(sel!(saveClicked:)));
        }

        window.makeKeyAndOrderFront(None);
        Ok(Self {
            window,
            state,
            folder_popup: controls.folder_popup,
            folder_items,
            status_label: controls.status_label,
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
