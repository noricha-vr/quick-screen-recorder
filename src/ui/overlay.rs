use std::cell::RefCell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSBackingStoreType, NSColor,
    NSFloatingWindowLevel, NSFont, NSPopUpButton, NSScreen, NSStatusWindowLevel, NSTextField,
    NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSNumber, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use crate::capture::audio_devices::{
    MicDeviceInfo, MicMenuItem, build_mic_menu_model, list_microphone_devices,
};
use crate::capture::shareable::{DisplayInfo, WindowInfo};
use crate::capture::{CaptureConfig, MicInputPreference};
use crate::config::Preferences;
use crate::fs_safety::sanitize_terminal_text;
use crate::geometry::{
    AspectRatio, DragMode, Rect, compute_toolbar_origin, hit_test_window_frames,
    window_frame_to_overlay_rect,
};
use crate::ui::overlay_actions::build_toolbar_actions;
use crate::ui::overlay_canvas::{CanvasIvars, OverlayCanvasView};
use crate::ui::overlay_toolbar::{
    OverlayToolbarActions, TOOLBAR_H, TOOLBAR_W, populate_toolbar_controls,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    Area,
    Window,
    Screen,
}

pub type StartCaptureCallback = Rc<dyn Fn(Result<(CaptureConfig, Preferences), String>)>;

pub struct ActiveDrag {
    pub mode: DragMode,
    pub start_x: f64,
    pub start_y: f64,
    pub initial_rect: Rect,
}

pub struct OverlayState {
    pub mode: SelectionMode,
    pub selection: Rect,
    pub bounds: Rect,
    pub screen_frame: Rect,
    pub visible_frame: Rect,
    pub overlay_display_id: u32,
    pub screen_display_id: u32,
    pub scale_factor: f64,
    pub aspect: AspectRatio,
    pub fps: u32,
    pub show_cursor: bool,
    pub highlight_clicks: bool,
    pub capture_system_audio: bool,
    pub mic_input: MicInputPreference,
    pub mic_items: Vec<MicMenuItem>,
    pub windows: Vec<WindowInfo>,
    pub selected_window_idx: usize,
    pub hovered_window_idx: Option<usize>,
    pub displays: Vec<DisplayInfo>,
    pub selected_display_idx: usize,
    pub demo_backdrop: bool,
    pub drag: Option<ActiveDrag>,
}

impl OverlayState {
    pub fn display_origin(&self) -> (f64, f64) {
        self.displays
            .iter()
            .find(|d| d.display_id == self.overlay_display_id)
            .map(|d| (d.frame.x, d.frame.y))
            .unwrap_or((0.0, 0.0))
    }

    pub fn window_overlay_rect(&self, idx: usize) -> Option<Rect> {
        let wf = self.windows.get(idx)?.frame;
        window_frame_to_overlay_rect(wf, self.display_origin(), self.bounds)
    }

    pub fn display_for_window(&self, idx: usize) -> Option<u32> {
        let wf = self.windows.get(idx)?.frame;
        let cx = wf.x + wf.width * 0.5;
        let cy = wf.y + wf.height * 0.5;
        if let Some(d) = self.displays.iter().find(|d| d.frame.contains(cx, cy)) {
            return Some(d.display_id);
        }
        self.displays
            .iter()
            .find(|d| {
                let ix0 = wf.x.max(d.frame.x);
                let iy0 = wf.y.max(d.frame.y);
                let ix1 = wf.right().min(d.frame.right());
                let iy1 = wf.top_y_up().min(d.frame.top_y_up());
                ix1 > ix0 && iy1 > iy0
            })
            .map(|d| d.display_id)
    }

    pub fn hit_test_window_at(
        &self,
        local_x: f64,
        local_y: f64,
        cycle_selected: bool,
    ) -> Option<usize> {
        let frames: Vec<Rect> = self.windows.iter().map(|w| w.frame).collect();
        let cur = cycle_selected.then_some(self.selected_window_idx);
        hit_test_window_frames(
            &frames,
            self.display_origin(),
            self.bounds.height,
            local_x,
            local_y,
            cur,
        )
    }

