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
