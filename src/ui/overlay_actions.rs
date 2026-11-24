use std::cell::RefCell;
use std::rc::Rc;

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSPopUpButton, NSScreen, NSTextField, NSView, NSWindow};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use crate::capture::{AudioCaptureMode, CaptureConfig, CaptureTarget};
use crate::config::Preferences;
use crate::geometry::{AspectRatio, Rect, apply_aspect_to_rect, compute_toolbar_origin};
use crate::ui::overlay::{
    OverlayState, SelectionMode, StartCaptureCallback, extract_nsscreen_display_id, nsrect_to_rect,
};
use crate::ui::overlay_toolbar::{
    OverlayToolbarActions, TOOLBAR_H, TOOLBAR_W, ToolbarActionIvars, append_tagged_popup_item,
};

#[allow(clippy::too_many_arguments)]
pub fn build_toolbar_actions(
    mtm: MainThreadMarker,
    state: &Rc<RefCell<OverlayState>>,
    canvas_window: &Retained<NSWindow>,
    toolbar_window: &Retained<NSWindow>,
    canvas_view: &Retained<NSView>,
    aspect_popup: &Retained<NSPopUpButton>,
    w_field: &Retained<NSTextField>,
    x_lbl: &Retained<NSTextField>,
    h_field: &Retained<NSTextField>,
    target_popup: &Retained<NSPopUpButton>,
    sync_fields: &Rc<dyn Fn()>,
    prefs: &Preferences,
    on_start: StartCaptureCallback,
    on_cancel: Rc<dyn Fn()>,
) -> Retained<OverlayToolbarActions> {
    let cwin_num = canvas_window.windowNumber() as u32;
    let twin_num = toolbar_window.windowNumber() as u32;

    let st_mode = Rc::clone(state);
    let cv_mode = canvas_view.clone();
    let cwin_mode = canvas_window.clone();
    let twin_pick_mode = toolbar_window.clone();
    let ap_mode = aspect_popup.clone();
    let wf_mode = w_field.clone();
    let xl_mode = x_lbl.clone();
    let hf_mode = h_field.clone();
    let tp_mode = target_popup.clone();
    let sf_mode = Rc::clone(sync_fields);

    let st_aspect = Rc::clone(state);
    let cv_aspect = canvas_view.clone();
    let sf_aspect = Rc::clone(sync_fields);

    let st_size = Rc::clone(state);
    let cv_size = canvas_view.clone();
    let wf_size = w_field.clone();
    let hf_size = h_field.clone();
    let sf_size = Rc::clone(sync_fields);

    let st_pick = Rc::clone(state);
    let cv_pick = canvas_view.clone();
    let cwin_pick = canvas_window.clone();
    let twin_pick = toolbar_window.clone();
    let sf_pick = Rc::clone(sync_fields);

    let st_fps = Rc::clone(state);
    let st_sys_audio = Rc::clone(state);
    let st_mic = Rc::clone(state);
    let st_cursor = Rc::clone(state);
    let st_clicks = Rc::clone(state);
    let st_rec = Rc::clone(state);
    let base_prefs = prefs.clone();

    OverlayToolbarActions::new(
        mtm,
        ToolbarActionIvars {
            on_mode: Box::new(move |seg| {
                let mut st = st_mode.borrow_mut();
                st.mode = match seg {
                    1 => SelectionMode::Window,
                    2 => SelectionMode::Screen,
                    _ => SelectionMode::Area,
                };
                let is_area = st.mode == SelectionMode::Area;
                let is_demo = st.demo_backdrop;
                st.hovered_window_idx = None;
                ap_mode.setHidden(!is_area);
                wf_mode.setHidden(!is_area);
                xl_mode.setHidden(!is_area);
                hf_mode.setHidden(!is_area);
                tp_mode.setHidden(is_area);
                tp_mode.removeAllItems();
                match st.mode {
                    SelectionMode::Area => {
                        st.selection = st.selection.clamp_and_align_even(st.bounds);
                    }
                    SelectionMode::Window => {
                        if st.windows.is_empty() {
                            append_tagged_popup_item(&tp_mode, "No windows available", -1);
                        } else {
                            for (i, w) in st.windows.iter().enumerate() {
                                append_tagged_popup_item(&tp_mode, &w.display_label(), i as isize);
                            }
                            let idx = st.selected_window_idx.min(st.windows.len() - 1);
                            tp_mode.selectItemAtIndex(idx as isize);
                            if st.window_overlay_rect(idx).is_none() && !st.demo_backdrop {
                                if let (Some(did), Some(m)) =
                                    (st.display_for_window(idx), MainThreadMarker::new())
                                {
                                    relocate_overlay_to_display(
                                        m,
                                        did,
                                        &mut st,
                                        &cwin_mode,
                                        &twin_pick_mode,
                                    );
                                }
                            }
                            st.select_window(idx);
                        }
                    }
                    SelectionMode::Screen => {
                        for (i, d) in st.displays.iter().enumerate() {
                            append_tagged_popup_item(
                                &tp_mode,
                                &format!("Display {} ({}×{})", i + 1, d.width, d.height),
                                i as isize,
                            );
                        }
                        if st.screen_display_id != st.overlay_display_id && !st.demo_backdrop {
                            let target_did = st.screen_display_id;
                            if let Some(m) = MainThreadMarker::new() {
                                relocate_overlay_to_display(
                                    m,
                                    target_did,
                                    &mut st,
                                    &cwin_mode,
                                    &twin_pick_mode,
                                );
                            }
                        }
                        tp_mode.selectItemAtIndex(st.selected_display_idx as isize);
                        st.selection = st.bounds;
                    }
                }
                drop(st);
                sf_mode();
                cv_mode.setNeedsDisplay(true);
                if !is_demo {
                    cwin_mode.makeKeyWindow();
                }
            }),
            on_aspect: Box::new(move |idx| {
                let mut st = st_aspect.borrow_mut();
                st.aspect = AspectRatio::from_index(idx.max(0) as usize);
                st.selection = apply_aspect_to_rect(st.selection, st.aspect, st.bounds);
                drop(st);
                sf_aspect();
                cv_aspect.setNeedsDisplay(true);
            }),
            on_size_commit: Box::new(move || {
                let wt = wf_size.stringValue().to_string();
                let ht = hf_size.stringValue().to_string();
                if let (Ok(px_w), Ok(px_h)) = (wt.trim().parse::<f64>(), ht.trim().parse::<f64>()) {
                    let mut st = st_size.borrow_mut();
                    let sf = st.scale_factor.max(1.0);
                    st.selection = Rect::new(st.selection.x, st.selection.y, px_w / sf, px_h / sf)
                        .clamp_and_align_for_scale(st.bounds, sf);
                }
                sf_size();
                cv_size.setNeedsDisplay(true);
            }),
            on_target_pick: Box::new(move |idx| {
                if idx < 0 {
                    return;
                }
                let mut st = st_pick.borrow_mut();
                let is_demo = st.demo_backdrop;
                let u = idx as usize;
                match st.mode {
                    SelectionMode::Window => {
                        if st.window_overlay_rect(u).is_none() && !st.demo_backdrop {
                            if let (Some(did), Some(m)) =
                                (st.display_for_window(u), MainThreadMarker::new())
                            {
                                relocate_overlay_to_display(
                                    m, did, &mut st, &cwin_pick, &twin_pick,
                                );
                            }
                        }
                        st.select_window(u);
                    }
                    SelectionMode::Screen => {
                        if let Some(did) = st.displays.get(u).map(|d| d.display_id) {
                            st.selected_display_idx = u;
                            st.screen_display_id = did;
                            if !st.demo_backdrop {
                                if let Some(m) = MainThreadMarker::new() {
                                    relocate_overlay_to_display(
                                        m, did, &mut st, &cwin_pick, &twin_pick,
                                    );
                                }
                            }
                        }
                    }
                    SelectionMode::Area => {}
                }
                drop(st);
                sf_pick();
                cv_pick.setNeedsDisplay(true);
                if !is_demo {
                    cwin_pick.makeKeyWindow();
                }
            }),
            on_fps: Box::new(move |idx| {
                st_fps.borrow_mut().fps = if idx == 0 { 30 } else { 60 };
            }),
            on_sys_audio: Box::new(move |on| {
                st_sys_audio.borrow_mut().capture_system_audio = on;
            }),
            on_mic: Box::new(move |idx| {
                if idx < 0 {
                    return;
                }
                let mut st = st_mic.borrow_mut();
                if let Some(item) = st.mic_items.get(idx as usize) {
                    st.mic_input = item.to_preference();
                }
            }),
            on_cursor: Box::new(move |on| {
                st_cursor.borrow_mut().show_cursor = on;
            }),
            on_clicks: Box::new(move |on| {
                st_clicks.borrow_mut().highlight_clicks = on;
            }),
            on_record: Box::new(move || {
                let st = st_rec.borrow();
                let (target, source_app_name) = match st.mode {
                    SelectionMode::Area => (
                        CaptureTarget::Region {
                            // Always record on the exact display where the selection rectangle is drawn
                            display_id: st.overlay_display_id,
                            rect_points: st.selection.appkit_to_top_left(st.bounds.height),
                            scale_factor: st.scale_factor,
                        },
                        None,
                    ),
                    SelectionMode::Window => {
                        let Some(win) = st.windows.get(st.selected_window_idx) else {
                            let err = "No shareable window available to record".to_string();
                            drop(st);
                            on_start(Err(err));
                            return;
                        };
                        (
                            CaptureTarget::Window {
                                window_id: win.window_id,
                            },
                            Some(win.app_name.clone()),
                        )
                    }
                    SelectionMode::Screen => (
                        CaptureTarget::Display {
                            display_id: st.screen_display_id,
                            scale_factor: st.scale_factor,
                        },
                        None,
                    ),
                };
                let audio_mode = AudioCaptureMode::from_split(
                    st.capture_system_audio,
                    st.mic_input.is_enabled(),
                );
                let microphone_device_id = st.mic_input.explicit_device_id().map(ToOwned::to_owned);
                let mut updated = base_prefs.clone();
                updated.fps = st.fps;
                updated.show_cursor = st.show_cursor;
                updated.highlight_clicks = st.highlight_clicks;
                updated.capture_system_audio = st.capture_system_audio;
                updated.mic_input = st.mic_input.clone();
                updated.audio_mode = audio_mode;
                updated.aspect_ratio = st.aspect;
                if st.mode == SelectionMode::Area {
                    updated.last_region = st.selection;
                }
                let cfg = CaptureConfig {
                    target,
                    fps: st.fps,
                    codec: updated.recording_codec,
                    show_cursor: st.show_cursor,
                    highlight_clicks: st.highlight_clicks,
                    audio_mode,
                    microphone_device_id,
                    excluded_window_ids: vec![cwin_num, twin_num],
                    source_app_name,
                };
                drop(st);
                on_start(Ok((cfg, updated)));
            }),
            on_cancel: Box::new(move || {
                on_cancel();
            }),
        },
    )
}

