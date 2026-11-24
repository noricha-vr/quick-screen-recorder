use super::*;

#[test]
fn delayed_start_callback_interrupted_triggers_immediate_and_late_stop() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    let (tx, rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let latch = InterruptLatch::new();

    let immediate_stops = Arc::new(AtomicUsize::new(0));
    let late_cleanups = Arc::new(AtomicUsize::new(0));
    let imm_hook = Arc::clone(&immediate_stops);
    let cancel_hook = Arc::clone(&cancelled);
    latch.attach_stop_hook(Arc::new(move || {
        cancel_hook.store(true, Ordering::Release);
        imm_hook.fetch_add(1, Ordering::SeqCst);
    }));

    // Trigger screen lock 30ms into the start wait, BEFORE the delayed start callback fires at 90ms.
    let latch_bg = latch.clone();
    let interrupter = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        let _ = latch_bg.trigger(crate::capture::SystemInterruptReason::ScreenLocked);
    });

    let signal = Arc::new((Mutex::new(DelegateSignal::default()), Condvar::new()));
    let cancel_cb = Arc::clone(&cancelled);
    let late_cb = Arc::clone(&late_cleanups);
    let sig_cb = Arc::clone(&signal);
    let delayed_start_cb = thread::spawn(move || {
        thread::sleep(Duration::from_millis(90));
        handle_start_callback_result(Ok(()), &sig_cb, &cancel_cb, &tx, |late_gen| {
            late_cb.fetch_add(1, Ordering::SeqCst);
            assert!(delegate::record_stop_callback_for_gen(
                &sig_cb, late_gen, None
            ));
        });
    });

    let started = Instant::now();
    let sig_wait = Arc::clone(&signal);
    let wait_res = wait_for_capture_start_with_interrupt(
        &rx,
        &signal,
        Duration::from_secs(8),
        &cancelled,
        || {
            latch
                .reason()
                .map(|r| format!("interrupted: {}", r.clause()))
        },
        || false,
        || {
            immediate_stops.fetch_add(1, Ordering::SeqCst);
            // Simulate pre-start `-3808` setting stream_stopped=true while start_completed=false.
            if let Ok(mut g) = sig_wait.0.lock() {
                g.stream_stopped = true;
            }
            delegate::wait_for_stream_stopped(&sig_wait, Duration::from_secs(2))
        },
    );
    assert!(
        wait_res.is_err(),
        "interrupted startup must abort instead of waiting for delayed callback"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "must abort promptly rather than waiting full 8s timeout"
    );
    assert!(
        immediate_stops.load(Ordering::SeqCst) >= 1,
        "immediate stop must fire when interrupted during start wait"
    );

    interrupter.join().expect("join interrupter");
    delayed_start_cb.join().expect("join delayed callback");
    assert_eq!(
        late_cleanups.load(Ordering::SeqCst),
        1,
        "delayed start completion after cancellation must trigger forced late-start teardown"
    );
}

