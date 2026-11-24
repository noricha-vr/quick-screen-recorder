use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::config::APP_SUPPORT_DIR_NAME;
use crate::fs_safety::{
    ensure_private_dir, format_tilde_path, private_cache_dir, random_hex_token, valid_home_dir,
};
use crate::staging::ExclusiveStagingDir;
use crate::staging::io_helpers::preserve_file_to_recovery_dir;

/// Returns a private (`0700`, non-auto-pruned) local recovery directory inside Application Support
/// so finished recordings are never lost if a removable/network destination disconnects mid-capture.
pub fn private_recovery_dir() -> Result<PathBuf, String> {
    let base = if let Some(home) = valid_home_dir() {
        let app_support = home
            .join("Library")
            .join("Application Support")
            .join(APP_SUPPORT_DIR_NAME);
        ensure_private_dir(&app_support)?;
        app_support.join("Recovered")
    } else {
        private_cache_dir()?.join("Recovered")
    };
    ensure_private_dir(&base)?;
    Ok(base)
}

/// Durably writes `.qsr-keep` with `b"1"` (`0600`) inside `dir_fd`, verifying 1-byte write and
/// `fsync` succeed (retrying `EINTR`). Used at recording-staging creation so later retention needs
/// zero writes/FDs even under ENOSPC, EIO, or EMFILE.
pub(super) fn write_keep_marker_durable(dir_fd: libc::c_int, excl: bool) -> io::Result<()> {
    let mut flags = libc::O_WRONLY | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    if excl {
        flags |= libc::O_EXCL;
    }
    let fd = unsafe { libc::openat(dir_fd, c".qsr-keep".as_ptr(), flags, 0o600_u32) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let write_res = loop {
        let rc = unsafe { libc::pwrite(fd, b"1".as_ptr().cast(), 1, 0) };
        if rc == 1 {
            break Ok(());
        }
        let err = io::Error::last_os_error();
        if rc < 0 && err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        break Err(if rc < 0 {
            err
        } else {
            io::Error::new(io::ErrorKind::WriteZero, "short write on .qsr-keep")
        });
    };
    let sync_res = write_res.and_then(|()| {
        if unsafe { libc::fsync(fd) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    });
    unsafe { libc::close(fd) };
    sync_res
}

/// Attempts to rename `.qsr-work-<suffix>` -> `qsr-preserved-<suffix>` outside cache prune globs.
pub(super) fn rename_work_dir_to_preserved(
    stage_dir_path: &mut PathBuf,
    staged_file: &mut PathBuf,
) {
    let (Some(parent), Some(old_name), Some(file_name)) = (
        stage_dir_path.parent(),
        stage_dir_path.file_name().and_then(|s| s.to_str()),
        staged_file.file_name(),
    ) else {
        return;
    };
    let Some(suffix) = old_name.strip_prefix(".qsr-work-") else {
        return;
    };
    let preserved_dir = parent.join(format!("qsr-preserved-{suffix}"));
    let (Ok(parent_fd), Ok(old_c), Ok(new_c)) = (
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(parent),
        CString::new(old_name.as_bytes()),
        CString::new(format!("qsr-preserved-{suffix}").as_bytes()),
    ) else {
        return;
    };
    let rc = unsafe {
        libc::renameatx_np(
            parent_fd.as_raw_fd(),
            old_c.as_ptr(),
            parent_fd.as_raw_fd(),
            new_c.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if rc == 0 {
        let new_staged = preserved_dir.join(file_name);
        *stage_dir_path = preserved_dir;
        *staged_file = new_staged;
    }
}

/// Commits a finalized staged MP4 to `output_path`. When `prevent_overwrite` is `true`, uses
/// exclusive non-overwriting publish (`RENAME_EXCL` / numeric suffix) so salvaged partial clips
/// can never replace an existing healthy file. Falls back to `recovery_dir`, and if that also
/// fails, retains staging exempt from cache pruning.
pub fn commit_or_recover_staging_with_policy(
    staging: &mut ExclusiveStagingDir,
    output_path: PathBuf,
    partial_error: Option<String>,
    prevent_overwrite: bool,
    recovery_dir: Result<PathBuf, String>,
) -> Result<(PathBuf, u64, Option<String>), String> {
    let commit_res = if prevent_overwrite {
        staging.commit_with_policy(true)
    } else {
        staging.commit().map(|size| (output_path, size))
    };
    match commit_res {
        Ok((committed_path, size)) => Ok((committed_path, size, partial_error)),
        Err(com_err) => {
            match recovery_dir.and_then(|rec_dir| staging.preserve_to_recovery_dir(&rec_dir)) {
                Ok((recovered_path, rec_size)) => {
                    let rec_display = format_tilde_path(&recovered_path);
                    let msg = match partial_error {
                        Some(cap_err) => format!(
                            "{cap_err}; also failed moving recording to destination ({com_err}). Saved locally instead: {rec_display}"
                        ),
                        None => format!(
                            "Destination became unavailable ({com_err}). Recording was saved locally instead: {rec_display}"
                        ),
                    };
                    Ok((recovered_path, rec_size, Some(msg)))
                }
                Err(rec_err) => {
                    let kept_path = staging.retain_staged_file();
                    let kept_display = format_tilde_path(&kept_path);
                    Err(match partial_error {
                        Some(cap_err) => format!(
                            "{cap_err}; failed committing partial video ({com_err}) and local recovery failed ({rec_err}). The finished MP4 was preserved in staging at: {kept_display}"
                        ),
                        None => format!(
                            "Failed saving recording to destination ({com_err}) and local recovery failed ({rec_err}). The finished MP4 was preserved in staging at: {kept_display}"
                        ),
                    })
                }
            }
        }
    }
}

pub fn commit_or_recover_staging(
    staging: &mut ExclusiveStagingDir,
    output_path: PathBuf,
    partial_error: Option<String>,
    recovery_dir: Result<PathBuf, String>,
) -> Result<(PathBuf, u64, Option<String>), String> {
    commit_or_recover_staging_with_policy(staging, output_path, partial_error, false, recovery_dir)
}

/// Convenience wrapper using the default private local recovery directory.
pub fn commit_or_recover_default(
    staging: &mut ExclusiveStagingDir,
    output_path: PathBuf,
    partial_error: Option<String>,
) -> Result<(PathBuf, u64, Option<String>), String> {
    commit_or_recover_staging_with_policy(
        staging,
        output_path,
        partial_error,
        false,
        private_recovery_dir(),
    )
}

/// Non-overwriting variant used when salvaging an aborted/partial recording so an existing target
/// video at `output_path` is never replaced.
pub fn commit_or_recover_salvaged_default(
    staging: &mut ExclusiveStagingDir,
    output_path: PathBuf,
    partial_error: Option<String>,
) -> Result<(PathBuf, u64, Option<String>), String> {
    commit_or_recover_staging_with_policy(
        staging,
        output_path,
        partial_error,
        true,
        private_recovery_dir(),
    )
}

/// Permanently preserves a completed staged MP4 directly into `recovery_dir` (never into
/// `output_path` / GUI `temp_files`). If `recovery_dir` fails, retains staging exempt from
/// drop deletion and 24-hour cache pruning, returning `Err((reason, surviving_path))`.
pub fn preserve_staged_permanently(
    staging: &mut ExclusiveStagingDir,
    recovery_dir: Result<PathBuf, String>,
) -> Result<PathBuf, (String, PathBuf)> {
    match recovery_dir.and_then(|rec_dir| staging.preserve_to_recovery_dir(&rec_dir)) {
        Ok((recovered_path, _)) => Ok(recovered_path),
        Err(rec_err) => {
            let kept = staging.retain_staged_file();
            Err((rec_err, kept))
        }
    }
}

/// Rescues a finished GUI capture out of `temp_files` into `recovery_dir` so shutdown cleanup,
/// OS reboots, and 24-hour cache pruning never delete it. If `recovery_dir` is unavailable,
/// renames the file in place to `qsr-preserved-*` and returns `Err((reason, surviving_path))`.
pub fn preserve_capture_to_recovery(
    output_path: &Path,
    temp_files: &mut Vec<PathBuf>,
    recovery_dir: Result<PathBuf, String>,
    stem_hint: &str,
) -> Result<PathBuf, (String, PathBuf)> {
    let was_temp = temp_files.iter().any(|p| p == output_path);
    temp_files.retain(|p| p != output_path);
    if !was_temp {
        return Ok(output_path.to_path_buf());
    }
    let rec_res = recovery_dir
        .and_then(|rec_dir| preserve_file_to_recovery_dir(output_path, &rec_dir, stem_hint));
    match rec_res {
        Ok(saved) => Ok(saved),
        Err(err) => {
            let surviving = rename_file_as_preserved_in_place(output_path)
                .unwrap_or_else(|| output_path.to_path_buf());
            Err((err, surviving))
        }
    }
}

pub fn preserve_recording_on_quit(
    output_path: &Path,
    temp_files: &mut Vec<PathBuf>,
    recovery_dir: Result<PathBuf, String>,
) -> Result<PathBuf, (String, PathBuf)> {
    preserve_capture_to_recovery(
        output_path,
        temp_files,
        recovery_dir,
        "Quit-Saved-Recording",
    )
}

fn rename_file_as_preserved_in_place(path: &Path) -> Option<PathBuf> {
    let (parent, old_name) = (path.parent()?, path.file_name()?);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("mp4");
    let new_name = format!("qsr-preserved-{}.{ext}", &random_hex_token()[..12]);
    let parent_fd = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)
        .ok()?;
    let (old_c, new_c) = (
        CString::new(old_name.as_bytes()).ok()?,
        CString::new(new_name.as_bytes()).ok()?,
    );
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    let rc = unsafe {
        libc::renameatx_np(
            parent_fd.as_raw_fd(),
            old_c.as_ptr(),
            parent_fd.as_raw_fd(),
            new_c.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    (rc == 0).then(|| parent.join(new_name))
}
