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
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(1);

    // Simulate caller timing out: receiver dropped before worker finishes Frame 0 init.
    drop(init_rx);

    let mut captures = 0_u32;
    let res = run_timelapse_loop(
        Duration::from_millis(5),
        &control,
        &frames_written,
        Instant::now(),
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
        "worker must never enter interval loop after handoff fails"
    );

    // Also verify pre-cancelled control aborts before capturing even Frame 0.
    signal_stop(&control);
    let (init_tx2, _init_rx2) = mpsc::sync_channel::<Result<(), String>>(1);
    let mut captures2 = 0_u32;
    let res2 = run_timelapse_loop(
        Duration::from_millis(5),
        &control,
        &frames_written,
        Instant::now(),
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
fn consecutive_capture_failures_abort_timelapse_instead_of_saving_incomplete_clip() {
    let control = Arc::new((Mutex::new(TimelapseControl::default()), Condvar::new()));
    let frames_written = AtomicU64::new(0);
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(1);

    let mut calls = 0_u32;
    let res = run_timelapse_loop(
        Duration::from_millis(2),
        &control,
        &frames_written,
        Instant::now(),
        init_tx,
        || {
            calls += 1;
            if calls == 1 {
                Ok(dummy_frame())
            } else {
                Err("target window closed".to_string())
            }
        },
        |_| Ok(()),
    );
    assert_eq!(init_rx.try_recv(), Ok(Ok(())));
    let err = res.expect_err("3 consecutive failures must fail the timelapse");
    assert!(err.contains("3 consecutive capture failures"), "got: {err}");
    assert!(err.contains("target window closed"), "got: {err}");
    assert_eq!(calls, 1 + MAX_CONSECUTIVE_CAPTURE_FAILURES);
}

#[test]
fn stop_timeout_exceeds_writer_finish_and_recovers_delayed_finalization() {
    assert!(
        TIMELAPSE_STOP_TIMEOUT > WRITER_FINISH_TIMEOUT,
        "stop wait must exceed AVAssetWriter finish timeout so slow finalization is not abandoned"
    );

    let control = Arc::new((Mutex::new(TimelapseControl::default()), Condvar::new()));
    let (done_tx, done_rx) = mpsc::sync_channel::<Result<PathBuf, String>>(1);
    let mut pending = PendingTimelapseStop {
        control,
        done_rx,
        result: None,
        stop_initiated_at: Instant::now(),
        timeout: Duration::from_secs(2),
    };

    // Still finalizing -> Pending
    assert_eq!(pending.poll(), StopPollState::Pending);

    // Delayed completion arrives before timeout -> Finished & recovers exact output path
    let expected = PathBuf::from("/tmp/timelapse-final.mp4");
    done_tx.send(Ok(expected.clone())).expect("send result");
    assert_eq!(pending.poll(), StopPollState::Finished);
    assert_eq!(pending.finish(), Ok(expected));
}
