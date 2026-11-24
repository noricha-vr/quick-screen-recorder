use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::export_name::sanitize_export_stem;
use crate::fs_safety::{clear_fd_extended_acl, ensure_private_dir, random_hex_token};

pub(super) fn fstatat_overwrite_mode(
    dir_fd: libc::c_int,
    name_c: &CString,
    euid: u32,
) -> Result<u32, String> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstatat(dir_fd, name_c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
    if rc == 0 {
        let ftype = st.st_mode & libc::S_IFMT;
        if ftype != libc::S_IFREG || st.st_uid != euid {
            return Err(
                "Refusing to overwrite symlink, non-regular file, or file owned by another user"
                    .to_string(),
            );
        }
        Ok(u32::from(st.st_mode) & 0o777)
    } else {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOENT) {
            Ok(0o600)
        } else {
            Err(format!("Failed inspecting destination entry: {err}"))
        }
    }
}

pub(super) fn stage_once_on_dest_volume(
    staged: &Path,
    dest_dir_fd: libc::c_int,
) -> Result<CString, String> {
    let tmp_name = format!(".qsr-xdev-{}", random_hex_token());
    let tmp_c = CString::new(tmp_name.as_bytes()).unwrap();
    let mut src = File::open(staged).map_err(|e| format!("Failed opening staged file: {e}"))?;
    let fd = unsafe {
        libc::openat(
            dest_dir_fd,
            tmp_c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600_u32,
        )
    };
    if fd < 0 {
        return Err(format!(
            "Failed creating cross-volume staging file: {}",
            io::Error::last_os_error()
        ));
    }
    if let Err(e) = clear_fd_extended_acl(fd) {
        unsafe {
            libc::close(fd);
            libc::unlinkat(dest_dir_fd, tmp_c.as_ptr(), 0);
        }
        return Err(e);
    }
    if unsafe { libc::fchmod(fd, 0o600 as libc::mode_t) } != 0 {
        let err = io::Error::last_os_error();
        unsafe {
            libc::close(fd);
            libc::unlinkat(dest_dir_fd, tmp_c.as_ptr(), 0);
        }
        return Err(format!(
            "Failed setting 0600 permissions on cross-volume staging file: {err}"
        ));
    }
    let mut out_file = unsafe { File::from_owned_fd_compat(fd) };
    if let Err(e) = io::copy(&mut src, &mut out_file).and_then(|_| out_file.sync_all()) {
        drop(out_file);
        unsafe { libc::unlinkat(dest_dir_fd, tmp_c.as_ptr(), 0) };
        return Err(format!("Failed writing cross-volume output file: {e}"));
    }
    Ok(tmp_c)
}

