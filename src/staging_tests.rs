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
