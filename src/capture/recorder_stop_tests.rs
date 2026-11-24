use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::*;

#[test]
fn shared_stop_control_fixes_single_deadline_across_hook_ui_and_cli() {
    let t0 = Instant::now();
    let ctrl = StopControl::new(t0, true);

    // First stop request (background lock hook at t0 + 18.6s) fixes deadline to t0 + 18.6s + 60s.
    let t_hook = t0 + Duration::from_millis(18_600);
    assert!(ctrl.register_stop_request(StopMode::Normal, t_hook));
    let snap1 = ctrl.snapshot();
    assert_eq!(snap1.stop_requested_at, Some(t_hook));
    assert_eq!(snap1.finalize_deadline, Some(t_hook + STOP_DUAL_AUDIO_MIN));
    assert!(snap1.worker_running);

    // Later UI/CLI calls MUST NOT overwrite `stop_requested_at` or shift `finalize_deadline`,
    // and MUST NOT spawn a second worker while one is running.
    let t_ui = t_hook + Duration::from_secs(5);
    assert!(!ctrl.register_stop_request(StopMode::Normal, t_ui));
    let snap2 = ctrl.snapshot();
    assert_eq!(snap2.stop_requested_at, Some(t_hook));
    assert_eq!(snap2.finalize_deadline, snap1.finalize_deadline);
}

#[test]
fn serial_worker_waits_for_dual_audio_mixing_instead_of_stopping_early() {
    let t0 = Instant::now();
    let ctrl = Arc::new(StopControl::new(t0, true));
    let signal = Arc::new((
        Mutex::new(DelegateSignal {
            start_completed: true,
            started: true,
            ..Default::default()
        }),
        Condvar::new(),
    ));

    let fake_clock = Arc::new(Mutex::new(t0 + Duration::from_secs(20)));
    assert!(ctrl.register_stop_request(StopMode::Normal, *fake_clock.lock().unwrap()));

    let remove_calls = Arc::new(AtomicUsize::new(0));
    let stop_calls = Arc::new(AtomicUsize::new(0));
    let ctrl_w = Arc::clone(&ctrl);
    let sig_w = Arc::clone(&signal);
    let rem_w = Arc::clone(&remove_calls);
    let stp_w = Arc::clone(&stop_calls);
    let clk_w = Arc::clone(&fake_clock);

    let worker = thread::spawn(move || {
        ctrl_w.run_serial_worker_loop(
            &sig_w,
            || {
                rem_w.fetch_add(1, Ordering::SeqCst);
                true
            },
            |_gen| {
                stp_w.fetch_add(1, Ordering::SeqCst);
            },
            || *clk_w.lock().unwrap(),
        );
    });

    // Advance simulated time by 10s (> old 2s fallback!): `stopCapture` MUST NOT fire yet!
    thread::sleep(Duration::from_millis(60));
    *fake_clock.lock().unwrap() += Duration::from_secs(10);
    thread::sleep(Duration::from_millis(80));
    assert_eq!(remove_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        stop_calls.load(Ordering::SeqCst),
        0,
        "stopCapture must NEVER fire while dual-audio RPAudioMixUtility is still finishing"
    );
    assert!(!ctrl.snapshot().forced_stop_on_timeout);

    // Signal `recordingOutputDidFinishRecording:` -> worker dispatches `stopCapture` promptly!
    {
        let mut g = signal.0.lock().unwrap();
        g.finished = true;
        signal.1.notify_all();
    }
    worker.join().expect("join worker");
    assert_eq!(remove_calls.load(Ordering::SeqCst), 1);
    assert_eq!(stop_calls.load(Ordering::SeqCst), 1);
    let snap = ctrl.snapshot();
    assert_eq!(snap.remove_phase, RemovePhase::Succeeded);
    assert!(snap.stop_capture_sent_at.is_some());
    assert!(!snap.forced_stop_on_timeout);
    assert!(!snap.worker_running);
}

#[test]
fn startup_abort_upgrades_to_full_deadline_when_started_flips_true() {
    let t0 = Instant::now();
    let ctrl = Arc::new(StopControl::new(t0, true));
    let signal = Arc::new((
        Mutex::new(DelegateSignal {
            start_completed: true,
            started: false,
            ..Default::default()
        }),
        Condvar::new(),
    ));

    let fake_clock = Arc::new(Mutex::new(t0 + Duration::from_millis(200)));
    assert!(ctrl.register_stop_request(StopMode::StartupAbort, *fake_clock.lock().unwrap()));

    let stop_calls = Arc::new(AtomicUsize::new(0));
    let ctrl_w = Arc::clone(&ctrl);
    let sig_w = Arc::clone(&signal);
    let stp_w = Arc::clone(&stop_calls);
    let clk_w = Arc::clone(&fake_clock);

    let worker = thread::spawn(move || {
        ctrl_w.run_serial_worker_loop(
            &sig_w,
            || true,
            |_gen| {
                stp_w.fetch_add(1, Ordering::SeqCst);
            },
            || *clk_w.lock().unwrap(),
        );
    });

    // Wait briefly so worker enters Step 2 while `started == false`, then flip `started = true`
    // and advance clock past STARTUP_UNSTARTED_GRACE (1s) to 5s!
    thread::sleep(Duration::from_millis(40));
    {
        let mut g = signal.0.lock().unwrap();
        g.started = true;
        signal.1.notify_all();
    }
    *fake_clock.lock().unwrap() += Duration::from_secs(5);
    thread::sleep(Duration::from_millis(80));
    assert_eq!(
        stop_calls.load(Ordering::SeqCst),
        0,
        "flipping started=true mid-startup-abort must upgrade from 1s grace to full finalize deadline"
    );

    // Finish cleanly.
    {
        let mut g = signal.0.lock().unwrap();
        g.finished = true;
        signal.1.notify_all();
    }
    worker.join().expect("join worker");
    assert_eq!(stop_calls.load(Ordering::SeqCst), 1);
    assert!(!ctrl.snapshot().forced_stop_on_timeout);
}

