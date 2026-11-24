use std::sync::Mutex;

use super::*;
use crate::capture::timelapse_loop::MAX_CONSECUTIVE_CAPTURE_FAILURES;
use crate::export::avfoundation::RgbaFrame;

fn dummy_frame(tag: u8) -> RgbaFrame {
    RgbaFrame {
        width: 4,
        height: 4,
        pixels: vec![tag; 4 * 4 * 4],
    }
}

#[test]
fn delayed_init_after_caller_timeout_cancels_without_continuing_capture() {
    let sync = SyncState::new();
    let frames_written = AtomicU64::new(0);
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(0);

    // Caller timed out and dropped init_rx before worker finished Frame 0.
    drop(init_rx);

    let mut dispatches = 0_u32;
    let res = run_timelapse_loop(
        Duration::from_millis(5),
        &sync,
        &frames_written,
        init_tx,
        || {
            dispatches += 1;
            dispatches
        },
        |_| Ok(dummy_frame(1)),
        |_| Ok(()),
    );
    assert_eq!(res, Ok(LoopExit::CancelledBeforeStart));
    assert_eq!(
        dispatches, 1,
        "worker must never enter interval loop after rendezvous handoff fails"
    );

    // Pre-cancelled control aborts before dispatching even Frame 0.
    sync.signal_start_cancelled();
    let (init_tx2, _init_rx2) = mpsc::sync_channel::<Result<(), String>>(0);
    let mut dispatches2 = 0_u32;
    let res2 = run_timelapse_loop(
        Duration::from_millis(5),
        &sync,
        &frames_written,
        init_tx2,
        || {
            dispatches2 += 1;
            dispatches2
        },
        |_| Ok(dummy_frame(2)),
        |_| Ok(()),
    );
    assert_eq!(res2, Ok(LoopExit::CancelledBeforeStart));
    assert_eq!(dispatches2, 0);
}

