use super::*;

#[test]
fn stop_poll_state_machine_handles_ordering_fallback_delay_and_timeout() {
    // Before 2s with neither finished nor stopped: stays Pending, no fallback yet
    assert_eq!(
        evaluate_stop_poll(false, false, None, Duration::from_millis(500)),
        (StopPollState::Pending, false)
    );
    // Normal path: recording output finishes first -> triggers stopCapture, waits for stream_stopped
    assert_eq!(
        evaluate_stop_poll(true, false, None, Duration::from_millis(120)),
        (StopPollState::Pending, true)
    );
    assert_eq!(
        evaluate_stop_poll(
            true,
            true,
            Some(Duration::from_millis(60)),
            Duration::from_millis(180)
        ),
        (StopPollState::Finished, false)
    );
    // Delayed first poll (> 12s, e.g. user held menu/modal): sends stopCapture and keeps waiting
    // instead of timing out immediately and discarding a finalized MP4.
    assert_eq!(
        evaluate_stop_poll(true, false, None, Duration::from_secs(15)),
        (StopPollState::Pending, true)
    );
    assert_eq!(
        evaluate_stop_poll(
            true,
            false,
            Some(Duration::from_millis(100)),
            Duration::from_secs(15)
        ),
        (StopPollState::Pending, false)
    );
    assert_eq!(
        evaluate_stop_poll(
            true,
            true,
            Some(Duration::from_millis(150)),
            Duration::from_secs(15)
        ),
        (StopPollState::Finished, false)
    );
    // Fallback path: if recording output hasn't finished at 2s, trigger stopCapture
    assert_eq!(
        evaluate_stop_poll(false, false, None, STOP_FALLBACK_AFTER),
        (StopPollState::Pending, true)
    );
    // Delayed fallback (> 12s while still unfinished): still grants confirm window after sending stop
    assert_eq!(
        evaluate_stop_poll(false, false, None, Duration::from_secs(14)),
        (StopPollState::Pending, true)
    );
    assert_eq!(
        evaluate_stop_poll(
            false,
            false,
            Some(Duration::from_secs(1)),
            Duration::from_secs(15)
        ),
        (StopPollState::Pending, false)
    );
    // Reverse arrival order: stream_stopped arrives before recordingOutputDidFinishRecording
    assert_eq!(
        evaluate_stop_poll(
            false,
            true,
            Some(Duration::from_secs(1)),
            Duration::from_secs(3)
        ),
        (StopPollState::Pending, false)
    );
    assert_eq!(
        evaluate_stop_poll(
            true,
            true,
            Some(Duration::from_secs(2)),
            Duration::from_secs(4)
        ),
        (StopPollState::Finished, false)
    );
    // Timeout once stream-stop confirmation window (and total timeout when unfinished) expire
    assert_eq!(
        evaluate_stop_poll(false, true, Some(STOP_STREAM_CONFIRM_TIMEOUT), STOP_TIMEOUT),
        (StopPollState::TimedOut, false)
    );
    assert_eq!(
        evaluate_stop_poll(
            true,
            false,
            Some(STOP_STREAM_CONFIRM_TIMEOUT),
            Duration::from_secs(6)
        ),
        (StopPollState::TimedOut, false)
    );
}

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
        handle_start_callback_result(Ok(()), &sig_cb, &cancel_cb, &tx, || {
            late_cb.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut g) = sig_cb.0.lock() {
                g.stream_stopped = true;
                g.stream_stop_error = None;
                sig_cb.1.notify_all();
            }
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
        handle_start_callback_result(Ok(()), &sig_bg, &cancel_bg, &tx, || {
            if let Ok(mut g) = sig_bg.0.lock() {
                g.stream_stopped = true;
                g.stream_stop_error = Some("late forced stop failed".to_string());
                sig_bg.1.notify_all();
            }
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
    let mut staging = ExclusiveStagingDir::create_for_target(&target, "mp4").expect("staging");
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
