use std::ffi::{CStr, CString};
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::ptr;

use crate::config::APP_SUPPORT_DIR_NAME;
use crate::export_name::APP_DISPLAY_NAME;
use crate::fs_safety::ensure_private_dir;

const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;

unsafe extern "C" {
    fn acl_get_link_np(path: *const libc::c_char, acl_type: libc::c_int) -> *mut libc::c_void;
    fn acl_init(count: libc::c_int) -> *mut libc::c_void;
    fn acl_set_fd_np(fd: libc::c_int, acl: *mut libc::c_void, acl_type: libc::c_int)
    -> libc::c_int;
    fn acl_to_text(acl: *mut libc::c_void, len_p: *mut libc::ssize_t) -> *mut libc::c_char;
    fn acl_free(obj_p: *mut libc::c_void) -> libc::c_int;
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

/// Returns `~/Movies/Quick Screen Recorder` (creating it privately if needed) when available as a
/// verified writable persistent directory owned by the current user, falling back safely to
/// `~/Movies`, `~/Desktop`, `HOME`, or `~/Library/Application Support/QuickScreenRecorder/Exports`.
/// Never falls back to `/tmp` or auto-pruned cache directories (`None` prompts user to pick a folder).
pub fn default_movies_export_dir() -> Option<PathBuf> {
    let home = valid_home_dir()?;
    default_export_dir_in_home(&home)
}

pub fn default_export_dir_in_home(home: &Path) -> Option<PathBuf> {
    let movies = home.join("Movies");
    if is_usable_export_dir(&movies) {
        let app_dir = movies.join(APP_DISPLAY_NAME);
        match fs::symlink_metadata(&app_dir) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let _ = DirBuilder::new().mode(0o700).create(&app_dir);
            }
            Err(_) => {}
        }
        if is_usable_export_dir(&app_dir) {
            return Some(app_dir);
        }
        return Some(movies);
    }
    let desktop = home.join("Desktop");
    if is_usable_export_dir(&desktop) {
        return Some(desktop);
    }
    if is_usable_export_dir(home) {
        return Some(home.to_path_buf());
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

pub(crate) fn is_acl_text_allowlisted(acl_text: &str, euid: u32) -> bool {
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