#[test]
fn start_cancel_during_inflight_frame0_discards_frame_without_writing() {
    let sync = SyncState::new();
    let sync_worker = Arc::clone(&sync);
    let frames_written = Arc::new(AtomicU64::new(0));
    let frames_worker = Arc::clone(&frames_written);
    let writes = Arc::new(AtomicU64::new(0));
    let writes_worker = Arc::clone(&writes);

    let (dispatched_tx, dispatched_rx) = mpsc::sync_channel::<()>(1);
    let (release_tx, release_rx) = mpsc::sync_channel::<()>(1);
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(0);

    let handle = thread::spawn(move || {
        run_timelapse_loop(
            Duration::from_secs(60),
            &sync_worker,
            &frames_worker,
            init_tx,
            || {
                let _ = dispatched_tx.send(());
            },
            |()| {
                let _ = release_rx.recv();
                Ok(dummy_frame(99))
            },
            |_| {
                writes_worker.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
    });

    // Wait until Frame 0 is in flight, then cancel start (simulating start timeout) before it finishes.
    dispatched_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("frame 0 dispatched");
    drop(init_rx);
    sync.signal_start_cancelled();
    release_tx.send(()).expect("release inflight frame 0");

    let exit = handle.join().expect("join worker");
    assert_eq!(exit, Ok(LoopExit::CancelledBeforeStart));
    assert_eq!(
        writes.load(Ordering::SeqCst),
        0,
        "frame 0 completed after start_cancelled must be discarded without writing"
    );
}

#[test]
fn stop_immediately_after_start_and_never_captures_after_stop_requested() {
    let sync = SyncState::new();
    let sync_worker = Arc::clone(&sync);
    let frames_written = Arc::new(AtomicU64::new(0));
    let frames_worker = Arc::clone(&frames_written);
    let dispatches = Arc::new(AtomicU64::new(0));
    let dispatches_worker = Arc::clone(&dispatches);

    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(0);
    let handle = thread::spawn(move || {
        run_timelapse_loop(
            Duration::from_secs(60),
            &sync_worker,
            &frames_worker,
            init_tx,
            || dispatches_worker.fetch_add(1, Ordering::SeqCst) + 1,
            |_| Ok(dummy_frame(1)),
            |_| Ok(()),
        )
    });

    assert_eq!(init_rx.recv_timeout(Duration::from_secs(2)), Ok(Ok(())));
    sync.signal_stop();

    let exit = handle.join().expect("join worker");
    assert_eq!(exit, Ok(LoopExit::Stopped));
    assert_eq!(
        dispatches.load(Ordering::SeqCst),
        1,
        "must never dispatch a capture after stop was requested"
    );
    assert_eq!(
        frames_written.load(Ordering::Relaxed),
        2,
        "single pre-stop frame is duplicated once so MP4 has valid >=2 frame duration"
    );
}

#[test]
fn stop_during_inflight_interval_capture_discards_returned_frame() {
    let sync = SyncState::new();
    let sync_worker = Arc::clone(&sync);
    let frames_written = Arc::new(AtomicU64::new(0));
    let frames_worker = Arc::clone(&frames_written);
    let written_tags = Arc::new(Mutex::new(Vec::<u8>::new()));
    let written_worker = Arc::clone(&written_tags);

    let (second_dispatched_tx, second_dispatched_rx) = mpsc::sync_channel::<()>(1);
    let (release_second_tx, release_second_rx) = mpsc::sync_channel::<()>(1);
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(0);

    let handle = thread::spawn(move || {
        let mut seq = 0_u8;
        run_timelapse_loop(
            Duration::from_millis(5),
            &sync_worker,
            &frames_worker,
            init_tx,
            || {
                seq += 1;
                if seq == 2 {
                    let _ = second_dispatched_tx.send(());
                }
                seq
            },
            |n| {
                if n == 2 {
                    let _ = release_second_rx.recv();
                }
                Ok(dummy_frame(n))
            },
            |frame| {
                written_worker.lock().unwrap().push(frame.pixels[0]);
                Ok(())
            },
        )
    });

    assert_eq!(init_rx.recv_timeout(Duration::from_secs(2)), Ok(Ok(())));
    // Wait until tick #2 has dispatched and is in flight, then signal stop before tick #2 completes.
    second_dispatched_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("second capture dispatched");
    sync.signal_stop();
    release_second_tx.send(()).expect("release second capture");

    let exit = handle.join().expect("join worker");
    assert_eq!(exit, Ok(LoopExit::Stopped));
    let tags = written_tags.lock().unwrap().clone();
    assert_eq!(
        tags,
        vec![1, 1],
        "in-flight tick #2 returned after stop must be discarded (only pre-stop frame #1 written)"
    );
}

#[test]
fn stop_serialized_before_tick_expiry_prevents_dispatching_second_capture() {
    let sync = SyncState::new();
    let sync_worker = Arc::clone(&sync);
    let frames_written = Arc::new(AtomicU64::new(0));
    let frames_worker = Arc::clone(&frames_written);
    let dispatches = Arc::new(AtomicU64::new(0));
    let dispatches_worker = Arc::clone(&dispatches);

    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(0);
    let handle = thread::spawn(move || {
        run_timelapse_loop(
            Duration::from_millis(25),
            &sync_worker,
            &frames_worker,
            init_tx,
            || dispatches_worker.fetch_add(1, Ordering::SeqCst) + 1,
            |_| Ok(dummy_frame(7)),
            |_| Ok(()),
        )
    });

    assert_eq!(init_rx.recv_timeout(Duration::from_secs(2)), Ok(Ok(())));
    // Hold the control lock across the tick boundary and set stop_requested before releasing it:
    // proves the worker cannot dispatch a second capture once stop_requested becomes true.
    {
        let (lock, cvar) = &sync.control;
        let mut guard = lock.lock().unwrap();
        thread::sleep(Duration::from_millis(35));
        guard.stop_requested = true;
        sync.cancelled.store(true, Ordering::Release);
        cvar.notify_all();
    }

    let exit = handle.join().expect("join worker");
    assert_eq!(exit, Ok(LoopExit::Stopped));
    assert_eq!(
        dispatches.load(Ordering::SeqCst),
        1,
        "serialized stop across tick expiry must prevent dispatching capture #2"
    );
}

#[test]
fn consecutive_capture_failures_preserve_partial_frames_and_report_error() {
    let sync = SyncState::new();
    let sync_worker = Arc::clone(&sync);
    let frames_written = Arc::new(AtomicU64::new(0));
    let frames_worker = Arc::clone(&frames_written);
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(0);

    let handle = thread::spawn(move || {
        let mut calls = 0_u32;
        let res = run_timelapse_loop(
            Duration::from_millis(2),
            &sync_worker,
            &frames_worker,
            init_tx,
            || {
                calls += 1;
                calls
            },
            |n| {
                if n <= 2 {
                    Ok(dummy_frame(n as u8))
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

    let sync = SyncState::new();
    let committing = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::sync_channel::<Result<RecordingOutcome, String>>(1);
    let mut pending = PendingTimelapseStop {
        sync,
        committing,
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

#[test]
fn slow_cross_volume_commit_does_not_time_out_after_encoder_finishes() {
    let sync = SyncState::new();
    let committing = Arc::new(AtomicBool::new(true));
    let (done_tx, done_rx) = mpsc::sync_channel::<Result<RecordingOutcome, String>>(1);
    let mut pending = PendingTimelapseStop {
        sync,
        committing: Arc::clone(&committing),
        done_rx,
        result: None,
        stop_initiated_at: Instant::now(),
        timeout: Duration::from_millis(15),
    };

    thread::sleep(Duration::from_millis(30));
    assert_eq!(
        pending.poll(),
        StopPollState::Pending,
        "active cross-volume commit must never time out at encoder finalization limit"
    );

    let expected = RecordingOutcome {
        output_path: PathBuf::from("/tmp/slow-usb-timelapse.mp4"),
        partial_error: None,
    };
    done_tx
        .send(Ok(expected.clone()))
        .expect("send commit done");
    assert_eq!(pending.poll(), StopPollState::Finished);
    assert_eq!(pending.finish(), Ok(expected));

    // Verify normal timeout still triggers when encoder is NOT yet in commit phase.
    let (_tx2, rx2) = mpsc::sync_channel::<Result<RecordingOutcome, String>>(1);
    let mut stuck = PendingTimelapseStop {
        sync: SyncState::new(),
        committing: Arc::new(AtomicBool::new(false)),
        done_rx: rx2,
        result: None,
        stop_initiated_at: Instant::now(),
        timeout: Duration::from_millis(15),
    };
    thread::sleep(Duration::from_millis(30));
    assert_eq!(stuck.poll(), StopPollState::TimedOut);
}

#[test]
fn single_frame_stop_propagates_closing_frame_write_error() {
    let sync = SyncState::new();
    let sync_worker = Arc::clone(&sync);
    let frames_written = Arc::new(AtomicU64::new(0));
    let frames_worker = Arc::clone(&frames_written);
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(0);

    let handle = thread::spawn(move || {
        let mut write_calls = 0_u32;
        run_timelapse_loop(
            Duration::from_secs(60),
            &sync_worker,
            &frames_worker,
            init_tx,
            || (),
            |()| Ok(dummy_frame(5)),
            |_| {
                write_calls += 1;
                if write_calls == 1 {
                    Ok(())
                } else {
                    Err("encoder input queue closed".to_string())
                }
            },
        )
    });

    assert_eq!(init_rx.recv_timeout(Duration::from_secs(2)), Ok(Ok(())));
    sync.signal_stop();

    match handle.join().expect("join worker") {
        Ok(LoopExit::AbortedAfterPartialCapture(err)) => {
            assert!(err.contains("closing frame"), "got: {err}");
            assert!(err.contains("encoder input queue closed"), "got: {err}");
        }
        other => panic!("expected AbortedAfterPartialCapture, got {other:?}"),
    }
    assert_eq!(frames_written.load(Ordering::Relaxed), 1);
}
