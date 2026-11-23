use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use crate::config::APP_SUPPORT_DIR_NAME;
use crate::export_name::default_export_stem;
pub(crate) use crate::fs_acl::{clear_fd_extended_acl, verify_pre_canonical_target_parent};
pub use crate::fs_acl::{
    default_export_dir_in_home, default_movies_export_dir, has_only_safe_acls,
    is_usable_export_dir, valid_home_dir,
};
use crate::observe::now_unix_secs;
pub use crate::staging::ExclusiveStagingDir;

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(1);

pub fn random_hex_token() -> String {
    let mut buf = [0_u8; 16];
    if let Ok(mut f) = File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            let mut hex = String::with_capacity(32);
            for b in buf {
                use std::fmt::Write as _;
                let _ = write!(&mut hex, "{b:02x}");
            }
            return hex;
        }
    }
    let seq = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:016x}{:08x}{:08x}",
        now_unix_secs(),
        std::process::id(),
        seq
    )
}

pub fn next_unique_token() -> String {
    random_hex_token()
}

/// Formats a directory path with `~` substitution for UI display so personal home usernames never appear on screen.
pub fn format_tilde_path(path: &Path) -> String {
    if let Some(home) = valid_home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            if rel.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!(
                "~/{}",
                sanitize_terminal_text(&rel.display().to_string(), false)
            );
        }
    }
    sanitize_terminal_text(&path.display().to_string(), false)
}

/// Generates a local-timestamped output path `YYYY-MM-DD_HH-MM-SS_Quick-Screen-Recorder.<ext>`
/// inside `dir` without overwriting any existing file or symlink.
pub fn next_recording_output_path(dir: &Path, ext: &str) -> PathBuf {
    let clean_ext = ext.trim_start_matches('.');
    let stem = default_export_stem(now_unix_secs(), None);
    next_non_colliding_path(dir, &stem, clean_ext)
}

pub fn next_non_colliding_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.{ext}"));
    if matches!(fs::symlink_metadata(&first), Err(e) if e.kind() == io::ErrorKind::NotFound) {
        return first;
    }
    for suffix in 2..=9999_u32 {
        let candidate = dir.join(format!("{stem}-{suffix}.{ext}"));
        if matches!(fs::symlink_metadata(&candidate), Err(e) if e.kind() == io::ErrorKind::NotFound)
        {
            return candidate;
        }
    }
    dir.join(format!("{stem}-{}.{ext}", random_hex_token()))
}

pub fn ensure_distinct_files(input: &Path, output: &Path) -> Result<(), String> {
    if input == output {
        return Err("Output path must be different from input recording path".to_string());
    }
    if let (Ok(in_meta), Ok(out_meta)) = (fs::metadata(input), fs::metadata(output)) {
        if in_meta.dev() == out_meta.dev() && in_meta.ino() == out_meta.ino() {
            return Err("Output path resolves to the same file as the input recording".to_string());
        }
    }
    Ok(())
}

pub fn private_cache_dir() -> Result<PathBuf, String> {
    let base = if let Some(home) = valid_home_dir() {
        home.join("Library")
            .join("Caches")
            .join(APP_SUPPORT_DIR_NAME)
    } else {
        std::env::temp_dir().join(format!("{APP_SUPPORT_DIR_NAME}-{}", unsafe {
            libc::geteuid()
        }))
    };

    ensure_private_dir(&base)?;
    Ok(base)
}

pub fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    if !dir.is_absolute()
        || dir
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("Private directory path must be absolute and dot-free".to_string());
    }
    let euid = unsafe { libc::geteuid() };
    verify_pre_canonical_target_parent(dir.parent().unwrap_or(Path::new("/")), euid)?;
    if let Some(parent) = dir.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed creating parent directory: {e}"))?;
        }
    }
    verify_pre_canonical_target_parent(dir.parent().unwrap_or(Path::new("/")), euid)?;

    match fs::symlink_metadata(dir) {
        Ok(sym_meta) => {
            if sym_meta.file_type().is_symlink() || !sym_meta.is_dir() {
                return Err("Refusing to use symlink or non-directory private path".to_string());
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            DirBuilder::new()
                .mode(0o700)
                .create(dir)
                .map_err(|e| format!("Failed creating private directory: {e}"))?;
        }
        Err(e) => return Err(format!("Failed checking private directory: {e}")),
    }

    let verified = fs::symlink_metadata(dir)
        .map_err(|e| format!("Failed verifying private directory: {e}"))?;
    if verified.file_type().is_symlink() || !verified.is_dir() || verified.uid() != euid {
        return Err("Private directory must be owned by the current user".to_string());
    }

    let dir_fd = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)
        .map_err(|e| format!("Failed opening private directory without following symlinks: {e}"))?;
    dir_fd
        .set_permissions(fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("Failed setting 0700 permissions on private directory: {e}"))?;
    clear_fd_extended_acl(dir_fd.as_raw_fd())?;
    if !has_only_safe_acls(dir, euid) {
        return Err("Private directory still carries unsafe extended ACL".to_string());
    }
    Ok(())
}

