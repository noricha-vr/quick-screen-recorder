use std::ffi::{CStr, CString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use crate::config::APP_SUPPORT_DIR_NAME;
use crate::observe::now_unix_secs;
pub use crate::staging::ExclusiveStagingDir;

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(1);
const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;

unsafe extern "C" {
    fn acl_get_link_np(path: *const libc::c_char, acl_type: libc::c_int) -> *mut libc::c_void;
    fn acl_init(count: libc::c_int) -> *mut libc::c_void;
    fn acl_set_fd_np(fd: libc::c_int, acl: *mut libc::c_void, acl_type: libc::c_int)
    -> libc::c_int;
    fn acl_to_text(acl: *mut libc::c_void, len_p: *mut libc::ssize_t) -> *mut libc::c_char;
    fn acl_free(obj_p: *mut libc::c_void) -> libc::c_int;
}

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

/// Returns `HOME` if it is non-empty and absolute, falling back to OS `getpwuid_r` for `geteuid()`.
pub fn valid_home_dir() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("HOME") {
        if !raw.is_empty() {
            let p = PathBuf::from(raw);
            if p.is_absolute() {
                return Some(p);
            }
        }
    }
    pwuid_home_dir()
}

fn pwuid_home_dir() -> Option<PathBuf> {
    let euid = unsafe { libc::geteuid() };
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = ptr::null_mut();
    let mut buf = vec![0_u8; 4096];
    let rc = unsafe {
        libc::getpwuid_r(
            euid,
            &mut pwd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(pwd.pw_dir).to_str().ok()? };
    if s.is_empty() {
        return None;
    }
    let p = PathBuf::from(s);
    p.is_absolute().then_some(p)
}

/// Returns `~/Desktop` when available as a verified writable persistent directory owned by the current user,
/// falling back to `HOME` or `~/Library/Application Support/QuickScreenRecorder/Exports`.
/// Never falls back to `/tmp` or auto-pruned cache directories (`None` prompts user to pick a folder).
pub fn default_desktop_export_dir() -> Option<PathBuf> {
    let home = valid_home_dir()?;
    let desktop = home.join("Desktop");
    if is_usable_export_dir(&desktop) {
        return Some(desktop);
    }
    if is_usable_export_dir(&home) {
        return Some(home);
    }
    let app_exports = home
        .join("Library")
        .join("Application Support")
        .join(APP_SUPPORT_DIR_NAME)
        .join("Exports");
    if ensure_private_dir(&app_exports).is_ok() && is_usable_export_dir(&app_exports) {
        return Some(app_exports);
    }
    None
}

/// Verifies `dir` is an absolute, dot-free, writable, non-symlink directory with safe group/ACL
/// permissions and no user-controlled symlinks in its ancestor chain.
pub fn is_usable_export_dir(dir: &Path) -> bool {
    if !dir.is_absolute()
        || dir
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    let euid = unsafe { libc::geteuid() };
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return false;
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return false;
    }
    let mode = meta.permissions().mode();
    let sticky = (mode & 0o1000) != 0;
    // Leaf export dir must be owned by current user (or be a sticky root temp dir), never e.g. /System
    if meta.uid() != euid && !(meta.uid() == 0 && sticky) {
        return false;
    }
    if !is_safe_dir_mode_and_group(mode, meta.gid()) || !has_only_safe_acls(dir, euid) {
        return false;
    }
    if !is_dir_writable(dir) {
        return false;
    }
    verify_safe_dir_ancestors(dir.parent(), euid)
}

