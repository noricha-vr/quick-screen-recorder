use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use crate::capture::recorder::finish::finalize_stop_staging_outcome;
use crate::fs_safety::{ExclusiveStagingDir, prune_stale_cache_files_in, random_hex_token};

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
                    if let Ok(mut g) = sig_w.0.lock() {
                        g.start_completed = true;
                        g.started = true;
                        g.stop_gen = 42;
                    }
                    let _ = ctrl_w.register_late_start_stop(42, now, &sig_w);
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
                if let Ok(mut g) = signal.0.lock() {
                    g.stop_gen = 77;
                }
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
fn late_start_generation_inversion_allocates_newer_gen_so_post_start_callback_succeeds() {
    use crate::capture::recorder::delegate::{
        record_stop_callback_for_gen, wait_for_stream_stopped,
    };

    let t0 = Instant::now();
    let ctrl = Arc::new(StopControl::new(t0, false));
    let signal = Arc::new((Mutex::new(DelegateSignal::default()), Condvar::new()));

    // 1. Pre-start cancel starts. Delayed `startCapture` bumps `stop_gen` to 1, BUT BEFORE
    //    `register_late_start_stop(1)` runs, the pre-start worker dispatches stopCapture, bumping
    //    `signal.stop_gen` from 1 -> 2 (and pre-start stop fails with an error).
    let t_cancel = t0 + Duration::from_millis(50);
    assert!(ctrl.register_stop_request(StopMode::StartupAbort, t_cancel));
    {
        let mut g = signal.0.lock().unwrap();
        g.start_completed = true;
        g.stop_gen = 1; // delayed start already bumped stop_gen to 1
    }

    let stop_gens = Arc::new(Mutex::new(Vec::<u64>::new()));
    let gens_c = Arc::clone(&stop_gens);
    let sig_c = Arc::clone(&signal);
    ctrl.run_serial_worker_loop(
        &signal,
        || false,
        move |g| {
            gens_c.lock().unwrap().push(g);
            // Pre-start worker got gen 2 (newer than delayed start's 1) and failed!
            let _ = record_stop_callback_for_gen(&sig_c, g, Some("pre-start failure".to_string()));
        },
        || t_cancel,
    );
    assert_eq!(*stop_gens.lock().unwrap(), vec![2]);

    // 2. Now delayed start's `register_late_start_stop(1)` runs with stale preferred_gen=1:
    //    `ensure_latest_stop_generation` MUST notice `signal.stop_gen (2) != 1`, allocate gen 3,
    //    clear the prior error, and accept the post-start stop callback for gen 3!
    assert!(ctrl.register_late_start_stop(1, t_cancel + Duration::from_millis(20), &signal));
    {
        let mut g = signal.0.lock().unwrap();
        g.finished = true;
    }
    let gens_c2 = Arc::clone(&stop_gens);
    let sig_c2 = Arc::clone(&signal);
    ctrl.run_serial_worker_loop(
        &signal,
        || true,
        move |g| {
            gens_c2.lock().unwrap().push(g);
            assert!(
                record_stop_callback_for_gen(&sig_c2, g, None),
                "post-start callback for allocated gen {g} must NOT be rejected as stale"
            );
        },
        || t_cancel + Duration::from_millis(20),
    );
    assert_eq!(*stop_gens.lock().unwrap(), vec![2, 3]);
    assert!(wait_for_stream_stopped(&signal, Duration::from_millis(20)));
}

#[test]
fn hung_remove_past_deadline_claims_emergency_stop_once_and_protects_staging() {
    let t0 = Instant::now();
    let ctrl = Arc::new(StopControl::new(t0, false));
    let signal = Arc::new((Mutex::new(DelegateSignal::default()), Condvar::new()));
    let t_stop = t0 + Duration::from_secs(5);
    assert!(ctrl.register_stop_request(StopMode::Normal, t_stop));

    // Park a background worker inside `do_remove()` so `remove_phase == RemovePhase::InFlight`.
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let ctrl_w = Arc::clone(&ctrl);
    let sig_w = Arc::clone(&signal);
    let gate_w = Arc::clone(&gate);
    let worker_stop_calls = Arc::new(AtomicUsize::new(0));
    let w_calls = Arc::clone(&worker_stop_calls);
    let worker = thread::spawn(move || {
        ctrl_w.run_serial_worker_loop(
            &sig_w,
            || {
                let (l, c) = &*gate_w;
                let g = l.lock().unwrap();
                drop(c.wait_while(g, |open| !*open));
                true
            },
            |_| {
                w_calls.fetch_add(1, Ordering::SeqCst);
            },
            || t_stop,
        );
    });

    // Wait briefly until `remove_phase == InFlight`.
    for _ in 0..40 {
        if ctrl.snapshot().remove_phase == RemovePhase::InFlight {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(ctrl.snapshot().remove_phase, RemovePhase::InFlight);

    let deadline = ctrl.snapshot().finalize_deadline.unwrap();
    // Before deadline: emergency stop MUST NOT trigger.
    assert!(
        ctrl.claim_emergency_stop_if_remove_hung(&signal, deadline - Duration::from_secs(1))
            .is_none()
    );
    // At deadline: emergency stop triggers ONCE, sets `forced_stop_on_timeout`, records `sent_at`.
    let emg_gen = ctrl
        .claim_emergency_stop_if_remove_hung(&signal, deadline)
        .expect("must claim emergency stop once remove hung past deadline");
    assert!(emg_gen >= 1);
    assert!(ctrl.snapshot().forced_stop_on_timeout);
    assert_eq!(ctrl.snapshot().stop_capture_sent_at, Some(deadline));
    // Second claim is a no-op (at-most-once).
    assert!(
        ctrl.claim_emergency_stop_if_remove_hung(&signal, deadline + Duration::from_secs(1))
            .is_none()
    );

    // Unblock worker so thread exits cleanly; worker MUST NOT double-send `stopCapture`.
    {
        let mut open = gate.0.lock().unwrap();
        *open = true;
        gate.1.notify_all();
    }
    {
        let mut g = signal.0.lock().unwrap();
        g.finished = true;
        signal.1.notify_all();
    }
    worker.join().expect("join unblocked worker");
    assert_eq!(worker_stop_calls.load(Ordering::SeqCst), 0);
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
