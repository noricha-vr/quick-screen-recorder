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

/// Generates a local-timestamped output path `YYYY-MM-DD_HH-MM-SS_AirRec.<ext>`
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
            match DirBuilder::new().mode(0o700).create(dir) {
                Ok(()) => {}
                Err(ce) if ce.kind() == io::ErrorKind::AlreadyExists => {}
                Err(ce) => return Err(format!("Failed creating private directory: {ce}")),
            }
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
    prune_stale_cache_files_in(&dir, max_age);
}

pub(crate) fn prune_stale_cache_files_in(dir: &Path, max_age: Duration) {
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
            .is_some_and(|age| age >= max_age);
        if !expired {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if meta.is_file() {
            if !name_str.starts_with("qsr-preserved-") {
                let _ = fs::remove_file(entry.path());
            }
        } else if meta.is_dir() {
            if name_str.starts_with(".qsr-work-") {
                if let Ok(dfd) = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(entry.path())
                {
                    // Acquire the exclusive lock FIRST, then inspect `.qsr-keep`, so a recorder
                    // that flips `.qsr-keep` from `0` -> `1` before releasing its lock can never
                    // race with a pruner that read stale `0` prior to locking.
                    if unsafe { libc::flock(dfd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                        if has_staged_keep_marker(dfd.as_raw_fd()) {
                            continue;
                        }
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

fn has_staged_keep_marker(dir_fd: libc::c_int) -> bool {
    let fd = unsafe {
        libc::openat(
            dir_fd,
            c".qsr-keep".as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        // Only definitively absent (`ENOENT`) is prunable; fail closed on EACCES/EMFILE/EIO/etc.
        return io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT);
    }
    let mut byte = [0_u8; 1];
    let n = loop {
        let rc = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), 1) };
        if rc < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        break rc;
    };
    unsafe { libc::close(fd) };
    if n < 0 {
        // Read error (e.g. EIO): fail closed and skip pruning.
        return true;
    }
    // `b'0'` = active/unretained staging (prunable after crash + 24h once flock releases).
    // `b'1'` (explicitly retained) or 0-byte legacy marker = permanently exempt from pruning.
    n == 0 || (n == 1 && byte[0] != b'0')
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
#[path = "fs_safety_tests.rs"]
mod tests;