#[test]
fn late_start_reschedules_cleanly_without_double_remove_when_pre_start_remove_succeeded() {
    let t0 = Instant::now();
    let ctrl = Arc::new(StopControl::new(t0, false));
    let signal = Arc::new((Mutex::new(DelegateSignal::default()), Condvar::new()));

    // Pre-start cancel runs when `remove` succeeds and `finished` arrives immediately.
    let t_cancel = t0 + Duration::from_millis(50);
    assert!(ctrl.register_stop_request(StopMode::StartupAbort, t_cancel));
    {
        let mut g = signal.0.lock().unwrap();
        g.finished = true;
    }

    let remove_calls = Arc::new(AtomicUsize::new(0));
    let stop_gens = Arc::new(Mutex::new(Vec::<u64>::new()));

    ctrl.run_serial_worker_loop(
        &signal,
        || {
            remove_calls.fetch_add(1, Ordering::SeqCst);
            true
        },
        |g| stop_gens.lock().unwrap().push(g),
        || t_cancel,
    );
    assert_eq!(remove_calls.load(Ordering::SeqCst), 1);
    assert_eq!(stop_gens.lock().unwrap().len(), 1);

    // Delayed `startCapture` arrives later with `late_gen = 99`: MUST NOT call `do_remove` a
    // second time because pre-start remove already succeeded, and MUST dispatch `stopCapture(99)`.
    {
        let mut g = signal.0.lock().unwrap();
        g.stop_gen = 99;
    }
    assert!(ctrl.register_late_start_stop(99, t_cancel + Duration::from_millis(40), &signal));
    ctrl.run_serial_worker_loop(
        &signal,
        || {
            remove_calls.fetch_add(1, Ordering::SeqCst);
            true
        },
        |g| stop_gens.lock().unwrap().push(g),
        || t_cancel + Duration::from_millis(40),
    );
    assert_eq!(
        remove_calls.load(Ordering::SeqCst),
        1,
        "late start must not re-invoke removeRecordingOutput when pre-start remove already succeeded"
    );
    assert_eq!(*stop_gens.lock().unwrap(), vec![1, 99]);
}

#[test]
fn late_start_retries_remove_only_when_pre_start_remove_failed() {
    let t0 = Instant::now();
    let ctrl = Arc::new(StopControl::new(t0, false));
    let signal = Arc::new((Mutex::new(DelegateSignal::default()), Condvar::new()));

    let t_cancel = t0 + Duration::from_millis(50);
    assert!(ctrl.register_stop_request(StopMode::StartupAbort, t_cancel));

    let remove_calls = Arc::new(AtomicUsize::new(0));
    let stop_gens = Arc::new(Mutex::new(Vec::<u64>::new()));

    // First pre-start run: remove fails -> immediate pre-start stopCapture.
    ctrl.run_serial_worker_loop(
        &signal,
        || {
            remove_calls.fetch_add(1, Ordering::SeqCst);
            false
        },
        |g| stop_gens.lock().unwrap().push(g),
        || t_cancel,
    );
    assert_eq!(remove_calls.load(Ordering::SeqCst), 1);
    assert_eq!(ctrl.snapshot().remove_phase, RemovePhase::Failed);

    // Delayed `startCapture` completes (`late_gen = 7`) and writer finishes cleanly after remove.
    {
        let mut g = signal.0.lock().unwrap();
        g.stop_gen = 7;
        g.finished = true;
    }
    assert!(ctrl.register_late_start_stop(7, t_cancel + Duration::from_millis(40), &signal));
    ctrl.run_serial_worker_loop(
        &signal,
        || {
            remove_calls.fetch_add(1, Ordering::SeqCst);
            true
        },
        |g| stop_gens.lock().unwrap().push(g),
        || t_cancel + Duration::from_millis(40),
    );
    assert_eq!(remove_calls.load(Ordering::SeqCst), 2);
    assert_eq!(ctrl.snapshot().remove_phase, RemovePhase::Succeeded);
    assert_eq!(*stop_gens.lock().unwrap(), vec![1, 7]);
}
