use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::capture::recorder::{
    STOP_FALLBACK_AFTER, STOP_STREAM_CONFIRM_TIMEOUT, STOP_TIMEOUT, StopPollState,
};
use crate::export::avfoundation::{extract_rgba_frame, probe_video_metadata};
use crate::fs_safety::{ExclusiveStagingDir, format_tilde_path};
use crate::staging::{
    commit_or_recover_default, commit_or_recover_salvaged_default, preserve_staged_permanently,
};

/// Pure transition table for [`crate::capture::recorder::PendingRecordingStop::poll`].
/// Returns `(next_poll_state, should_send_stop_capture)`.
pub(super) fn evaluate_stop_poll(
    finished: bool,
    stream_stopped: bool,
    stop_sent_elapsed: Option<Duration>,
    total_elapsed: Duration,
) -> (StopPollState, bool) {
    if finished && stream_stopped {
        return (StopPollState::Finished, stop_sent_elapsed.is_none());
    }
    if stop_sent_elapsed.is_none() && (finished || total_elapsed >= STOP_FALLBACK_AFTER) {
        return (StopPollState::Pending, true);
    }
    let timed_out = match stop_sent_elapsed {
        Some(sent_for) if finished => sent_for >= STOP_STREAM_CONFIRM_TIMEOUT,
        Some(sent_for) => total_elapsed >= STOP_TIMEOUT && sent_for >= STOP_STREAM_CONFIRM_TIMEOUT,
        None => false,
    };
    let state = if timed_out {
        StopPollState::TimedOut
    } else {
        StopPollState::Pending
    };
    (state, false)
}

/// Strictly verifies that `path` is a non-empty playable video file with at least one playable
/// video track and a decodable initial frame (rejecting non-video files and truncated MP4s).
pub fn is_valid_salvageable_mp4(path: &Path) -> bool {
    let Ok(meta_fs) = std::fs::metadata(path) else {
        return false;
    };
    if !meta_fs.is_file() || meta_fs.len() == 0 {
        return false;
    }
    let Ok(meta) = probe_video_metadata(path) else {
        return false;
    };
    meta.has_video
        && meta.is_playable
        && meta.duration_sec >= 0.05
        && extract_rgba_frame(path, 0.0, Some(64)).is_ok()
}

fn format_stop_error_with_preservation(
    base_err: &str,
    staging: &mut ExclusiveStagingDir,
    recovery_dir: Result<PathBuf, String>,
) -> String {
    if is_valid_salvageable_mp4(staging.staged_file()) {
        return match preserve_staged_permanently(staging, recovery_dir) {
            Ok(saved_path) => format!(
                "{base_err}. Playable footage was preserved at {}",
                format_tilde_path(&saved_path)
            ),
            Err((rec_err, kept_path)) => format!(
                "{base_err} (recovery failed: {rec_err}). Playable footage was preserved in staging at {}",
                format_tilde_path(&kept_path)
            ),
        };
    }
    // Stream stop was not cleanly confirmed (or writer never flushed yet): keep the staging
    // directory untouched in place with `.qsr-keep` so `ExclusiveStagingDir::drop` never deletes
    // a file while a delayed writer could still be opening or flushing it.
    let retained = staging.retain_staged_in_place();
    format!(
        "{base_err}. Staging path retained untouched at {}",
        format_tilde_path(&retained)
    )
}

/// Resolves staging ownership at the end of `PendingRecordingStop::finish`:
/// - Whenever `stop_issue` is `Some` (stream stop failed, timed out, or start never settled),
///   `staging` is NEVER deleted on drop: playable MP4s move to permanent recovery, and incomplete
///   or unconfirmed staging paths are retained in place via `.qsr-keep`.
/// - When stream stop IS cleanly confirmed, salvages valid MP4s without overwriting existing files,
///   or commits normally on clean completion.
pub(super) fn finalize_stop_staging_outcome(
    staging: &mut ExclusiveStagingDir,
    output_path: PathBuf,
    finished: bool,
    finish_err: Option<String>,
    stop_issue: Option<String>,
    recovery_dir: Result<PathBuf, String>,
) -> Result<(PathBuf, u64, Option<String>), String> {
    if let Some(stop_err) = stop_issue {
        let base_err = match (finish_err, finished) {
            (Some(err), _) => format!("Recording finalization error: {err}; {stop_err}"),
            (None, false) => format!(
                "Timed out waiting for MP4 recording output to flush container trailer; {stop_err}"
            ),
            (None, true) => stop_err,
        };
        return Err(format_stop_error_with_preservation(
            &base_err,
            staging,
            recovery_dir,
        ));
    }

    let mut salvaged_warn: Option<String> = None;
    if let Some(err) = finish_err {
        if is_valid_salvageable_mp4(staging.staged_file()) {
            salvaged_warn = Some(format!("Recording ended early: {err}"));
        } else {
            return Err(format!("Recording finalization error: {err}"));
        }
    } else if !finished {
        if is_valid_salvageable_mp4(staging.staged_file()) {
            salvaged_warn = Some(
                "Timed out waiting for recording finish callback; salvaged playable footage"
                    .to_string(),
            );
        } else {
            return Err(
                "Timed out waiting for MP4 recording output to flush container trailer".to_string(),
            );
        }
    }

    if salvaged_warn.is_some() {
        commit_or_recover_salvaged_default(staging, output_path, salvaged_warn)
    } else {
        commit_or_recover_default(staging, output_path, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_poll_state_machine_handles_ordering_fallback_delay_and_timeout() {
        assert_eq!(
            evaluate_stop_poll(false, false, None, Duration::from_millis(500)),
            (StopPollState::Pending, false)
        );
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
        assert_eq!(
            evaluate_stop_poll(false, false, None, STOP_FALLBACK_AFTER),
            (StopPollState::Pending, true)
        );
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
}
