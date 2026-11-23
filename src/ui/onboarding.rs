use std::cell::RefCell;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSWindow, NSWindowSharingType};
use objc2_foundation::{NSObject, NSObjectProtocol, NSTimer};

use crate::permissions::{
    has_screen_capture_permission, open_screen_recording_settings, reveal_in_finder,
};
use crate::ui::onboarding_layout::{
    OnboardingUi, apply_step_ui, build_onboarding_ui, position_onboarding_window,
};
use crate::ui::onboarding_visual::OnboardingStep;

pub struct OnboardingCallbacks {
    pub on_start_recording: Box<dyn Fn(MainThreadMarker)>,
    pub on_stop_recording: Box<dyn Fn(MainThreadMarker)>,
    pub on_skip: Box<dyn Fn(MainThreadMarker)>,
    pub on_done: Box<dyn Fn(MainThreadMarker)>,
}

struct OnboardingActionIvars {
    on_primary: Box<dyn Fn(MainThreadMarker)>,
    on_secondary: Box<dyn Fn(MainThreadMarker)>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "QSROnboardingActions"]
    #[ivars = OnboardingActionIvars]
    struct OnboardingActions;

    unsafe impl NSObjectProtocol for OnboardingActions {}

    impl OnboardingActions {
        #[unsafe(method(primaryClicked:))]
        fn primary_clicked(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_primary)(self.mtm());
        }

        #[unsafe(method(secondaryClicked:))]
        fn secondary_clicked(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_secondary)(self.mtm());
        }
    }
);

struct OnboardingState {
    step: OnboardingStep,
    permission_granted: bool,
    saved_path: Option<PathBuf>,
    demo_freeze_phase: Option<f64>,
    anim_timer: Option<Retained<NSTimer>>,
    perm_timer: Option<Retained<NSTimer>>,
}

pub struct OnboardingWindow {
    pub window: Retained<NSWindow>,
    ui: Rc<OnboardingUi>,
    state: Rc<RefCell<OnboardingState>>,
    _actions: Retained<OnboardingActions>,
}

impl OnboardingWindow {
    pub fn open(
        mtm: MainThreadMarker,
        initial_step: OnboardingStep,
        permission_granted: bool,
        saved_path: Option<PathBuf>,
        demo_freeze_phase: Option<f64>,
        callbacks: OnboardingCallbacks,
    ) -> Self {
        let ui = Rc::new(build_onboarding_ui(mtm, initial_step));
        let window = ui.window.clone();
        if demo_freeze_phase.is_none() {
            // Prevent ScreenCaptureKit / WindowServer capture streams from ever recording the tutorial window.
            window.setSharingType(NSWindowSharingType::None);
        }
        let state = Rc::new(RefCell::new(OnboardingState {
            step: initial_step,
            permission_granted,
            saved_path,
            demo_freeze_phase,
            anim_timer: None,
            perm_timer: None,
        }));

        let cbs = Rc::new(callbacks);
        let st_pri = Rc::clone(&state);
        let ui_pri = Rc::clone(&ui);
        let cbs_pri = Rc::clone(&cbs);
        let st_sec = Rc::clone(&state);
        let cbs_sec = Rc::clone(&cbs);

        let actions: Retained<OnboardingActions> = {
            let ivars = OnboardingActionIvars {
                on_primary: Box::new(move |m| {
                    let step = st_pri.borrow().step;
                    match step {
                        OnboardingStep::Permission => {
                            let _ = open_screen_recording_settings();
                            if has_screen_capture_permission() {
                                transition_step(
                                    &ui_pri,
                                    &st_pri,
                                    OnboardingStep::StartRecording,
                                    true,
                                    None,
                                );
                                if let Some(pt) = st_pri.borrow_mut().perm_timer.take() {
                                    pt.invalidate();
                                }
                            }
                        }
                        OnboardingStep::StartRecording => (cbs_pri.on_start_recording)(m),
                        OnboardingStep::RecordingInProgress => (cbs_pri.on_stop_recording)(m),
                        OnboardingStep::Completed => (cbs_pri.on_done)(m),
                    }
                }),
                on_secondary: Box::new(move |m| {
                    let (step, saved) = {
                        let s = st_sec.borrow();
                        (s.step, s.saved_path.clone())
                    };
                    if step == OnboardingStep::Completed {
                        if let Some(p) = saved {
                            reveal_in_finder(&p);
                        }
                    } else {
                        (cbs_sec.on_skip)(m);
                    }
                }),
            };
            let this = OnboardingActions::alloc(mtm).set_ivars(ivars);
            unsafe { msg_send![super(this), init] }
        };

        unsafe {
            ui.primary_btn.setTarget(Some(&actions));
            ui.primary_btn.setAction(Some(sel!(primaryClicked:)));
            ui.secondary_btn.setTarget(Some(&actions));
            ui.secondary_btn.setAction(Some(sel!(secondaryClicked:)));
        }

        let this = Self {
            window,
            ui,
            state,
            _actions: actions,
        };
        this.apply_and_present(mtm, initial_step, permission_granted, None, false);
        this
    }

