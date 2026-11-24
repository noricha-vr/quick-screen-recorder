use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use crate::capture::recorder::finish::finalize_stop_staging_outcome;
use crate::fs_safety::{ExclusiveStagingDir, prune_stale_cache_files_in, random_hex_token};

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
    assert!(ctrl.register_late_start_stop(7, t_cancel + Duration::from_millis(40), &signal));
    {
        let mut g = signal.0.lock().unwrap();
        g.finished = true;
    }
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

#[test]
fn late_start_arriving_between_failed_remove_and_first_stop_retries_remove_and_waits_finish() {
    let t0 = Instant::now();
    let ctrl = Arc::new(StopControl::new(t0, true));
    let signal = Arc::new((Mutex::new(DelegateSignal::default()), Condvar::new()));

    let fake_clock = Arc::new(Mutex::new(t0 + Duration::from_millis(50)));
    assert!(ctrl.register_stop_request(StopMode::StartupAbort, *fake_clock.lock().unwrap()));

    let remove_calls = Arc::new(AtomicUsize::new(0));
    let stop_gens = Arc::new(Mutex::new(Vec::<u64>::new()));

    let ctrl_w = Arc::clone(&ctrl);
    let sig_w = Arc::clone(&signal);
    let rem_w = Arc::clone(&remove_calls);
    let gens_w = Arc::clone(&stop_gens);
    let clk_w = Arc::clone(&fake_clock);

    let worker = thread::spawn(move || {
        ctrl_w.run_serial_worker_loop(
            &sig_w,
            || {
                let n = rem_w.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    // First pre-start remove fails; delayed `startCapture` arrives RIGHT BEFORE
                    // the worker reaches Step 3 (so `stop_capture_sent_at` is still `None`).
                    let now = *clk_w.lock().unwrap();
                    let _ = ctrl_w.register_late_start_stop(42, now, &sig_w);
                    if let Ok(mut g) = sig_w.0.lock() {
                        g.start_completed = true;
                        g.started = true;
                    }
                    false
                } else {
                    // Second remove (post-start) succeeds; worker MUST now wait for `finished`.
                    true
                }
            },
            |g| gens_w.lock().unwrap().push(g),
            || *clk_w.lock().unwrap(),
        );
    });

    // Give worker time to run remove #1 (fail), loop back, run remove #2 (succeed), and park
    // waiting for `finished` (MUST NOT have sent `stopCapture` prematurely!).
    thread::sleep(Duration::from_millis(80));
    *fake_clock.lock().unwrap() += Duration::from_secs(10);
    thread::sleep(Duration::from_millis(60));
    assert_eq!(remove_calls.load(Ordering::SeqCst), 2);
    assert!(
        stop_gens.lock().unwrap().is_empty(),
        "must wait for recordingOutputDidFinishRecording after post-start remove retry"
    );

    // Signal finish -> worker dispatches `stopCapture(42)` once and exits cleanly.
    {
        let mut g = signal.0.lock().unwrap();
        g.finished = true;
        signal.1.notify_all();
    }
    worker.join().expect("join worker");
    assert_eq!(remove_calls.load(Ordering::SeqCst), 2);
    assert_eq!(*stop_gens.lock().unwrap(), vec![42]);
    assert_eq!(ctrl.snapshot().remove_phase, RemovePhase::Succeeded);
    assert!(!ctrl.snapshot().forced_stop_on_timeout);
}

#[test]
fn late_start_repeated_remove_failure_bounds_retry_and_still_stops_stream() {
    let t0 = Instant::now();
    let ctrl = Arc::new(StopControl::new(t0, true));
    let signal = Arc::new((Mutex::new(DelegateSignal::default()), Condvar::new()));

    let t_cancel = t0 + Duration::from_millis(50);
    assert!(ctrl.register_stop_request(StopMode::StartupAbort, t_cancel));

    let remove_calls = Arc::new(AtomicUsize::new(0));
    let stop_gens = Arc::new(Mutex::new(Vec::<u64>::new()));

    ctrl.run_serial_worker_loop(
        &signal,
        || {
            let n = remove_calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                let _ = ctrl.register_late_start_stop(77, t_cancel, &signal);
            }
            // Keep failing remove even on the post-start retry: MUST NOT infinite-loop!
            false
        },
        |g| stop_gens.lock().unwrap().push(g),
        || t_cancel,
    );
    assert_eq!(
        remove_calls.load(Ordering::SeqCst),
        2,
        "post-start remove retry must be bounded to once per late_gen"
    );
    assert_eq!(*stop_gens.lock().unwrap(), vec![77]);
    assert_eq!(ctrl.snapshot().remove_phase, RemovePhase::Failed);
    assert!(!ctrl.snapshot().worker_running);
}

#[test]
fn forced_timeout_did_fail_retains_staging_untouched_even_when_stream_stop_confirmed() {
    use std::os::unix::fs::PermissionsExt;

    let base = std::env::temp_dir().join(format!("qsr-forced-timeout-{}", random_hex_token()));
    let cache_dir = base.join("cache");
    let rec_dir = base.join("recovered");
    std::fs::create_dir_all(&cache_dir).expect("mkdir cache");
    std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o700))
        .expect("chmod cache");
    let target = cache_dir.join("capture.mp4");

    let mut staging =
        ExclusiveStagingDir::create_in_cache_root(&cache_dir, &target, "mp4", false, true)
            .expect("staging");
    let staged_path = staging.staged_file().to_path_buf();
    let stage_dir = staged_path.parent().expect("stage dir").to_path_buf();
    std::fs::write(&staged_path, b"in-flight-audio-mix-output").expect("write staged");

    // Simulate `forced_stop_on_timeout == true` triggering `didFailWithError` (`-3811`) while
    // `stopCapture` itself returned cleanly (`stop_issue == None`), including `preserve_directly = true`
    // (auto-stop / quit_after_stop): staging MUST stay untouched in place with `.qsr-keep`!
    let res = finalize_stop_staging_outcome(
        &mut staging,
        target,
        true,
        Some("Failed to process first sample buffer".to_string()),
        None,
        true,
        true,
        Ok(rec_dir),
    );
    assert!(res.is_err());
    let msg = res.unwrap_err();
    assert!(msg.contains("retained untouched"), "err={msg}");
    drop(staging);

    assert!(
        staged_path.exists(),
        "staged file must not be deleted on drop"
    );
    assert!(stage_dir.join(".qsr-keep").exists());
    // Must also survive immediate cache pruning.
    prune_stale_cache_files_in(&cache_dir, Duration::ZERO);
    assert_eq!(
        std::fs::read(&staged_path).expect("read retained file"),
        b"in-flight-audio-mix-output"
    );
    let _ = std::fs::remove_dir_all(&base);
}