pub(super) fn rename_excl_within_dir(
    dir_fd: libc::c_int,
    tmp_c: &CString,
    name_c: &CString,
) -> Result<bool, String> {
    let rc = unsafe {
        libc::renameatx_np(
            dir_fd,
            tmp_c.as_ptr(),
            dir_fd,
            name_c.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if rc == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    if matches!(
        err.raw_os_error(),
        Some(libc::EEXIST | libc::EISDIR | libc::ENOTEMPTY)
    ) {
        Ok(false)
    } else {
        Err(format!("Failed publishing cross-volume output file: {err}"))
    }
}

pub(super) struct XdevTempGuard {
    pub(super) dir_fd: libc::c_int,
    pub(super) name_c: Option<CString>,
}

impl Drop for XdevTempGuard {
    fn drop(&mut self) {
        if let Some(ref tmp_c) = self.name_c {
            unsafe {
                libc::unlinkat(self.dir_fd, tmp_c.as_ptr(), 0);
            }
        }
    }
}

pub(super) fn copy_into_pinned_dir_overwrite(
    staged: &Path,
    dest_dir_fd: libc::c_int,
    name_c: &CString,
    mode: u32,
) -> Result<(), String> {
    let tmp_name = format!(".qsr-xdev-{}", random_hex_token());
    let tmp_c = CString::new(tmp_name.as_bytes()).unwrap();
    let mut src = File::open(staged).map_err(|e| format!("Failed opening staged file: {e}"))?;
    let fd = unsafe {
        libc::openat(
            dest_dir_fd,
            tmp_c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err(format!(
            "Failed creating cross-volume temp file: {}",
            io::Error::last_os_error()
        ));
    }
    if let Err(e) = clear_fd_extended_acl(fd) {
        unsafe {
            libc::close(fd);
            libc::unlinkat(dest_dir_fd, tmp_c.as_ptr(), 0);
        }
        return Err(e);
    }
    if unsafe { libc::fchmod(fd, mode as libc::mode_t) } != 0 {
        let err = io::Error::last_os_error();
        unsafe {
            libc::close(fd);
            libc::unlinkat(dest_dir_fd, tmp_c.as_ptr(), 0);
        }
        return Err(format!(
            "Failed preserving permissions on cross-volume file: {err}"
        ));
    }
    let mut out_file = unsafe { File::from_owned_fd_compat(fd) };
    if let Err(e) = io::copy(&mut src, &mut out_file).and_then(|_| out_file.sync_all()) {
        drop(out_file);
        unsafe { libc::unlinkat(dest_dir_fd, tmp_c.as_ptr(), 0) };
        return Err(format!("Failed copying cross-volume output file: {e}"));
    }
    drop(out_file);
    let rc = unsafe { libc::renameat(dest_dir_fd, tmp_c.as_ptr(), dest_dir_fd, name_c.as_ptr()) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        unsafe { libc::unlinkat(dest_dir_fd, tmp_c.as_ptr(), 0) };
        return Err(format!("Failed committing cross-volume output file: {err}"));
    }
    Ok(())
}

trait FromRawFdCompat {
    unsafe fn from_owned_fd_compat(fd: libc::c_int) -> Self;
}

impl FromRawFdCompat for File {
    unsafe fn from_owned_fd_compat(fd: libc::c_int) -> Self {
        use std::os::fd::FromRawFd;
        unsafe { Self::from_raw_fd(fd) }
    }
}

/// Atomically moves an already-written local recording into a verified private recovery directory
/// (`0700` dir, `0600` file, `O_NOFOLLOW`, `RENAME_EXCL`), falling back safely across volumes.
pub fn preserve_file_to_recovery_dir(
    src_path: &Path,
    recovery_dir: &Path,
    stem_hint: &str,
) -> Result<PathBuf, String> {
    let euid = unsafe { libc::geteuid() };
    let src_meta = fs::symlink_metadata(src_path)
        .map_err(|e| format!("Recorded file missing before recovery: {e}"))?;
    if src_meta.file_type().is_symlink()
        || !src_meta.is_file()
        || src_meta.uid() != euid
        || src_meta.len() == 0
    {
        return Err("Recorded file is empty or invalid".to_string());
    }
    let src_parent = src_path
        .parent()
        .ok_or("Recorded file is missing parent directory")?;
    let src_name = src_path
        .file_name()
        .ok_or("Recorded file is missing filename")?;
    let src_name_c = CString::new(src_name.as_bytes()).map_err(|_| "Invalid source filename")?;
    let src_dir_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(src_parent)
        .map_err(|e| format!("Failed opening source directory descriptor: {e}"))?;

    ensure_private_dir(recovery_dir)?;
    let canon_rec = fs::canonicalize(recovery_dir)
        .map_err(|e| format!("Failed resolving recovery directory: {e}"))?;
    let rec_dir_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&canon_rec)
        .map_err(|e| format!("Failed pinning recovery directory descriptor: {e}"))?;
    let rec_meta = rec_dir_file
        .metadata()
        .map_err(|e| format!("Failed inspecting recovery directory: {e}"))?;
    if !rec_meta.is_dir() || rec_meta.uid() != euid || (rec_meta.permissions().mode() & 0o077 != 0)
    {
        return Err("Recovery directory failed 0700 ownership check".to_string());
    }

    fs::set_permissions(src_path, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("Failed setting 0600 permissions on recording: {e}"))?;

    let ext = src_path
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("mp4");
    let stem = sanitize_export_stem(stem_hint, ext, "recording");
    let rec_name = format!("{stem}-recovered-{}.{ext}", &random_hex_token()[..12]);
    let rec_name_c = CString::new(rec_name.as_bytes()).map_err(|_| "Invalid recovery name")?;

    let rc = unsafe {
        libc::renameatx_np(
            src_dir_file.as_raw_fd(),
            src_name_c.as_ptr(),
            rec_dir_file.as_raw_fd(),
            rec_name_c.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if rc == 0 {
        return Ok(canon_rec.join(rec_name));
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EXDEV) {
        let tmp_c = stage_once_on_dest_volume(src_path, rec_dir_file.as_raw_fd())?;
        let mut guard = XdevTempGuard {
            dir_fd: rec_dir_file.as_raw_fd(),
            name_c: Some(tmp_c),
        };
        if rename_excl_within_dir(
            rec_dir_file.as_raw_fd(),
            guard.name_c.as_ref().unwrap(),
            &rec_name_c,
        )? {
            guard.name_c = None;
            unsafe {
                libc::unlinkat(src_dir_file.as_raw_fd(), src_name_c.as_ptr(), 0);
            }
            return Ok(canon_rec.join(rec_name));
        }
        return Err("Recovery filename unexpectedly collided".to_string());
    }
    Err(format!(
        "Failed moving recording to recovery directory: {err}"
    ))
}
