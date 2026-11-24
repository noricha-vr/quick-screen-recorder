use super::*;

fn dummy_frame() -> RgbaFrame {
    RgbaFrame {
        width: 4,
        height: 4,
        pixels: vec![0_u8; 4 * 4 * 4],
    }
}

#[test]
fn delayed_init_after_caller_timeout_cancels_without_continuing_capture() {
    let control = Arc::new((Mutex::new(TimelapseControl::default()), Condvar::new()));
    let frames_written = AtomicU64::new(0);
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(0);

    // Caller timed out and dropped init_rx before worker finished Frame 0.
    drop(init_rx);

    let mut captures = 0_u32;
    let res = run_timelapse_loop(
        Duration::from_millis(5),
        &control,
        &frames_written,
        init_tx,
        || {
            captures += 1;
            Ok(dummy_frame())
        },
        |_| Ok(()),
    );
    assert_eq!(res, Ok(LoopExit::CancelledBeforeStart));
    assert_eq!(
        captures, 1,
        "worker must never enter interval loop after rendezvous handoff fails"
    );

    // Pre-cancelled control aborts before capturing even Frame 0.
    signal_start_cancelled(&control);
    let (init_tx2, _init_rx2) = mpsc::sync_channel::<Result<(), String>>(0);
    let mut captures2 = 0_u32;
    let res2 = run_timelapse_loop(
        Duration::from_millis(5),
        &control,
        &frames_written,
        init_tx2,
        || {
            captures2 += 1;
            Ok(dummy_frame())
        },
        |_| Ok(()),
    );
    assert_eq!(res2, Ok(LoopExit::CancelledBeforeStart));
    assert_eq!(captures2, 0);
}

#[test]
fn stop_immediately_after_start_and_never_captures_after_stop_requested() {
    let control = Arc::new((Mutex::new(TimelapseControl::default()), Condvar::new()));
    let control_worker = Arc::clone(&control);
    let frames_written = Arc::new(AtomicU64::new(0));
    let frames_worker = Arc::clone(&frames_written);
    let captures = Arc::new(AtomicU64::new(0));
    let captures_worker = Arc::clone(&captures);

    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(0);
    let handle = thread::spawn(move || {
        run_timelapse_loop(
            Duration::from_secs(60),
            &control_worker,
            &frames_worker,
            init_tx,
            || {
                captures_worker.fetch_add(1, Ordering::SeqCst);
                Ok(dummy_frame())
            },
            |_| Ok(()),
        )
    });

    // Receive startup success, then immediately request stop (exercises post-start stop race
    // AND verifies zero post-stop screen captures even when only Frame 0 existed).
    assert_eq!(init_rx.recv_timeout(Duration::from_secs(2)), Ok(Ok(())));
    signal_stop(&control);

    let exit = handle.join().expect("join worker");
    assert_eq!(
        exit,
        Ok(LoopExit::Stopped),
        "post-start stop must never be misclassified as CancelledBeforeStart"
    );
    assert_eq!(
        captures.load(Ordering::SeqCst),
        1,
        "must never call capture_frame after stop was requested"
    );
    assert_eq!(
        frames_written.load(Ordering::Relaxed),
        2,
        "single pre-stop frame is duplicated once so MP4 has valid >=2 frame duration"
    );
}

#[test]
fn consecutive_capture_failures_preserve_partial_frames_and_report_error() {
    let control = Arc::new((Mutex::new(TimelapseControl::default()), Condvar::new()));
    let control_worker = Arc::clone(&control);
    let frames_written = Arc::new(AtomicU64::new(0));
    let frames_worker = Arc::clone(&frames_written);
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(0);

    let handle = thread::spawn(move || {
        let mut calls = 0_u32;
        let res = run_timelapse_loop(
            Duration::from_millis(2),
            &control_worker,
            &frames_worker,
            init_tx,
            || {
                calls += 1;
                if calls <= 2 {
                    Ok(dummy_frame())
                } else {
                    Err("target window closed".to_string())
                }
            },
            |_| Ok(()),
        );
        (res, calls)
    });

    assert_eq!(init_rx.recv_timeout(Duration::from_secs(2)), Ok(Ok(())));
    let (res, calls) = handle.join().expect("join worker");
    match res {
        Ok(LoopExit::AbortedAfterPartialCapture(err)) => {
            assert!(err.contains("3 consecutive capture failures"), "got: {err}");
            assert!(err.contains("2 frames captured"), "got: {err}");
            assert!(err.contains("target window closed"), "got: {err}");
        }
        other => panic!("expected AbortedAfterPartialCapture, got {other:?}"),
    }
    assert_eq!(calls, 2 + MAX_CONSECUTIVE_CAPTURE_FAILURES);
    assert_eq!(frames_written.load(Ordering::Relaxed), 2);
}

#[test]
fn stop_timeout_exceeds_writer_finish_and_recovers_delayed_finalization() {
    assert!(
        TIMELAPSE_STOP_TIMEOUT > WRITER_FINISH_TIMEOUT,
        "stop wait must exceed AVAssetWriter finish timeout so slow finalization is not abandoned"
    );

    let control = Arc::new((Mutex::new(TimelapseControl::default()), Condvar::new()));
    let (done_tx, done_rx) = mpsc::sync_channel::<Result<RecordingOutcome, String>>(1);
    let mut pending = PendingTimelapseStop {
        control,
        done_rx,
        result: None,
        stop_initiated_at: Instant::now(),
        timeout: Duration::from_secs(2),
    };

    assert_eq!(pending.poll(), StopPollState::Pending);

    let expected = RecordingOutcome {
        output_path: PathBuf::from("/tmp/timelapse-final.mp4"),
        partial_error: None,
    };
    done_tx.send(Ok(expected.clone())).expect("send result");
    assert_eq!(pending.poll(), StopPollState::Finished);
    assert_eq!(pending.finish(), Ok(expected));
}