#[test]
fn interrupt_after_writer_started_preserves_session_for_normal_stop_and_recovery() {
    use std::sync::atomic::AtomicUsize;

    // Case 1: Writer started before interrupt -> wait_for_recording_started returns Ok(()) so
    // `RecordingSession` keeps `staging` alive and hands it to normal stop/recovery.
    let signal = Arc::new((
        Mutex::new(DelegateSignal {
            started: true,
            ..Default::default()
        }),
        Condvar::new(),
    ));
    let stops = AtomicUsize::new(0);
    let res = wait_for_recording_started(
        &signal,
        Duration::from_secs(1),
        || Some("interrupted: screen locked".to_string()),
        || {
            stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            true
        },
    );
    assert!(
        res.is_ok(),
        "once writer started, startup must return Ok so caller salvages/saves staging"
    );
    assert_eq!(stops.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Case 2: Writer started and also received a delegate error -> still returns Ok(()) so
    // `PendingRecordingStop::finish` can run strict salvage validation on staging.
    let err_signal = Arc::new((
        Mutex::new(DelegateSignal {
            started: true,
            error: Some("stream interrupted".to_string()),
            ..Default::default()
        }),
        Condvar::new(),
    ));
    let res_err_after_start =
        wait_for_recording_started(&err_signal, Duration::from_secs(1), || None, || true);
    assert!(res_err_after_start.is_ok());

    // Case 3: Writer has NOT started and stop IS confirmed -> returns Err cleanly (no footage lost).
    let unstarted = Arc::new((Mutex::new(DelegateSignal::default()), Condvar::new()));
    let unstarted_stops = AtomicUsize::new(0);
    let res_unstarted = wait_for_recording_started(
        &unstarted,
        Duration::from_secs(1),
        || Some("interrupted: screen locked".to_string()),
        || {
            unstarted_stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            true
        },
    );
    assert!(res_unstarted.is_err());
    assert_eq!(unstarted_stops.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Case 4: `wait_for_capture_start_with_interrupt` hands off to stop if writer already started.
    let (_tx, rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let cap_sig = Arc::new((Mutex::new(DelegateSignal::default()), Condvar::new()));
    let cancelled = AtomicBool::new(false);
    let late_stops = AtomicUsize::new(0);
    let res_capture_wait = wait_for_capture_start_with_interrupt(
        &rx,
        &cap_sig,
        Duration::from_secs(1),
        &cancelled,
        || Some("interrupted: screen locked".to_string()),
        || true,
        || {
            late_stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            true
        },
    );
    assert_eq!(res_capture_wait, Ok(StartWaitOutcome::HandOffToStop));
    assert!(!cancelled.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(late_stops.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn async_did_start_before_stream_stopped_and_unconfirmed_stop_preserve_session() {
    use std::thread;

    // Case 1: Stop initiates asynchronously; AFTER the stop request is sent, a background thread
    // delivers `did_start` first and `stream_stopped` afterward. `wait_for_stream_stopped` waits
    // for stop confirmation so the post-stop recheck sees `started == true` and preserves staging!
    let signal = Arc::new((
        Mutex::new(DelegateSignal {
            start_completed: true,
            ..Default::default()
        }),
        Condvar::new(),
    ));
    let sig_bg = Arc::clone(&signal);
    let sig_wait = Arc::clone(&signal);
    let mut async_handle: Option<thread::JoinHandle<()>> = None;
    let res = wait_for_recording_started(
        &signal,
        Duration::from_secs(1),
        || Some("interrupted: screen locked".to_string()),
        || {
            let s = Arc::clone(&sig_bg);
            async_handle = Some(thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                if let Ok(mut g) = s.0.lock() {
                    g.started = true;
                }
                thread::sleep(Duration::from_millis(20));
                if let Ok(mut g) = s.0.lock() {
                    g.stream_stopped = true;
                    s.1.notify_all();
                }
            }));
            delegate::wait_for_stream_stopped(&sig_wait, Duration::from_secs(2))
        },
    );
    assert!(
        res.is_ok(),
        "async did_start arriving before stream_stopped confirmation must preserve staging"
    );
    if let Some(h) = async_handle {
        h.join().expect("join async delegate thread");
    }

    // Case 2: Stream stop confirmation times out (`false`) even while `started == false` ->
    // MUST still preserve staging (`Ok` / `HandOffToStop`) instead of deleting it prematurely.
    let unconfirmed_sig = Arc::new((Mutex::new(DelegateSignal::default()), Condvar::new()));
    let res_unconfirmed = wait_for_recording_started(
        &unconfirmed_sig,
        Duration::from_secs(1),
        || Some("interrupted: screen locked".to_string()),
        || false,
    );
    assert!(
        res_unconfirmed.is_ok(),
        "unconfirmed stream stop must hand off staging to normal stop/recovery"
    );

    let (_tx, rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let cap_sig = Arc::new((Mutex::new(DelegateSignal::default()), Condvar::new()));
    let cancelled = AtomicBool::new(false);
    let res_cap_unconfirmed = wait_for_capture_start_with_interrupt(
        &rx,
        &cap_sig,
        Duration::from_secs(1),
        &cancelled,
        || Some("interrupted: screen locked".to_string()),
        || false,
        || false,
    );
    assert_eq!(
        res_cap_unconfirmed,
        Ok(StartWaitOutcome::HandOffToStop),
        "unconfirmed capture-start stop must hand off to normal stop/recovery"
    );
}

#[test]
fn stop_error_and_failed_late_start_stop_never_count_as_confirmed_stop() {
    use std::thread;

    // 1. Stop completion callback returned an error -> `wait_for_stream_stopped` MUST return false,
    // so `wait_for_recording_started` preserves staging by returning Ok(()) for handoff.
    let err_stop_sig = Arc::new((
        Mutex::new(DelegateSignal {
            start_completed: true,
            stream_stopped: true,
            stream_stop_error: Some("SCStream stop error".to_string()),
            ..Default::default()
        }),
        Condvar::new(),
    ));
    assert!(!delegate::wait_for_stream_stopped(
        &err_stop_sig,
        Duration::from_millis(50)
    ));
    let sig_check = Arc::clone(&err_stop_sig);
    let res = wait_for_recording_started(
        &err_stop_sig,
        Duration::from_secs(1),
        || Some("interrupted: screen locked".to_string()),
        || delegate::wait_for_stream_stopped(&sig_check, Duration::from_millis(50)),
    );
    assert!(
        res.is_ok(),
        "stop error must not be treated as confirmed stop; staging must be handed off"
    );

    // 2. Pre-start `-3808` (`stream_stopped=true, start_completed=false`) followed by delayed
    // `startCapture` whose forced post-start stop FAILS -> must return false & HandOffToStop!
    let (tx, rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let late_sig = Arc::new((
        Mutex::new(DelegateSignal {
            start_completed: false,
            stream_stopped: true, // pre-start -3808
            ..Default::default()
        }),
        Condvar::new(),
    ));
    let cancelled = Arc::new(AtomicBool::new(false));
    let sig_bg = Arc::clone(&late_sig);
    let cancel_bg = Arc::clone(&cancelled);
    let h = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        handle_start_callback_result(Ok(()), &sig_bg, &cancel_bg, &tx, |late_gen| {
            assert!(delegate::record_stop_callback_for_gen(
                &sig_bg,
                late_gen,
                Some("late forced stop failed".to_string()),
            ));
        });
    });

    let sig_wait = Arc::clone(&late_sig);
    let handoff = wait_for_capture_start_with_interrupt(
        &rx,
        &late_sig,
        Duration::from_secs(2),
        &cancelled,
        || Some("interrupted: screen locked".to_string()),
        || false,
        || delegate::wait_for_stream_stopped(&sig_wait, Duration::from_secs(2)),
    );
    h.join().expect("join late start thread");
    assert_eq!(
        handoff,
        Ok(StartWaitOutcome::HandOffToStop),
        "failed late-start stop must hand off staging instead of dropping it"
    );
}

#[test]
fn unconfirmed_stop_handoff_retains_staging_on_disk_after_finish_timeout() {
    let dir = std::env::temp_dir().join(format!(
        "qsr-unconfirmed-handoff-{}",
        crate::fs_safety::random_hex_token()
    ));
    std::fs::create_dir_all(&dir).expect("create temp test dir");
    let target = dir.join("capture.mp4");
    let mut staging = ExclusiveStagingDir::create_for_recording(&target, "mp4").expect("staging");
    let staged_path = staging.staged_file().to_path_buf();
    let stage_dir = staged_path.parent().expect("stage dir").to_path_buf();
    std::fs::write(&staged_path, b"in-progress-unflushed-bytes").expect("write staged bytes");

    // Simulate startup handoff on unconfirmed stop followed by finish() timing out with
    // `finished == false` and `stop_issue == Some(...)`.
    let sig = Arc::new((Mutex::new(DelegateSignal::default()), Condvar::new()));
    let handoff = wait_for_recording_started(
        &sig,
        Duration::from_secs(1),
        || Some("interrupted: screen locked".to_string()),
        || false,
    );
    assert!(handoff.is_ok());

    let finish_res = finalize_stop_staging_outcome(
        &mut staging,
        target.clone(),
        false,
        None,
        Some("Timed out confirming capture stream stopped".to_string()),
        false,
        Ok(dir.join("Recovered")),
    );
    assert!(
        finish_res.is_err(),
        "unconfirmed stop timeout must report Err"
    );
    let err_msg = finish_res.unwrap_err();
    assert!(err_msg.contains("retained untouched"), "err={err_msg}");

    // Dropping `staging` MUST NOT delete the file or directory when stop was unconfirmed!
    drop(staging);
    assert!(
        staged_path.exists(),
        "staged file must survive drop when stream stop was unconfirmed"
    );
    assert!(
        stage_dir.join(".qsr-keep").exists(),
        ".qsr-keep marker must exempt retained staging from cache pruning"
    );
    assert_eq!(
        std::fs::read(&staged_path).expect("read retained bytes"),
        b"in-progress-unflushed-bytes"
    );

    let _ = std::fs::remove_dir_all(&stage_dir);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stale_pre_start_stop_callback_cannot_complete_post_start_forced_stop() {
    let (tx, _rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let signal = Arc::new((Mutex::new(DelegateSignal::default()), Condvar::new()));
    let cancelled = AtomicBool::new(true);

    // 1. Pre-start stop request allocates generation 1 (callback hasn't returned yet).
    let pre_start_gen = delegate::begin_stop_generation(&signal);

    // 2. Delayed `startCapture` completes Ok(()) -> bumps generation to `late_gen` (> pre_start_gen)
    //    and initiates post-start forced stop (still in flight, NOT completed yet).
    let mut captured_late_gen = 0_u64;
    handle_start_callback_result(Ok(()), &signal, &cancelled, &tx, |late_gen| {
        captured_late_gen = late_gen;
    });
    assert!(captured_late_gen > pre_start_gen);

    // 3. Reversed arrival order: stale pre-start stop callback returns `-3808` (Ok/None) AFTER
    //    `start_completed` flipped -> MUST be ignored so forced stop is NOT marked complete!
    assert!(
        !delegate::record_stop_callback_for_gen(&signal, pre_start_gen, None),
        "stale pre-start stop callback must be rejected by generation check"
    );
    assert!(
        !delegate::wait_for_stream_stopped(&signal, Duration::from_millis(30)),
        "stale pre-start -3808 must never satisfy wait_for_stream_stopped while late stop is pending"
    );

    // 4. Only the matching `late_gen` callback can complete (and error still yields false).
    assert!(delegate::record_stop_callback_for_gen(
        &signal,
        captured_late_gen,
        Some("forced stop failed".to_string()),
    ));
    assert!(!delegate::wait_for_stream_stopped(
        &signal,
        Duration::from_millis(20)
    ));

    // 5. Companion check: a clean `late_gen` callback DOES confirm cleanly stopped.
    let gen3 = delegate::begin_stop_generation(&signal);
    assert!(!delegate::record_stop_callback_for_gen(
        &signal,
        pre_start_gen,
        None
    ));
    assert!(delegate::record_stop_callback_for_gen(&signal, gen3, None));
    assert!(delegate::wait_for_stream_stopped(
        &signal,
        Duration::from_millis(20)
    ));
}

#[test]
fn delayed_finish_timeout_retains_staging_even_when_stream_stop_confirmed() {
    let base = std::env::temp_dir().join(format!(
        "qsr-delayed-fin-{}",
        crate::fs_safety::random_hex_token()
    ));
    let cache_dir = base.join("cache");
    let dest_dir = base.join("dest");
    std::fs::create_dir_all(&cache_dir).expect("mkdir cache");
    std::fs::create_dir_all(&dest_dir).expect("mkdir dest");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o700))
        .expect("chmod cache");
    let target = dest_dir.join("delayed.mp4");

    let mut staging =
        ExclusiveStagingDir::create_in_cache_root(&cache_dir, &target, "mp4", false, true)
            .expect("stg");
    let staged_path = staging.staged_file().to_path_buf();
    std::fs::write(&staged_path, b"unflushed-moov-pending").expect("write partial");

    // Stream stop IS confirmed (`stop_issue == None`), yet `finished == false` timed out before
    // macOS finished flushing the MP4 trailer -> staging MUST be retained untouched on disk!
    let res = finalize_stop_staging_outcome(
        &mut staging,
        target,
        false,
        None,
        None,
        false,
        Ok(base.join("Recovered")),
    );
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("retained untouched"));
    drop(staging);

    // Simulate delayed trailer flush completing after drop, then verify pruning does not delete it.
    std::fs::write(&staged_path, b"delayed-trailer-flushed-mp4").expect("delayed flush write");
    crate::fs_safety::prune_stale_cache_files_in(&cache_dir, Duration::ZERO);
    assert_eq!(
        std::fs::read(&staged_path).expect("delayed flush survives drop and prune"),
        b"delayed-trailer-flushed-mp4"
    );
    let _ = std::fs::remove_dir_all(&base);
}