/// Validates existing ancestors BEFORE `canonicalize` so `symlink_dir/real_sub/out.mp4` is rejected.
pub(crate) fn verify_pre_canonical_target_parent(parent: &Path, euid: u32) -> Result<(), String> {
    let abs = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("Cannot resolve cwd: {e}"))?
            .join(parent)
    };
    if abs.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("Refusing parent-traversal ('..') in output path".to_string());
    }
    let mut cur: Option<&Path> = Some(&abs);
    while let Some(p) = cur {
        if p.as_os_str().is_empty() {
            break;
        }
        match fs::symlink_metadata(p) {
            Ok(lmeta) => {
                if lmeta.file_type().is_symlink() && lmeta.uid() != 0 {
                    return Err("Refusing to traverse user symlink in output path".to_string());
                }
                if !lmeta.file_type().is_symlink() {
                    let mode = lmeta.permissions().mode();
                    if !lmeta.is_dir()
                        || (lmeta.uid() != euid && lmeta.uid() != 0)
                        || !is_safe_dir_mode_and_group(mode, lmeta.gid())
                        || !has_only_safe_acls(p, euid)
                    {
                        return Err(
                            "Output ancestor directory failed ownership or permission check"
                                .to_string(),
                        );
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Failed inspecting output path ancestor: {e}")),
        }
        cur = p.parent();
    }
    Ok(())
}

fn verify_safe_dir_ancestors(mut cur: Option<&Path>, euid: u32) -> bool {
    while let Some(anc) = cur {
        if anc.as_os_str().is_empty() {
            break;
        }
        let Ok(lmeta) = fs::symlink_metadata(anc) else {
            return false;
        };
        if lmeta.file_type().is_symlink() && lmeta.uid() != 0 {
            return false;
        }
        let Ok(dmeta) = fs::metadata(anc) else {
            return false;
        };
        let dmode = dmeta.permissions().mode();
        if !dmeta.is_dir()
            || (dmeta.uid() != euid && dmeta.uid() != 0)
            || !is_safe_dir_mode_and_group(dmode, dmeta.gid())
            || !has_only_safe_acls(anc, euid)
        {
            return false;
        }
        cur = anc.parent();
    }
    true
}

const fn is_safe_dir_mode_and_group(mode: u32, gid: u32) -> bool {
    let sticky = (mode & 0o1000) != 0;
    if sticky {
        return true;
    }
    if (mode & 0o002) != 0 {
        return false;
    }
    if (mode & 0o020) != 0 && gid != 0 && gid != 80 {
        return false;
    }
    true
}

fn is_dir_writable(dir: &Path) -> bool {
    let Ok(c_path) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(c_path.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

/// Allowlist check for Darwin extended ACLs (`<sys/acl.h>`): checked on EVERY directory (including sticky).
/// Distinguishes `ENOENT` (no extended ACL) from real errors (`EACCES`, `EIO`, `ENOMEM`), failing closed on errors.
pub fn has_only_safe_acls(dir: &Path, euid: u32) -> bool {
    let Ok(c_path) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe {
        *libc::__error() = 0;
    }
    let acl = unsafe { acl_get_link_np(c_path.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let err = io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return err == libc::ENOENT || err == 0;
    }
    let mut text_len: libc::ssize_t = 0;
    let text_ptr = unsafe { acl_to_text(acl, &mut text_len) };
    if text_ptr.is_null() {
        unsafe {
            acl_free(acl);
        }
        return false;
    }
    let text = unsafe { CStr::from_ptr(text_ptr).to_string_lossy().into_owned() };
    unsafe {
        acl_free(text_ptr.cast());
        acl_free(acl);
    }
    is_acl_text_allowlisted(&text, euid)
}

fn is_acl_text_allowlisted(acl_text: &str, euid: u32) -> bool {
    for raw_line in acl_text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("!#acl") {
            continue;
        }
        // Darwin acl_to_text format: <tag>:<uuid>:<name>:<id>:<allow|deny>[,<flag>...]:<perms>
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() != 6 {
            return false;
        }
        let tag = parts[0];
        let id_str = parts[3];
        let mut action_and_flags = parts[4].split(',');
        let base_action = action_and_flags.next().unwrap_or("");
        let has_inherit_or_flags = action_and_flags.any(|f| !f.trim().is_empty());
        if base_action == "deny" {
            continue;
        }
        if base_action != "allow" {
            return false;
        }
        if tag == "user" && id_str.parse::<u32>() == Ok(euid) {
            continue;
        }
        // Reject any third-party `allow` ACE carrying inheritance flags (e.g. allow,file_inherit:read)
        if has_inherit_or_flags {
            return false;
        }
        let perms = parts[5];
        let all_readonly = perms.split(',').all(|p| {
            matches!(
                p.trim(),
                "read"
                    | "list"
                    | "search"
                    | "execute"
                    | "readattr"
                    | "readextattr"
                    | "readsecurity"
            )
        });
        if !all_readonly {
            return false;
        }
    }
    true
}

/// Clears any inherited Darwin extended ACL on an open descriptor via `acl_init(0)` + `acl_set_fd_np`.
pub(crate) fn clear_fd_extended_acl(fd: libc::c_int) -> Result<(), String> {
    let empty = unsafe { acl_init(0) };
    if empty.is_null() {
        return Err("acl_init(0) failed".to_string());
    }
    let rc = unsafe { acl_set_fd_np(fd, empty, ACL_TYPE_EXTENDED) };
    let err = io::Error::last_os_error();
    unsafe {
        acl_free(empty);
    }
    if rc == 0 || err.raw_os_error() == Some(libc::ENOTSUP) {
        Ok(())
    } else {
        Err(format!("Failed clearing extended ACL on descriptor: {err}"))
    }
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

/// Generates a local-timestamped output path `Recording-YYYY-MM-DD-HHMMSS.<ext>` inside `dir`
/// without overwriting any existing file or symlink.
pub fn next_recording_output_path(dir: &Path, ext: &str) -> PathBuf {
    let clean_ext = ext.trim_start_matches('.');
    let stem = local_recording_stem(now_unix_secs());
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

fn local_recording_stem(unix_secs: u64) -> String {
    let ts = unix_secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&ts, &mut tm).is_null() };
    if ok {
        format!(
            "Recording-{:04}-{:02}-{:02}-{:02}{:02}{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    } else {
        format!("Recording-{unix_secs}")
    }
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
    let _ = ptr::null::<u8>();
    Ok(())
}

pub fn unique_private_file_path(prefix: &str, ext: &str) -> Result<PathBuf, String> {
    let dir = private_cache_dir()?;
    Ok(dir.join(format!("{prefix}-{}.{ext}", random_hex_token())))
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
        } else if meta.is_dir()
            && entry
                .file_name()
                .to_string_lossy()
                .starts_with(".qsr-work-")
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Strips ASCII/Unicode control characters as well as Unicode bidirectional override characters.
pub fn sanitize_terminal_text(input: &str, allow_newlines: bool) -> String {
    input
        .chars()
        .map(|c| {
            if is_bidi_control(c) {
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
    fn format_tilde_hides_home_prefix_and_desktop_is_usable() {
        if let Some(home) = valid_home_dir() {
            let desktop = home.join("Desktop");
            assert_eq!(format_tilde_path(&desktop), "~/Desktop");
            if desktop.is_dir() {
                assert!(is_usable_export_dir(&desktop));
            }
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
}
