use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::time::Duration;

use super::*;
use crate::fs_safety::prune_stale_cache_files_in;

#[test]
fn preserve_to_recovery_dir_and_retain_staged_survive_drop_and_cache_pruning() {
    let base = std::env::temp_dir().join(format!("qsr-recover-{}", random_hex_token()));
    let dest_dir = base.join("external-drive");
    let rec_dir = base.join("local-recovered");
    fs::create_dir_all(&dest_dir).expect("create mock external drive");
    let target = dest_dir.join("2026-11-24_timelapse.mp4");

    // 1. Primary recovery into private recovery dir when destination disappears.
    let staging = ExclusiveStagingDir::create_for_target(&target, "mp4").expect("stage");
    fs::write(staging.staged_file(), b"valid-mp4-payload").expect("write staged mp4");
    fs::remove_dir_all(&dest_dir).expect("remove mock external drive");
    assert!(staging.commit().is_err());

    let sym_rec = base.join("symlink-recovered");
    std::os::unix::fs::symlink(&base, &sym_rec).expect("create symlink");
    assert!(staging.preserve_to_recovery_dir(&sym_rec).is_err());

    let (rec_path, size) = staging.preserve_to_recovery_dir(&rec_dir).expect("recover");
    assert_eq!(size, b"valid-mp4-payload".len() as u64);
    drop(staging);
    assert_eq!(fs::read(&rec_path).unwrap(), b"valid-mp4-payload");
    assert_eq!(
        fs::symlink_metadata(&rec_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    // 2. Last-resort fallback: retain_staged_file() survives drop() AND expired cache pruning!
    fs::create_dir_all(&dest_dir).expect("recreate mock drive");
    let mut staging2 = ExclusiveStagingDir::create_for_target(&target, "mp4").expect("stage2");
    fs::write(staging2.staged_file(), b"last-resort-mp4").expect("write staged2");
    fs::remove_dir_all(&dest_dir).expect("remove mock drive again");
    assert!(staging2.commit().is_err());
    assert!(staging2.preserve_to_recovery_dir(&sym_rec).is_err());
    let kept = staging2.retain_staged_file();
    drop(staging2);

    let kept_dir = kept.parent().expect("kept parent dir");
    assert!(
        kept_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("qsr-preserved-"),
        "retained dir should be renamed out of .qsr-work-*: {kept_dir:?}"
    );
    assert!(kept_dir.join(".qsr-keep").exists());

    // Simulate an isolated cache dir containing:
    //  (a) a renamed `qsr-preserved-*` dir
    //  (b) a `.qsr-work-*` dir with `.qsr-keep` marker (fallback if rename failed)
    //  (c) an ordinary abandoned `.qsr-work-*` dir (must still be pruned!)
    let mock_cache = base.join("mock-cache");
    fs::create_dir_all(&mock_cache).expect("create mock cache");
    let preserved_copy = mock_cache.join("qsr-preserved-test");
    DirBuilder::new()
        .mode(0o700)
        .create(&preserved_copy)
        .unwrap();
    fs::write(preserved_copy.join("staged.mp4"), b"renamed-mp4").unwrap();

    let marked_work = mock_cache.join(".qsr-work-marked");
    DirBuilder::new().mode(0o700).create(&marked_work).unwrap();
    fs::write(marked_work.join(".qsr-keep"), b"").unwrap();
    fs::write(marked_work.join("staged.mp4"), b"marked-mp4").unwrap();

    let stale_work = mock_cache.join(".qsr-work-abandoned");
    DirBuilder::new().mode(0o700).create(&stale_work).unwrap();
    fs::write(stale_work.join("staged.mp4"), b"abandoned").unwrap();

    // Run real cache pruning with max_age = ZERO (every entry is expired).
    prune_stale_cache_files_in(&mock_cache, Duration::ZERO);

    assert_eq!(
        fs::read(preserved_copy.join("staged.mp4")).unwrap(),
        b"renamed-mp4",
        "qsr-preserved-* must survive expired cache pruning"
    );
    assert_eq!(
        fs::read(marked_work.join("staged.mp4")).unwrap(),
        b"marked-mp4",
        ".qsr-work-* with .qsr-keep marker must survive expired cache pruning"
    );
    assert!(
        !stale_work.exists(),
        "ordinary abandoned .qsr-work-* must still be pruned"
    );

    assert_eq!(fs::read(&kept).unwrap(), b"last-resort-mp4");
    assert_eq!(
        fs::symlink_metadata(&kept).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let _ = fs::remove_dir_all(kept_dir);
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn quit_during_recording_preserves_video_in_recovery_and_excludes_from_temp_cleanup() {
    let base = std::env::temp_dir().join(format!("qsr-quit-save-{}", random_hex_token()));
    let cache_dir = base.join("cache");
    let rec_dir = base.join("recovered");
    fs::create_dir_all(&cache_dir).expect("mkdir cache");

    let raw_out = cache_dir.join("capture-active.mp4");
    let stale_other = cache_dir.join("capture-older.mp4");
    fs::write(&raw_out, b"hours-of-timelapse-bytes").expect("write active");
    fs::write(&stale_other, b"old-temp").expect("write stale");
    let mut temp_files = vec![stale_other.clone(), raw_out.clone()];

    let saved = preserve_recording_on_quit(&raw_out, &mut temp_files, Ok(rec_dir.clone()))
        .expect("preserve recording on quit");
    assert!(!temp_files.contains(&raw_out));
    assert!(!raw_out.exists(), "source temp file should be moved out");

    // Simulate finish_app_termination() draining temp_files.
    for tmp in temp_files.drain(..) {
        let _ = fs::remove_file(tmp);
    }
    assert!(
        !stale_other.exists(),
        "unrelated temp file should be cleaned"
    );
    assert_eq!(
        fs::read(&saved).expect("read quit-saved recording"),
        b"hours-of-timelapse-bytes"
    );
    assert_eq!(
        fs::symlink_metadata(&saved).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // If recovery move fails (symlinked recovery dir), raw_out must STILL be removed from
    // temp_files and renamed to qsr-preserved-* so neither shutdown nor 24h pruning deletes it.
    let raw_out2 = cache_dir.join("capture-fallback.mp4");
    fs::write(&raw_out2, b"fallback-bytes").expect("write fallback");
    let sym_rec = base.join("sym-rec");
    std::os::unix::fs::symlink(&base, &sym_rec).expect("symlink");
    let mut temp_files2 = vec![raw_out2.clone()];
    let (_, kept_fallback) = preserve_recording_on_quit(&raw_out2, &mut temp_files2, Ok(sym_rec))
        .expect_err("symlink recovery dir must error");
    for tmp in temp_files2.drain(..) {
        let _ = fs::remove_file(tmp);
    }
    prune_stale_cache_files_in(&cache_dir, Duration::ZERO);
    assert_eq!(fs::read(&kept_fallback).unwrap(), b"fallback-bytes");

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn commit_or_recover_staging_rescues_on_destination_loss_and_retains_on_double_failure() {
    let base = std::env::temp_dir().join(format!("qsr-cor-{}", random_hex_token()));
    let dest_dir = base.join("usb-dest");
    let rec_dir = base.join("rec");
    fs::create_dir_all(&dest_dir).expect("mkdir dest");
    let target = dest_dir.join("clip.mp4");

    // 1. Destination lost -> recovers cleanly into recovery_dir with warning.
    let mut s1 = ExclusiveStagingDir::create_for_target(&target, "mp4").expect("stage1");
    fs::write(s1.staged_file(), b"realtime-mp4-bytes").expect("write s1");
    fs::remove_dir_all(&dest_dir).expect("unmount mock usb");
    let (saved_path, size, warn) =
        commit_or_recover_staging(&mut s1, target.clone(), None, Ok(rec_dir.clone()))
            .expect("recover into rec_dir");
    drop(s1);
    assert_eq!(size, b"realtime-mp4-bytes".len() as u64);
    assert!(warn.is_some());
    assert_eq!(fs::read(&saved_path).unwrap(), b"realtime-mp4-bytes");

    // 2. Both destination and recovery fail -> error reports retained staging path that survives drop.
    fs::create_dir_all(&dest_dir).expect("remount mock usb");
    let mut s2 = ExclusiveStagingDir::create_for_target(&target, "mp4").expect("stage2");
    fs::write(s2.staged_file(), b"double-fail-bytes").expect("write s2");
    fs::remove_dir_all(&dest_dir).expect("unmount mock usb again");
    let err = commit_or_recover_staging(
        &mut s2,
        target,
        None,
        Err("recovery dir blocked".to_string()),
    )
    .expect_err("double failure returns Err");
    let retained_file = s2.staged_file().to_path_buf();
    drop(s2);
    assert!(err.contains("preserved in staging"), "got: {err}");
    assert_eq!(fs::read(&retained_file).unwrap(), b"double-fail-bytes");

    let _ = fs::remove_dir_all(retained_file.parent().unwrap());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn preserve_staged_permanently_survives_gui_temp_files_cleanup_on_stop_error() {
    let base = std::env::temp_dir().join(format!("qsr-perm-{}", random_hex_token()));
    let cache_dir = base.join("cache");
    let rec_dir = base.join("rec");
    fs::create_dir_all(&cache_dir).expect("mkdir cache");
    let gui_temp_out = cache_dir.join("capture-gui.mp4");
    let mut temp_files = vec![gui_temp_out.clone()];

    let mut s1 = ExclusiveStagingDir::create_for_target(&gui_temp_out, "mp4").expect("stage1");
    fs::write(s1.staged_file(), b"salvaged-during-stop-error").expect("write s1");
    let perm_path =
        preserve_staged_permanently(&mut s1, Ok(rec_dir.clone())).expect("preserve permanently");
    drop(s1);
    assert_ne!(perm_path, gui_temp_out);

    // Draining GUI temp_files must never remove the permanently preserved recording.
    for tmp in temp_files.drain(..) {
        let _ = fs::remove_file(tmp);
    }
    assert_eq!(fs::read(&perm_path).unwrap(), b"salvaged-during-stop-error");

    // Fallback when recovery dir fails: retained in qsr-preserved-* and survives cache pruning.
    let mut s2 = ExclusiveStagingDir::create_for_target(&gui_temp_out, "mp4").expect("stage2");
    fs::write(s2.staged_file(), b"retained-fallback").expect("write s2");
    let (_, kept) = preserve_staged_permanently(&mut s2, Err("blocked".to_string()))
        .expect_err("recovery error returns kept path");
    drop(s2);
    assert_eq!(fs::read(&kept).unwrap(), b"retained-fallback");

    let _ = fs::remove_dir_all(kept.parent().unwrap());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn preallocated_keep_marker_survives_retain_marker_creation_failure_and_cache_pruning() {
    let base = std::env::temp_dir().join(format!("qsr-prekeep-{}", random_hex_token()));
    let cache_dir = base.join("cache");
    let dest_dir = base.join("dest");
    fs::create_dir_all(&cache_dir).expect("mkdir cache");
    fs::create_dir_all(&dest_dir).expect("mkdir dest");
    fs::set_permissions(&cache_dir, fs::Permissions::from_mode(0o700)).expect("chmod cache");
    let target = dest_dir.join("capture.mp4");

    let mut staging =
        ExclusiveStagingDir::create_in_cache_root(&cache_dir, &target, "mp4", false).expect("stg");
    let staged_path = staging.staged_file().to_path_buf();
    let stage_dir = staged_path.parent().expect("stage dir").to_path_buf();
    fs::write(&staged_path, b"critical-unflushed-recording").expect("write payload");

    // Pre-allocated `.qsr-keep` exists before retain is ever needed.
    assert!(stage_dir.join(".qsr-keep").exists());

    // Simulate ENOSPC/EMFILE/EACCES at retain time by making `stage_dir` read-only (0o500),
    // ensuring creating any new file inside `stage_dir` fails during `retain_staged_in_place()`.
    fs::set_permissions(&stage_dir, fs::Permissions::from_mode(0o500)).expect("chmod 0500");
    assert!(
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(stage_dir.join(".probe-cannot-create"))
            .is_err(),
        "new file creation inside read-only staging dir must fail"
    );
    let kept = staging.retain_staged_in_place();
    assert_eq!(kept, staged_path);
    drop(staging);

    // Restore writable mode so `prune_stale_cache_files_in` CAN delete unmarked dirs, then run
    // expired cache pruning (`Duration::ZERO`) — pre-allocated `.qsr-keep` MUST protect the dir!
    fs::set_permissions(&stage_dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    prune_stale_cache_files_in(&cache_dir, Duration::ZERO);

    assert!(
        staged_path.exists(),
        "retained staged file must survive expired cache pruning even when marker creation fails at retain time"
    );
    assert_eq!(
        fs::read(&staged_path).expect("read preserved payload"),
        b"critical-unflushed-recording"
    );

    let _ = fs::remove_dir_all(&base);
}