pub fn unique_private_file_path(prefix: &str, ext: &str) -> Result<PathBuf, String> {
    let dir = private_cache_dir()?;
    Ok(dir.join(format!("{prefix}-{}.{ext}", random_hex_token())))
}

/// Creates a fresh 0700 subdirectory inside our private cache dir so clipboard exports can carry
/// the user's chosen display filename while staying isolated from other local users.
pub fn unique_private_clipboard_dir() -> Result<PathBuf, String> {
    let root = private_cache_dir()?;
    let clip_dir = root.join(format!("qsr-clip-{}", random_hex_token()));
    DirBuilder::new()
        .mode(0o700)
        .create(&clip_dir)
        .map_err(|e| format!("Failed creating clipboard staging directory: {e}"))?;
    let dfd = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&clip_dir)
        .map_err(|e| format!("Failed opening clipboard directory: {e}"))?;
    clear_fd_extended_acl(dfd.as_raw_fd())?;
    Ok(clip_dir)
}

pub fn atomic_write_private(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let euid = unsafe { libc::geteuid() };
    if let Ok(meta) = fs::symlink_metadata(target) {
        if meta.file_type().is_symlink() || !meta.is_file() || meta.uid() != euid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Refusing to overwrite symlink, special file, or foreign file",
            ));
        }
    }
    let parent = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => std::env::current_dir()?,
    };
    fs::create_dir_all(&parent)?;
    let parent_sym = fs::symlink_metadata(&parent)?;
    if parent_sym.file_type().is_symlink() || !parent_sym.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Refusing to write into symlinked parent directory",
        ));
    }

    let tmp = parent.join(format!(".qsr-tmp-{}", random_hex_token()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .mode(0o600)
        .open(&tmp)?;
    if let Err(e) = clear_fd_extended_acl(file.as_raw_fd()) {
        drop(file);
        let _ = fs::remove_file(&tmp);
        return Err(io::Error::other(e));
    }
    if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    drop(file);
    if let Err(e) = fs::rename(&tmp, target) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

pub fn prune_stale_cache_files(max_age: Duration) {
    let Ok(dir) = private_cache_dir() else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry
            .file_type()
            .and_then(|_| fs::symlink_metadata(entry.path()))
        else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let expired = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > max_age);
        if !expired {
            continue;
        }
        if meta.is_file() {
            let _ = fs::remove_file(entry.path());
        } else if meta.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with(".qsr-work-") {
                if let Ok(dfd) = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(entry.path())
                {
                    if unsafe { libc::flock(dfd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                        let _ = fs::remove_dir_all(entry.path());
                    }
                }
            } else if name_str.starts_with("qsr-clip-") && meta.uid() == unsafe { libc::geteuid() }
            {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
}

/// Strips ASCII/Unicode control characters as well as Unicode bidirectional override characters.
pub fn sanitize_terminal_text(input: &str, allow_newlines: bool) -> String {
    input
        .chars()
        .map(|c| {
            if is_bidi_control(c) || (!allow_newlines && matches!(c, '\u{2028}' | '\u{2029}')) {
                '?'
            } else if c.is_control() {
                if allow_newlines && (c == '\n' || c == '\t') {
                    c
                } else {
                    '?'
                }
            } else {
                c
            }
        })
        .collect()
}

const fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
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
        if fs::metadata(&grp_dir).unwrap().gid() != 0 && fs::metadata(&grp_dir).unwrap().gid() != 80
        {
            assert!(!is_usable_export_dir(&grp_dir));
        }

        // Reject pre-canonicalize ancestor symlink (link/sub/out.mp4)
        let real_sub = dir.join("real-sub");
        fs::create_dir_all(&real_sub).expect("mkdir real-sub");
        fs::set_permissions(&real_sub, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
        let sym_anc = dir.join("sym-anc");
        std::os::unix::fs::symlink(&dir, &sym_anc).expect("symlink anc");
        assert!(
            ExclusiveStagingDir::create_for_target(&sym_anc.join("real-sub/out.mp4"), "mp4")
                .is_err()
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
        assert_eq!(resolved, movies.join("Quick Screen Recorder"));
        assert!(resolved.is_dir());

        // Numeric custom stems like `demo-2026` must become `demo-2026-2` on collision, not `demo-2`
        let target = resolved.join("demo-2026.mp4");
        let s1 = ExclusiveStagingDir::create_for_target(&target, "mp4").expect("stage1");
        fs::write(s1.staged_file(), b"first").expect("write s1");
        let (p1, _) = s1.commit_with_policy(true).expect("commit s1");
        assert_eq!(p1.file_name().unwrap(), "demo-2026.mp4");

        let s2 = ExclusiveStagingDir::create_for_target_with_policy(&target, "mp4", true)
            .expect("stage2");
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
}
