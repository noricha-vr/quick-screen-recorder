use std::path::{Path, PathBuf};
use std::time::Instant;

use super::stop::{RemovePhase, STOP_STREAM_CONFIRM_TIMEOUT};
use crate::capture::recorder::StopPollState;
use crate::export::avfoundation::{extract_rgba_frame, probe_video_metadata};
use crate::fs_safety::{ExclusiveStagingDir, format_tilde_path};
use crate::staging::{
    commit_or_recover_default, commit_or_recover_salvaged_default, preserve_staged_permanently,
};

/// Pure transition table for [`crate::capture::recorder::PendingRecordingStop::poll`].
///
/// Never triggers `stopCapture` prematurely while `SCRecordingOutput` is finalizing/mixing audio;
/// the serial stop worker owns all `removeRecordingOutput` / `stopCapture` transitions and shares
/// `stop_capture_sent_at` and `finalize_deadline` with GUI/CLI polling.
pub(super) fn evaluate_stop_poll(
    now: Instant,
    finished: bool,
    stream_settled: bool,
    remove_phase: RemovePhase,
    stop_capture_sent_at: Option<Instant>,
    finalize_deadline: Instant,
) -> StopPollState {
    if finished && stream_settled {
        return StopPollState::Finished;
    }
    if let Some(sent_at) = stop_capture_sent_at {
        let sent_for = now.saturating_duration_since(sent_at);
        if sent_for < STOP_STREAM_CONFIRM_TIMEOUT {
            return StopPollState::Pending;
        }
        if finished || remove_phase == RemovePhase::Failed || now >= finalize_deadline {
            return StopPollState::TimedOut;
        }
        return StopPollState::Pending;
    }
    // Safety valve if `removeRecordingOutput_error` itself hangs indefinitely in `replayd`:
    if now >= finalize_deadline + STOP_STREAM_CONFIRM_TIMEOUT {
        return StopPollState::TimedOut;
    }
    StopPollState::Pending
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

fn retain_staging_with_error(base_err: &str, staging: &mut ExclusiveStagingDir) -> String {
    // Whenever stream stop is unconfirmed or finalization timed out / forced stop, `replayd`
    // (or `RPAudioMixUtility`) may still be flushing the staged file in the background.
    // Never delete or move the file out from under the writer: retain it untouched in place
    // with `.qsr-keep` so neither Drop nor 24h cache pruning deletes it.
    let retained = staging.retain_staged_in_place();
    format!(
        "{base_err}. Staging path retained untouched at {}",
        format_tilde_path(&retained)
    )
}

/// Preserves a completed staged MP4 directly into permanent recovery (never committing to a
/// prunable cache path). If recovery fails, retains the protected staging dir (`.qsr-keep = 1`,
/// plus `qsr-preserved-*` rename if possible) so even double failure survives 24h cache pruning.
pub(crate) fn commit_staged_to_recovery_directly(
    staging: &mut ExclusiveStagingDir,
    partial_error: Option<String>,
    recovery_dir: Result<PathBuf, String>,
) -> Result<(PathBuf, u64, Option<String>), String> {
    let size = std::fs::metadata(staging.staged_file())
        .map(|m| m.len())
        .unwrap_or(0);
    match preserve_staged_permanently(staging, recovery_dir) {
        Ok(saved_path) => Ok((saved_path, size, partial_error)),
        Err((rec_err, kept_path)) => {
            let warn = match partial_error {
                Some(p) => format!("{p}; recovery fallback: {rec_err}"),
                None => format!("recovery fallback: {rec_err}"),
            };
            Ok((kept_path, size, Some(warn)))
        }
    }
}

/// Resolves staging ownership at the end of `PendingRecordingStop::finish`:
/// - Whenever `stop_issue` is `Some` (stream stop failed, timed out, or start never settled),
///   `staging` is retained untouched in place via `.qsr-keep` (no synchronous AVFoundation decode
///   or rename while a writer/mixer could still be active).
/// - Whenever `forced_stop_on_timeout` is `true` and recording did not finish cleanly without
///   error, `staging` is ALSO retained untouched in place via `.qsr-keep` so background offline
///   mixing (`RPAudioMixUtility`) can finish writing safely without Drop deleting the file.
/// - Whenever `finished == false`, `staging` is retained untouched in place via `.qsr-keep`.
/// - When `preserve_directly` is `true` (GUI auto-stop / quit-save), commits cleanly finished
///   footage directly into permanent recovery without touching prunable cache.
#[allow(clippy::too_many_arguments)]
pub(super) fn finalize_stop_staging_outcome(
    staging: &mut ExclusiveStagingDir,
    output_path: PathBuf,
    finished: bool,
    finish_err: Option<String>,
    stop_issue: Option<String>,
    forced_stop_on_timeout: bool,
    preserve_directly: bool,
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
        return Err(retain_staging_with_error(&base_err, staging));
    }

    if forced_stop_on_timeout && (!finished || finish_err.is_some()) {
        let base_err = match finish_err {
            Some(err) => {
                format!("Timed out waiting for recording finalization before forcing stop ({err})")
            }
            None => {
                "Timed out waiting for MP4 recording output to flush container trailer".to_string()
            }
        };
        return Err(retain_staging_with_error(&base_err, staging));
    }

    if !finished {
        return Err(retain_staging_with_error(
            "Timed out waiting for MP4 recording output to flush container trailer",
            staging,
        ));
    }

    let mut salvaged_warn: Option<String> = None;
    if let Some(err) = finish_err {
        if is_valid_salvageable_mp4(staging.staged_file()) {
            salvaged_warn = Some(format!("Recording ended early: {err}"));
        } else {
            return Err(format!("Recording finalization error: {err}"));
        }
    }

    if preserve_directly {
        commit_staged_to_recovery_directly(staging, salvaged_warn, recovery_dir)
    } else if salvaged_warn.is_some() {
        commit_or_recover_salvaged_default(staging, output_path, salvaged_warn)
    } else {
        commit_or_recover_default(staging, output_path, None)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::stop::{
        STOP_DUAL_AUDIO_MAX, STOP_DUAL_AUDIO_MIN, STOP_SINGLE_AUDIO_MAX, STOP_SINGLE_AUDIO_MIN,
        compute_finalize_timeout,
    };
    use super::*;

    #[test]
    fn compute_finalize_timeout_scales_and_clamps_by_audio_mode() {
        assert_eq!(
            compute_finalize_timeout(5_000, false),
            STOP_SINGLE_AUDIO_MIN
        );
        assert_eq!(
            compute_finalize_timeout(120_000, false),
            Duration::from_secs(60)
        );
        assert_eq!(
            compute_finalize_timeout(u64::MAX, false),
            STOP_SINGLE_AUDIO_MAX
        );

        // Dual audio (`System + Mic`): 18.6s clip gets >= 60s; 2min gets 4min; huge clamps to 10min.
        assert_eq!(compute_finalize_timeout(18_600, true), STOP_DUAL_AUDIO_MIN);
        assert_eq!(
            compute_finalize_timeout(120_000, true),
            Duration::from_secs(240)
        );
        assert_eq!(
            compute_finalize_timeout(u64::MAX, true),
            STOP_DUAL_AUDIO_MAX
        );
    }

    #[test]
    fn stop_poll_waits_past_two_seconds_for_dual_audio_mixing_and_respects_confirm_window() {
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(60);

        // At 2s, 8s, 30s while mixer is running (`finished == false`, `stop_capture_sent_at == None`):
        // MUST remain Pending (never time out or abort prematurely).
        for secs in [2, 8, 30, 59] {
            assert_eq!(
                evaluate_stop_poll(
                    t0 + Duration::from_secs(secs),
                    false,
                    false,
                    RemovePhase::Succeeded,
                    None,
                    deadline,
                ),
                StopPollState::Pending
            );
        }

        // Once `finished == true` at 7.9s and worker sends `stopCapture`:
        // waits up to STOP_STREAM_CONFIRM_TIMEOUT (5s) for `stream_settled`.
        let sent_at = t0 + Duration::from_millis(7_900);
        assert_eq!(
            evaluate_stop_poll(
                sent_at + Duration::from_secs(2),
                true,
                false,
                RemovePhase::Succeeded,
                Some(sent_at),
                deadline,
            ),
            StopPollState::Pending
        );
        assert_eq!(
            evaluate_stop_poll(
                sent_at + Duration::from_millis(120),
                true,
                true,
                RemovePhase::Succeeded,
                Some(sent_at),
                deadline,
            ),
            StopPollState::Finished
        );
        assert_eq!(
            evaluate_stop_poll(
                sent_at + STOP_STREAM_CONFIRM_TIMEOUT,
                true,
                false,
                RemovePhase::Succeeded,
                Some(sent_at),
                deadline,
            ),
            StopPollState::TimedOut
        );

        // When `removeRecordingOutput` failed immediately, `stopCapture` confirm timeout expires
        // after 5s instead of waiting the full 60s finalize deadline.
        let fail_sent = t0 + Duration::from_millis(20);
        assert_eq!(
            evaluate_stop_poll(
                fail_sent + Duration::from_secs(2),
                false,
                true,
                RemovePhase::Failed,
                Some(fail_sent),
                deadline,
            ),
            StopPollState::Pending
        );
        assert_eq!(
            evaluate_stop_poll(
                fail_sent + STOP_STREAM_CONFIRM_TIMEOUT,
                false,
                true,
                RemovePhase::Failed,
                Some(fail_sent),
                deadline,
            ),
            StopPollState::TimedOut
        );

        // Safety valve: if `removeRecordingOutput` itself hangs beyond `deadline + 5s`, time out.
        assert_eq!(
            evaluate_stop_poll(
                deadline + STOP_STREAM_CONFIRM_TIMEOUT,
                false,
                false,
                RemovePhase::InFlight,
                None,
                deadline,
            ),
            StopPollState::TimedOut
        );
    }

    #[test]
    fn timelapse_stop_timeout_then_delayed_worker_completion_preserves_video() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::mpsc;
        use std::thread;

        use crate::capture::RecordingCodec;
        use crate::capture::timelapse::{
            PendingTimelapseStop, TimelapseWorkerDone, deliver_or_preserve_timelapse_outcome,
        };
        use crate::fs_safety::{prune_stale_cache_files_in, random_hex_token};

        let base = std::env::temp_dir().join(format!("qsr-tl-timeout-{}", random_hex_token()));
        let cache_dir = base.join("cache");
        let rec_dir = base.join("recovered");
        fs::create_dir_all(&cache_dir).expect("mkdir cache");
        fs::set_permissions(&cache_dir, fs::Permissions::from_mode(0o700)).expect("chmod cache");

        let target = cache_dir.join("capture-timeout.mp4");
        let staging =
            ExclusiveStagingDir::create_in_cache_root(&cache_dir, &target, "mp4", false, true)
                .expect("staging");
        fs::write(staging.staged_file(), b"hours-of-delayed-timelapse").expect("write mp4");
        let (done_tx, done_rx) = mpsc::sync_channel::<Result<TimelapseWorkerDone, String>>(1);
        let pending = PendingTimelapseStop::new_for_test(done_rx, Duration::from_millis(15));
        assert!(
            pending
                .finish_with_policy(true, Ok(rec_dir.clone()))
                .is_err()
        );

        deliver_or_preserve_timelapse_outcome(
            done_tx,
            Ok(TimelapseWorkerDone::ReadyToCommit {
                staging: Some(staging),
                output_path: target,
                partial_error: None,
                codec: RecordingCodec::H264,
                elapsed: Duration::from_secs(60),
            }),
            Ok(rec_dir.clone()),
        );
        let recovered_files: Vec<_> = fs::read_dir(&rec_dir)
            .expect("read rec_dir")
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(recovered_files.len(), 1);
        assert_eq!(
            fs::read(&recovered_files[0]).expect("read rescued timelapse"),
            b"hours-of-delayed-timelapse"
        );

        let target2 = cache_dir.join("capture-timeout2.mp4");
        let staging2 =
            ExclusiveStagingDir::create_in_cache_root(&cache_dir, &target2, "mp4", false, true)
                .expect("staging2");
        let staged_path2 = staging2.staged_file().to_path_buf();
        fs::write(&staged_path2, b"double-failure-delayed-timelapse").expect("write mp4 2");
        let (done_tx2, done_rx2) = mpsc::sync_channel::<Result<TimelapseWorkerDone, String>>(1);
        drop(done_rx2);
        fs::set_permissions(&cache_dir, fs::Permissions::from_mode(0o500)).expect("chmod 0500");
        let worker = thread::spawn(move || {
            deliver_or_preserve_timelapse_outcome(
                done_tx2,
                Ok(TimelapseWorkerDone::ReadyToCommit {
                    staging: Some(staging2),
                    output_path: target2,
                    partial_error: None,
                    codec: RecordingCodec::H264,
                    elapsed: Duration::from_secs(60),
                }),
                Err("EMFILE".to_string()),
            );
        });
        worker.join().expect("join delayed worker");
        fs::set_permissions(&cache_dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
        prune_stale_cache_files_in(&cache_dir, Duration::ZERO);
        assert_eq!(
            fs::read(&staged_path2).expect("retained staging survives prune"),
            b"double-failure-delayed-timelapse"
        );
        let _ = fs::remove_dir_all(&base);
    }
}