    pub fn window_number(&self) -> u32 {
        self.window.windowNumber().max(0) as u32
    }

    pub fn show_step(
        &self,
        mtm: MainThreadMarker,
        step: OnboardingStep,
        permission_granted: bool,
        saved_path: Option<PathBuf>,
        dock_top_right: bool,
    ) {
        self.apply_and_present(mtm, step, permission_granted, saved_path, dock_top_right);
    }

    pub fn hide_temporarily(&self) {
        stop_timers(&self.state);
        self.window.orderOut(None);
    }

    pub fn close(&self) {
        stop_timers(&self.state);
        self.window.close();
    }

    fn apply_and_present(
        &self,
        mtm: MainThreadMarker,
        step: OnboardingStep,
        permission_granted: bool,
        saved_path: Option<PathBuf>,
        dock_top_right: bool,
    ) {
        position_onboarding_window(mtm, &self.window, dock_top_right);
        transition_step(&self.ui, &self.state, step, permission_granted, saved_path);
        ensure_timers(&self.ui, &self.state);

        let app = NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        self.window.makeKeyAndOrderFront(None);
    }
}

impl Drop for OnboardingWindow {
    fn drop(&mut self) {
        stop_timers(&self.state);
    }
}

fn transition_step(
    ui: &OnboardingUi,
    state: &RefCell<OnboardingState>,
    step: OnboardingStep,
    permission_granted: bool,
    saved_path: Option<PathBuf>,
) {
    let (phase, saved_clone) = {
        let mut st = state.borrow_mut();
        st.step = step;
        st.permission_granted = permission_granted;
        if let Some(p) = saved_path {
            st.saved_path = Some(p);
        }
        (st.demo_freeze_phase.unwrap_or(0.0), st.saved_path.clone())
    };
    apply_step_ui(ui, step, permission_granted, saved_clone.as_deref(), phase);
}

fn ensure_timers(ui: &Rc<OnboardingUi>, state: &Rc<RefCell<OnboardingState>>) {
    stop_timers(state);
    let (step, frozen) = {
        let s = state.borrow();
        (s.step, s.demo_freeze_phase.is_some())
    };
    if frozen {
        return;
    }

    if step != OnboardingStep::Completed {
        let ui_anim = Rc::clone(ui);
        let st_anim = Rc::clone(state);
        let anim_block = RcBlock::new(move |_t: NonNull<NSTimer>| {
            if !ui_anim.window.isVisible() {
                stop_timers(&st_anim);
                return;
            }
            ui_anim.visual.advance_phase(0.033 / 3.8);
        });
        let anim = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.033, true, &anim_block)
        };
        state.borrow_mut().anim_timer = Some(anim);
    }

    if step == OnboardingStep::Permission {
        let ui_perm = Rc::clone(ui);
        let st_perm = Rc::clone(state);
        let perm_block = RcBlock::new(move |_t: NonNull<NSTimer>| {
            if !ui_perm.window.isVisible() || st_perm.borrow().step != OnboardingStep::Permission {
                if let Some(pt) = st_perm.borrow_mut().perm_timer.take() {
                    pt.invalidate();
                }
                return;
            }
            if has_screen_capture_permission() {
                transition_step(
                    &ui_perm,
                    &st_perm,
                    OnboardingStep::StartRecording,
                    true,
                    None,
                );
                if let Some(pt) = st_perm.borrow_mut().perm_timer.take() {
                    pt.invalidate();
                }
            }
        });
        let perm = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0, true, &perm_block)
        };
        state.borrow_mut().perm_timer = Some(perm);
    }
}

fn stop_timers(state: &RefCell<OnboardingState>) {
    let mut st = state.borrow_mut();
    if let Some(t) = st.anim_timer.take() {
        t.invalidate();
    }
    if let Some(t) = st.perm_timer.take() {
        t.invalidate();
    }
}
