use std::ffi::CString;
use std::fs::File;
use std::io;
use std::path::Path;

use crate::fs_safety::{clear_fd_extended_acl, random_hex_token};

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