    pub fn select_window(&mut self, idx: usize) -> bool {
        if let Some(proj) = self.window_overlay_rect(idx) {
            self.selected_window_idx = idx;
            self.selection = proj;
            true
        } else {
            false
        }
    }
}

define_class!(
    #[unsafe(super(NSWindow))]
    #[thread_kind = MainThreadOnly]
    #[name = "QSRKeyableOverlayWindow"]
    pub struct KeyableOverlayWindow;

    unsafe impl NSObjectProtocol for KeyableOverlayWindow {}

    impl KeyableOverlayWindow {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            true
        }
    }
);

impl KeyableOverlayWindow {
    pub fn new_borderless(mtm: MainThreadMarker, frame: NSRect, defer: bool) -> Retained<NSWindow> {
        let win: Retained<Self> = unsafe {
            msg_send![
                Self::alloc(mtm),
                initWithContentRect: frame,
                styleMask: NSWindowStyleMask::Borderless,
                backing: NSBackingStoreType::Buffered,
                defer: defer
            ]
        };
        Retained::into_super(win)
    }
}

pub struct SelectorOverlay {
    pub canvas_window: Retained<NSWindow>,
    pub toolbar_window: Retained<NSWindow>,
    _actions: Retained<OverlayToolbarActions>,
}

impl SelectorOverlay {
    pub fn show(
        mtm: MainThreadMarker,
        prefs: &Preferences,
        displays: Vec<DisplayInfo>,
        windows: Vec<WindowInfo>,
        demo_backdrop: bool,
        on_start: StartCaptureCallback,
        on_cancel: Rc<dyn Fn()>,
    ) -> Result<Self, String> {
        let screen = NSScreen::mainScreen(mtm).ok_or("No active macOS screen found")?;
        let screen_frame = screen.frame();
        let visible_frame = screen.visibleFrame();
        let scale_factor = screen.backingScaleFactor();
        let matched_display_id = if demo_backdrop {
            displays.first().map(|d| d.display_id).unwrap_or(1)
        } else {
            let sid = extract_nsscreen_display_id(&screen)
                .ok_or("Unable to resolve CGDirectDisplayID for active NSScreen")?;
            displays
                .iter()
                .find(|d| d.display_id == sid)
                .map(|d| d.display_id)
                .ok_or_else(|| {
                    format!(
                        "Active screen (ID {sid}) is not present in ScreenCaptureKit display list"
                    )
                })?
        };
        let matched_idx = displays
            .iter()
            .position(|d| d.display_id == matched_display_id)
            .unwrap_or(0);

        let win_frame = if demo_backdrop {
            NSRect::new(
                NSPoint::new(
                    screen_frame.origin.x + (screen_frame.size.width - 1080.0).max(40.0) * 0.5,
                    screen_frame.origin.y + (screen_frame.size.height - 680.0).max(40.0) * 0.5,
                ),
                NSSize::new(1080.0, 680.0),
            )
        } else {
            screen_frame
        };

        let local_bounds = Rect::new(0.0, 0.0, win_frame.size.width, win_frame.size.height);
        let (tb_screen_rect, tb_visible_rect) = if demo_backdrop {
            (local_bounds, local_bounds)
        } else {
            (nsrect_to_rect(screen_frame), nsrect_to_rect(visible_frame))
        };
        let initial_sel = if demo_backdrop {
            Rect::new(190.0, 184.0, 700.0, 412.0)
        } else {
            prefs.last_region.clamp_and_align_even(local_bounds)
        };

        let mic_devices = if demo_backdrop {
            vec![MicDeviceInfo {
                id: "demo-mic".to_string(),
                name: "Built-in Mic".to_string(),
                is_default: true,
            }]
        } else {
            list_microphone_devices()
        };
        let (mic_items, selected_mic_idx) = build_mic_menu_model(&prefs.mic_input, &mic_devices);
        let initial_mic_pref = mic_items
            .get(selected_mic_idx)
            .map(MicMenuItem::to_preference)
            .unwrap_or_default();

        let state = Rc::new(RefCell::new(OverlayState {
            mode: SelectionMode::Area,
            selection: initial_sel,
            bounds: local_bounds,
            screen_frame: tb_screen_rect,
            visible_frame: tb_visible_rect,
            overlay_display_id: matched_display_id,
            screen_display_id: matched_display_id,
            scale_factor,
            aspect: prefs.aspect_ratio,
            fps: if prefs.fps >= 60 { 60 } else { 30 },
            show_cursor: prefs.show_cursor,
            highlight_clicks: prefs.highlight_clicks,
            capture_system_audio: prefs.capture_system_audio,
            mic_input: initial_mic_pref,
            mic_items: mic_items.clone(),
            windows,
            selected_window_idx: 0,
            hovered_window_idx: None,
            displays,
            selected_display_idx: matched_idx,
            demo_backdrop,
            drag: None,
        }));

        let canvas_window = KeyableOverlayWindow::new_borderless(mtm, win_frame, false);
        unsafe { canvas_window.setReleasedWhenClosed(false) };
        if let Some(dark) = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }) {
            canvas_window.setAppearance(Some(&dark));
        }
        canvas_window.setOpaque(demo_backdrop);
        canvas_window.setBackgroundColor(Some(&NSColor::clearColor()));
        canvas_window.setIgnoresMouseEvents(false);
        canvas_window.setMovable(false);
        canvas_window.setMovableByWindowBackground(false);
        canvas_window.setAcceptsMouseMovedEvents(true);
        canvas_window.setLevel(NSFloatingWindowLevel);

        let scale_for_px = scale_factor.max(1.0);
        let w_field = NSTextField::textFieldWithString(
            &NSString::from_str(&format!(
                "{}",
                (initial_sel.width * scale_for_px).round() as u32
            )),
            mtm,
        );
        let x_lbl = NSTextField::labelWithString(&NSString::from_str("×"), mtm);
        let h_field = NSTextField::textFieldWithString(
            &NSString::from_str(&format!(
                "{}",
                (initial_sel.height * scale_for_px).round() as u32
            )),
            mtm,
        );
        let aspect_popup = NSPopUpButton::new(mtm);
        for ar in AspectRatio::ALL {
            aspect_popup.addItemWithTitle(&NSString::from_str(ar.label()));
        }
        aspect_popup.selectItemAtIndex(prefs.aspect_ratio.index() as isize);

        let target_popup = NSPopUpButton::new(mtm);
        target_popup.setHidden(true);

        let window_badge = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        window_badge.setFont(Some(&NSFont::boldSystemFontOfSize(12.0)));
        window_badge.setTextColor(Some(&NSColor::whiteColor()));
        window_badge.setDrawsBackground(true);
        window_badge.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            0.08, 0.11, 0.18, 0.88,
        )));
        window_badge.setHidden(true);

        let (init_tb_x, init_tb_y) = compute_toolbar_origin(
            initial_sel,
            tb_screen_rect,
            tb_visible_rect,
            TOOLBAR_W,
            TOOLBAR_H,
        );
        let bar_rect = NSRect::new(
            NSPoint::new(init_tb_x, init_tb_y),
            NSSize::new(TOOLBAR_W, TOOLBAR_H),
        );
        let toolbar_window = KeyableOverlayWindow::new_borderless(mtm, bar_rect, demo_backdrop);
        unsafe { toolbar_window.setReleasedWhenClosed(false) };
        if let Some(dark) = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }) {
            toolbar_window.setAppearance(Some(&dark));
        }

        let bar_container = NSView::initWithFrame(
            NSView::alloc(mtm),
            if demo_backdrop {
                NSRect::new(
                    NSPoint::new(init_tb_x, init_tb_y),
                    NSSize::new(TOOLBAR_W, TOOLBAR_H),
                )
            } else {
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(TOOLBAR_W, TOOLBAR_H))
            },
        );

        let w_sync = w_field.clone();
        let h_sync = h_field.clone();
        let badge_sync = window_badge.clone();
        let tb_win_sync = toolbar_window.clone();
        let tb_view_sync = bar_container.clone();
        let state_sync = Rc::clone(&state);
        let sync_fields: Rc<dyn Fn()> = Rc::new(move || {
            let st = state_sync.borrow();
            let sf = st.scale_factor.max(1.0);
            w_sync.setStringValue(&NSString::from_str(&format!(
                "{}",
                (st.selection.width * sf).round() as u32
            )));
            h_sync.setStringValue(&NSString::from_str(&format!(
                "{}",
                (st.selection.height * sf).round() as u32
            )));
            let (tx, ty) = compute_toolbar_origin(
                st.selection,
                st.screen_frame,
                st.visible_frame,
                TOOLBAR_W,
                TOOLBAR_H,
            );
            if st.demo_backdrop {
                tb_view_sync.setFrameOrigin(NSPoint::new(tx, ty));
            } else {
                tb_win_sync.setFrameOrigin(NSPoint::new(tx, ty));
            }
            if st.mode == SelectionMode::Window && !st.demo_backdrop {
                if let Some(win) = st.windows.get(st.selected_window_idx) {
                    let clean = sanitize_terminal_text(&win.display_label(), false);
                    let label = format!("  {}  ", clean.chars().take(52).collect::<String>());
                    badge_sync.setStringValue(&NSString::from_str(&label));
                    badge_sync.sizeToFit();
                    let bsize = badge_sync.frame().size;
                    let bw = bsize.width.min((st.selection.width - 16.0).max(40.0));
                    let bx = st.selection.x + 8.0;
                    let by = (st.selection.top_y_up() - 28.0).max(st.selection.y + 4.0);
                    badge_sync.setFrame(NSRect::new(NSPoint::new(bx, by), NSSize::new(bw, 22.0)));
                    badge_sync.setHidden(false);
                } else {
                    badge_sync.setHidden(true);
                }
            } else {
                badge_sync.setHidden(true);
            }
        });

        let canvas_view = OverlayCanvasView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), win_frame.size),
            CanvasIvars {
                state: Rc::clone(&state),
                target_popup: target_popup.clone(),
                on_selection_changed: Rc::clone(&sync_fields),
            },
        );
        canvas_view.addSubview(&window_badge);
        canvas_window.setContentView(Some(&canvas_view));
        let _ = canvas_window.makeFirstResponder(Some(&canvas_view));

        let canvas_nsview: Retained<NSView> = Retained::into_super(canvas_view.clone());
        let actions = build_toolbar_actions(
            mtm,
            &state,
            &canvas_window,
            &toolbar_window,
            &canvas_nsview,
            &aspect_popup,
            &w_field,
            &x_lbl,
            &h_field,
            &target_popup,
            &sync_fields,
            prefs,
            on_start,
            on_cancel,
        );

        populate_toolbar_controls(
            mtm,
            &bar_container,
            &actions,
            &aspect_popup,
            &w_field,
            &x_lbl,
            &h_field,
            &target_popup,
            &mic_items,
            selected_mic_idx,
            prefs,
        );

        if demo_backdrop {
            canvas_view.addSubview(&bar_container);
        } else {
            toolbar_window.setOpaque(false);
            toolbar_window.setBackgroundColor(Some(&NSColor::clearColor()));
            toolbar_window.setHasShadow(true);
            toolbar_window.setMovable(false);
            toolbar_window.setMovableByWindowBackground(false);
            toolbar_window.setLevel(NSStatusWindowLevel);
            toolbar_window.setContentView(Some(&bar_container));
        }

        canvas_window.makeKeyAndOrderFront(None);
        if !demo_backdrop {
            toolbar_window.orderFrontRegardless();
        }

        Ok(Self {
            canvas_window,
            toolbar_window,
            _actions: actions,
        })
    }

    pub fn close(&self) {
        self.toolbar_window.orderOut(None);
        self.canvas_window.orderOut(None);
        self.toolbar_window.close();
        self.canvas_window.close();
    }
}

pub fn nsrect_to_rect(r: NSRect) -> Rect {
    Rect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}

pub fn extract_nsscreen_display_id(screen: &NSScreen) -> Option<u32> {
    let desc = screen.deviceDescription();
    let key = NSString::from_str("NSScreenNumber");
    let val: Option<Retained<AnyObject>> = desc.objectForKey(&key);
    let obj = val?;
    let num: &NSNumber = unsafe { &*(Retained::as_ptr(&obj) as *const NSNumber) };
    Some(num.unsignedIntValue())
}
