use super::*;

#[test]
fn stop_poll_state_machine_handles_ordering_fallback_and_timeout() {
    // Before 2s with neither finished nor stopped: stays Pending, no fallback yet
    assert_eq!(
        evaluate_stop_poll(false, false, false, Duration::from_millis(500)),
        (StopPollState::Pending, false)
    );
    // Normal path: recording output finishes first -> triggers stopCapture, waits for stream_stopped
    assert_eq!(
        evaluate_stop_poll(true, false, false, Duration::from_millis(120)),
        (StopPollState::Pending, true)
    );
    assert_eq!(
        evaluate_stop_poll(true, true, true, Duration::from_millis(180)),
        (StopPollState::Finished, false)
    );
    // Fallback path: if recording output hasn't finished at 2s, trigger stopCapture
    assert_eq!(
        evaluate_stop_poll(false, false, false, STOP_FALLBACK_AFTER),
        (StopPollState::Pending, true)
    );
    // Reverse arrival order: stream_stopped arrives before recordingOutputDidFinishRecording
    assert_eq!(
        evaluate_stop_poll(false, true, true, Duration::from_secs(3)),
        (StopPollState::Pending, false)
    );
    assert_eq!(
        evaluate_stop_poll(true, true, true, Duration::from_secs(4)),
        (StopPollState::Finished, false)
    );
    // Timeout if either recording output or stream stop never arrives by 12s
    assert_eq!(
        evaluate_stop_poll(false, true, true, STOP_TIMEOUT),
        (StopPollState::TimedOut, false)
    );
    assert_eq!(
        evaluate_stop_poll(true, false, true, STOP_TIMEOUT),
        (StopPollState::TimedOut, false)
    );
}