fn relocate_overlay_to_display(
    mtm: MainThreadMarker,
    target_display_id: u32,
    st: &mut OverlayState,
    canvas_window: &NSWindow,
    toolbar_window: &NSWindow,
) {
    let screens = NSScreen::screens(mtm);
    for i in 0..screens.count() {
        let s = screens.objectAtIndex(i);
        if extract_nsscreen_display_id(&s) == Some(target_display_id) {
            let sf = s.frame();
            let vf = s.visibleFrame();
            st.overlay_display_id = target_display_id;
            st.screen_display_id = target_display_id;
            if let Some(pos) = st
                .displays
                .iter()
                .position(|d| d.display_id == target_display_id)
            {
                st.selected_display_idx = pos;
            }
            st.scale_factor = s.backingScaleFactor();
            st.screen_frame = nsrect_to_rect(sf);
            st.visible_frame = nsrect_to_rect(vf);
            st.bounds = Rect::new(0.0, 0.0, sf.size.width, sf.size.height);
            st.selection = st.bounds;
            canvas_window.setFrame_display(sf, true);
            let (tx, ty) = compute_toolbar_origin(
                st.selection,
                st.screen_frame,
                st.visible_frame,
                TOOLBAR_W,
                TOOLBAR_H,
            );
            let next_tb = NSRect::new(NSPoint::new(tx, ty), NSSize::new(TOOLBAR_W, TOOLBAR_H));
            toolbar_window.setFrame_display(next_tb, true);
            break;
        }
    }
}
