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

    let cancel_cb = Arc::clone(&cancelled);
    let late_cb = Arc::clone(&late_cleanups);
    let delayed_start_cb = thread::spawn(move || {
        thread::sleep(Duration::from_millis(90));
        handle_start_callback_result(Ok(()), &cancel_cb, &tx, || {
            late_cb.fetch_add(1, Ordering::SeqCst);
        });
    });

    let started = Instant::now();
    let wait_res = wait_for_capture_start_with_interrupt(
        &rx,
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
        wait_for_recording_started(&err_signal, Duration::from_secs(1), || None, || {});
    assert!(res_err_after_start.is_ok());

    // Case 3: Writer has NOT started when interrupted -> returns Err immediately (no footage lost).
    let unstarted = Arc::new((Mutex::new(DelegateSignal::default()), Condvar::new()));
    let unstarted_stops = AtomicUsize::new(0);
    let res_unstarted = wait_for_recording_started(
        &unstarted,
        Duration::from_secs(1),
        || Some("interrupted: screen locked".to_string()),
        || {
            unstarted_stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        },
    );
    assert!(res_unstarted.is_err());
    assert_eq!(unstarted_stops.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Case 4: `wait_for_capture_start_with_interrupt` preserves Ok(()) if writer already started.
    let (_tx, rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let cancelled = AtomicBool::new(false);
    let late_stops = AtomicUsize::new(0);
    let res_capture_wait = wait_for_capture_start_with_interrupt(
        &rx,
        Duration::from_secs(1),
        &cancelled,
        || Some("interrupted: screen locked".to_string()),
        || true,
        || {
            late_stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        },
    );
    assert!(res_capture_wait.is_ok());
    assert!(!cancelled.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(late_stops.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn writer_starting_mid_stop_still_preserves_session_for_recovery() {
    use std::sync::atomic::Ordering;

    // Case 1: `wait_for_recording_started` sees `started == false`, invokes stop closure, and
    // `did_start` arrives mid-stop -> must re-check `started` after stop and return Ok(()).
    let signal = Arc::new((Mutex::new(DelegateSignal::default()), Condvar::new()));
    let sig_in_stop = Arc::clone(&signal);
    let res = wait_for_recording_started(
        &signal,
        Duration::from_secs(1),
        || Some("interrupted: screen locked".to_string()),
        move || {
            sig_in_stop.0.lock().expect("lock").started = true;
        },
    );
    assert!(
        res.is_ok(),
        "writer starting during stop teardown must return Ok so staging is preserved"
    );

    // Case 2: `wait_for_capture_start_with_interrupt` sees `started == false`, invokes stop
    // closure, and `did_start` arrives mid-stop -> must re-check after stop and return Ok(()).
    let (_tx, rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let cancelled = AtomicBool::new(false);
    let started_flag = Arc::new(AtomicBool::new(false));
    let started_read = Arc::clone(&started_flag);
    let started_in_stop = Arc::clone(&started_flag);
    let res_start_wait = wait_for_capture_start_with_interrupt(
        &rx,
        Duration::from_secs(1),
        &cancelled,
        || Some("interrupted: screen locked".to_string()),
        move || started_read.load(Ordering::Acquire),
        move || {
            started_in_stop.store(true, Ordering::Release);
        },
    );
    assert!(
        res_start_wait.is_ok(),
        "writer starting during capture-start stop teardown must return Ok so staging is preserved"
    );
}
