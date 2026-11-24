use super::*;
use crate::fs_acl::is_acl_text_allowlisted;

#[test]
fn detects_same_file_across_relative_and_hardlink() {
    let dir = std::env::temp_dir().join(format!("qsr-samefile-{}", random_hex_token()));
    fs::create_dir_all(&dir).expect("mkdir");
    let a = dir.join("orig.mp4");
    let b = dir.join("link.mp4");
    fs::write(&a, b"video").expect("write orig");
    fs::hard_link(&a, &b).expect("hardlink");

    assert!(ensure_distinct_files(&a, &b).is_err());
    assert!(ensure_distinct_files(&a, &dir.join("other.mp4")).is_ok());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn exclusive_staging_enforces_0600_for_new_files_and_cleans_up() {
    let dir = std::env::temp_dir().join(format!("qsr-stage-test-{}", random_hex_token()));
    fs::create_dir_all(&dir).expect("mkdir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let dest = dir.join("result.gif");
    {
        let stage = ExclusiveStagingDir::create_for_target(&dest, "gif").expect("stage");
        fs::write(stage.staged_file(), b"GIF89a-test").expect("write stage");
        stage.commit().expect("commit");
    }
    let mode = fs::metadata(&dest).expect("dest meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let remaining: Vec<_> = fs::read_dir(&dir).expect("readdir").collect();
    assert_eq!(remaining.len(), 1);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sanitizes_escape_and_bidi_sequences() {
    let dirty = "bad\x1b[31mred\u{202E}gpj.exe";
    assert_eq!(sanitize_terminal_text(dirty, false), "bad?[31mred?gpj.exe");
}

#[test]
fn non_colliding_recording_path_never_overwrites_files_or_symlinks() {
    let dir = std::env::temp_dir().join(format!("qsr-noncollide-{}", random_hex_token()));
    fs::create_dir_all(&dir).expect("mkdir");
    let p1 = next_non_colliding_path(&dir, "Recording-2026-11-21-120000", "mp4");
    assert_eq!(p1.file_name().unwrap(), "Recording-2026-11-21-120000.mp4");
    fs::write(&p1, b"clip1").expect("write p1");

    let link2 = dir.join("Recording-2026-11-21-120000-2.mp4");
    std::os::unix::fs::symlink(dir.join("missing.mp4"), &link2).expect("dangling symlink");

    let p3 = next_non_colliding_path(&dir, "Recording-2026-11-21-120000", "mp4");
    assert_eq!(p3.file_name().unwrap(), "Recording-2026-11-21-120000-3.mp4");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn format_tilde_hides_home_prefix_and_rejects_readonly_system_dir() {
    if let Some(home) = valid_home_dir() {
        let desktop = home.join("Desktop");
        assert_eq!(format_tilde_path(&desktop), "~/Desktop");
    }
    assert!(!is_usable_export_dir(Path::new("/System")));
}

#[test]
fn concurrent_commits_reject_group_write_acl_and_ancestor_symlinks() {
    let dir = std::env::temp_dir().join(format!("qsr-atomic-excl-{}", random_hex_token()));
    fs::create_dir_all(&dir).expect("mkdir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let same_target = dir.join("Recording-2026-11-21-150000.mp4");

    let s1 = ExclusiveStagingDir::create_for_target(&same_target, "mp4").expect("stage1");
    let s2 = ExclusiveStagingDir::create_for_target(&same_target, "mp4").expect("stage2");
    fs::write(s1.staged_file(), b"first-video").expect("write s1");
    fs::write(s2.staged_file(), b"second-video").expect("write s2");

    let (p1, _) = s1.commit_with_policy(true).expect("commit s1");
    let (p2, _) = s2.commit_with_policy(true).expect("commit s2");
    assert_ne!(p1, p2);
    assert_eq!(fs::read(&p1).unwrap(), b"first-video");
    assert_eq!(fs::read(&p2).unwrap(), b"second-video");

    // Reject 0770 group-writable non-sticky directory (staff = 20)
    let grp_dir = dir.join("group-writable");
    fs::create_dir_all(&grp_dir).expect("mkdir grp");
    fs::set_permissions(&grp_dir, fs::Permissions::from_mode(0o770)).expect("chmod 0770");
    if fs::metadata(&grp_dir).unwrap().gid() != 0 && fs::metadata(&grp_dir).unwrap().gid() != 80 {
        assert!(!is_usable_export_dir(&grp_dir));
    }

    // Reject pre-canonicalize ancestor symlink (link/sub/out.mp4)
    let real_sub = dir.join("real-sub");
    fs::create_dir_all(&real_sub).expect("mkdir real-sub");
    fs::set_permissions(&real_sub, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let sym_anc = dir.join("sym-anc");
    std::os::unix::fs::symlink(&dir, &sym_anc).expect("symlink anc");
    assert!(
        ExclusiveStagingDir::create_for_target(&sym_anc.join("real-sub/out.mp4"), "mp4").is_err()
    );

    // Verify ACL allowlist rejects third-party write ACE while accepting macOS deny:delete
    assert!(is_acl_text_allowlisted(
        "!#acl 1\ngroup:ABCDEFAB-CDEF-ABCD-EFAB-CDEF0000000C:everyone:12:deny:delete\n",
        501
    ));
    assert!(is_acl_text_allowlisted(
        "!#acl 1\ngroup:ABCDEFAB-CDEF-ABCD-EFAB-CDEF0000000C:everyone:12:deny,file_inherit,directory_inherit:delete\n",
        501
    ));
    assert!(!is_acl_text_allowlisted(
        "!#acl 1\ngroup:ABCDEFAB-CDEF-ABCD-EFAB-CDEF00000014:staff:20:allow:write,add_file\n",
        501
    ));
    assert!(!is_acl_text_allowlisted(
        "!#acl 1\ngroup:ABCDEFAB-CDEF-ABCD-EFAB-CDEF0000000C:everyone:12:allow,file_inherit:read\n",
        501
    ));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn default_export_dir_creates_movies_app_folder_and_preserves_custom_numeric_stems() {
    let mock_home = std::env::temp_dir().join(format!("qsr-mock-home-{}", random_hex_token()));
    let movies = mock_home.join("Movies");
    fs::create_dir_all(&movies).expect("mkdir mock movies");
    fs::set_permissions(&mock_home, fs::Permissions::from_mode(0o700)).expect("chmod home");
    fs::set_permissions(&movies, fs::Permissions::from_mode(0o700)).expect("chmod movies");

    let resolved = default_export_dir_in_home(&mock_home).expect("default movies app dir");
    assert_eq!(resolved, movies.join("AirRec"));
    assert!(resolved.is_dir());

    // Numeric custom stems like `demo-2026` must become `demo-2026-2` on collision, not `demo-2`
    let target = resolved.join("demo-2026.mp4");
    let s1 = ExclusiveStagingDir::create_for_target(&target, "mp4").expect("stage1");
    fs::write(s1.staged_file(), b"first").expect("write s1");
    let (p1, _) = s1.commit_with_policy(true).expect("commit s1");
    assert_eq!(p1.file_name().unwrap(), "demo-2026.mp4");

    let s2 =
        ExclusiveStagingDir::create_for_target_with_policy(&target, "mp4", true).expect("stage2");
    fs::write(s2.staged_file(), b"second").expect("write s2");
    let (p2, _) = s2.commit_with_policy(true).expect("commit s2");
    assert_eq!(p2.file_name().unwrap(), "demo-2026-2.mp4");

    // Existing directory and symlink collisions must also advance safely without failing or modifying them
    let colliding = resolved.join("collide.mp4");
    fs::create_dir(&colliding).expect("mkdir colliding dir");
    let link2 = resolved.join("collide-2.mp4");
    std::os::unix::fs::symlink(resolved.join("missing.mp4"), &link2).expect("dangling link");
    let s3 = ExclusiveStagingDir::create_for_target_with_policy(&colliding, "mp4", true)
        .expect("stage3");
    fs::write(s3.staged_file(), b"third").expect("write s3");
    let (p3, _) = s3.commit_with_policy(true).expect("commit s3");
    assert_eq!(p3.file_name().unwrap(), "collide-3.mp4");
    assert!(colliding.is_dir());
    assert!(
        fs::symlink_metadata(&link2)
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let _ = fs::remove_dir_all(&mock_home);
}
