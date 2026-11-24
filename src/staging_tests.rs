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
    let export_target = dest_dir.join("clip.gif");

    // 1. Ordinary export staging (`preallocate_keep = false`) has NO `.qsr-keep`, so crashed
    //    exports prune cleanly after 24h AND `try_retain_staged_in_place` propagates write error!
    let mut exp_stg =
        ExclusiveStagingDir::create_in_cache_root(&cache_dir, &export_target, "gif", false, false)
            .expect("export staging");
    let exp_dir = exp_stg.staged_file().parent().unwrap().to_path_buf();
    assert!(!exp_dir.join(".qsr-keep").exists());
    fs::set_permissions(&exp_dir, fs::Permissions::from_mode(0o500)).expect("chmod 0500 exp");
    assert!(
        exp_stg.try_retain_staged_in_place().is_err(),
        "non-preallocated staging must report marker write failure instead of silent success"
    );
    fs::set_permissions(&exp_dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700 exp");
    drop(exp_stg);
    // Unmarked expired export staging dir is pruned cleanly.
    prune_stale_cache_files_in(&cache_dir, Duration::ZERO);
    assert!(
        !exp_dir.exists(),
        "unmarked crashed export dir must be pruned after expiry"
    );

    // 2. Recording staging (`preallocate_keep = true`) durably syncs `.qsr-keep = b"1"` BEFORE
    //    capture starts. Even when BOTH marker writes (`chmod 0o400`) AND dir creation (`chmod 0o500`)
    //    fail at retain time, `try_retain_staged_in_place()` succeeds with 0 writes and survives prune!
    let mut staging =
        ExclusiveStagingDir::create_in_cache_root(&cache_dir, &target, "mp4", false, true)
            .expect("rec staging");
    let staged_path = staging.staged_file().to_path_buf();
    let stage_dir = staged_path.parent().expect("stage dir").to_path_buf();
    let keep_marker = stage_dir.join(".qsr-keep");
    fs::write(&staged_path, b"critical-unflushed-recording").expect("write payload");
    assert_eq!(fs::read(&keep_marker).unwrap(), b"1");

    // Inject write failures on BOTH `.qsr-keep` (0o400) AND `stage_dir` (0o500).
    fs::set_permissions(&keep_marker, fs::Permissions::from_mode(0o400)).expect("chmod 0400 keep");
    fs::set_permissions(&stage_dir, fs::Permissions::from_mode(0o500)).expect("chmod 0500 dir");
    assert!(
        fs::OpenOptions::new()
            .write(true)
            .open(&keep_marker)
            .is_err(),
        "direct write to read-only keep marker must fail"
    );
    let kept = staging
        .try_retain_staged_in_place()
        .expect("pre-synced recording keep marker succeeds with zero writes");
    assert_eq!(kept, staged_path);
    drop(staging);

    fs::set_permissions(&stage_dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700 dir");
    fs::set_permissions(&keep_marker, fs::Permissions::from_mode(0o600)).expect("chmod 0600 keep");
    prune_stale_cache_files_in(&cache_dir, Duration::ZERO);

    assert!(
        staged_path.exists(),
        "retained recording must survive expired cache pruning even under marker write failure"
    );
    assert_eq!(
        fs::read(&staged_path).expect("read preserved payload"),
        b"critical-unflushed-recording"
    );

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn auto_stop_direct_recovery_double_failure_survives_cache_pruning() {
    let base = std::env::temp_dir().join(format!("qsr-autostop-dbl-{}", random_hex_token()));
    let cache_dir = base.join("cache");
    fs::create_dir_all(&cache_dir).expect("mkdir cache");
    fs::set_permissions(&cache_dir, fs::Permissions::from_mode(0o700)).expect("chmod cache");
    let prunable_gui_target = cache_dir.join("capture-gui-temp.mp4");

    let mut staging = ExclusiveStagingDir::create_in_cache_root(
        &cache_dir,
        &prunable_gui_target,
        "mp4",
        false,
        true,
    )
    .expect("create staging");
    fs::write(staging.staged_file(), b"auto-stopped-recording-bytes").expect("write clip");

    // Force DOUBLE failure on auto-stop direct preservation:
    // (1) recovery_dir fails (Err("EMFILE")) AND (2) parent cache_dir is 0o500 so
    // `.qsr-work-*` -> `qsr-preserved-*` rename ALSO fails!
    fs::set_permissions(&cache_dir, fs::Permissions::from_mode(0o500)).expect("chmod 0500");
    let (saved_path, size, warning) = crate::capture::recorder::commit_staged_to_recovery_directly(
        &mut staging,
        None,
        Err("EMFILE".to_string()),
    )
    .expect("direct preservation fallback returns protected staging path");
    drop(staging);

    assert_eq!(size, b"auto-stopped-recording-bytes".len() as u64);
    assert!(warning.as_deref().unwrap_or("").contains("EMFILE"));
    assert!(
        !prunable_gui_target.exists(),
        "auto-stop direct recovery must never fall back to a prunable capture-*.mp4 in Caches"
    );

    // Restore writable cache_dir and run expired cache pruning (`Duration::ZERO`):
    // protected staging dir (`.qsr-keep == b"1"`) MUST survive pruning intact!
    fs::set_permissions(&cache_dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    prune_stale_cache_files_in(&cache_dir, Duration::ZERO);

    assert!(
        saved_path.exists(),
        "double-failure auto-stop recording must survive 24h cache pruning"
    );
    assert_eq!(
        fs::read(&saved_path).expect("read preserved bytes"),
        b"auto-stopped-recording-bytes"
    );

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn prune_checks_marker_after_flock_and_fails_closed_on_unreadable_marker() {
    use std::thread;

    let base = std::env::temp_dir().join(format!("qsr-flock-prune-{}", random_hex_token()));
    let cache_dir = base.join("cache");
    let dest_dir = base.join("dest");
    fs::create_dir_all(&cache_dir).expect("mkdir cache");
    fs::create_dir_all(&dest_dir).expect("mkdir dest");
    fs::set_permissions(&cache_dir, fs::Permissions::from_mode(0o700)).expect("chmod cache");

    // 1. Lock-then-check ordering: while non-preallocated staging holds flock without `.qsr-keep`,
    //    concurrent pruner cannot delete it; once it writes `.qsr-keep` and drops (releasing flock),
    //    a subsequent prune acquires flock, reads `.qsr-keep = '1'`, and preserves the directory!
    let target = dest_dir.join("long-rec.mp4");
    let mut staging =
        ExclusiveStagingDir::create_in_cache_root(&cache_dir, &target, "mp4", false, false)
            .expect("stg");
    let staged_path = staging.staged_file().to_path_buf();
    fs::write(&staged_path, b"over-24h-recording").expect("write rec");

    let cache_clone = cache_dir.clone();
    let pruner = thread::spawn(move || {
        prune_stale_cache_files_in(&cache_clone, Duration::ZERO);
    });
    pruner.join().expect("join concurrent pruner while locked");
    assert!(staged_path.exists(), "locked staging must not be pruned");

    staging
        .try_retain_staged_in_place()
        .expect("persist keep marker before unlock");
    drop(staging);
    prune_stale_cache_files_in(&cache_dir, Duration::ZERO);
    assert_eq!(
        fs::read(&staged_path).expect("retained staging survives prune after unlock"),
        b"over-24h-recording"
    );

    // 2. Unreadable `.qsr-keep` (EACCES, chmod 0o000): must fail CLOSED and NOT delete directory!
    let unreadable_dir = cache_dir.join(format!(".qsr-work-unreadable-{}", random_hex_token()));
    DirBuilder::new()
        .mode(0o700)
        .create(&unreadable_dir)
        .unwrap();
    let unreadable_marker = unreadable_dir.join(".qsr-keep");
    fs::write(&unreadable_marker, b"1").unwrap();
    fs::write(unreadable_dir.join("staged.mp4"), b"must-not-delete").unwrap();
    fs::set_permissions(&unreadable_marker, fs::Permissions::from_mode(0o000)).unwrap();

    prune_stale_cache_files_in(&cache_dir, Duration::ZERO);
    assert!(
        unreadable_dir.join("staged.mp4").exists(),
        "unreadable .qsr-keep must fail closed and skip pruning"
    );
    fs::set_permissions(&unreadable_marker, fs::Permissions::from_mode(0o600)).unwrap();
    let _ = fs::remove_dir_all(&base);
}
