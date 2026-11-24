use std::ffi::CString;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::export_name::sanitize_export_stem;
use crate::fs_safety::{
    clear_fd_extended_acl, ensure_private_dir, has_only_safe_acls, is_usable_export_dir,
    private_cache_dir, random_hex_token, verify_pre_canonical_target_parent,
};

#[path = "staging_io.rs"]
mod io_helpers;

use io_helpers::{
    XdevTempGuard, copy_into_pinned_dir_overwrite, create_initial_keep_marker,
    fstatat_overwrite_mode, rename_excl_within_dir, stage_once_on_dest_volume,
};
pub use io_helpers::{
    commit_or_recover_default, commit_or_recover_salvaged_default, commit_or_recover_staging,
    commit_or_recover_staging_with_policy, preserve_capture_to_recovery,
    preserve_recording_on_quit, preserve_staged_permanently, private_recovery_dir,
};

/// Exclusively created (`0700`) staging directory inside our private cache directory, paired with
/// a pinned `O_DIRECTORY | O_NOFOLLOW` descriptor for the verified destination directory.
/// Encoders never write temporary files into the destination directory itself.
pub struct ExclusiveStagingDir {
    stage_dir_path: PathBuf,
    stage_dir_fd: OwnedFd,
    keep_marker_fd: OwnedFd,
    staged_name_c: CString,
    staged_file: PathBuf,
    dest_dir_fd: OwnedFd,
    dest_parent_dev: u64,
    dest_parent_ino: u64,
    canon_dest: PathBuf,
    keep_staged_on_drop: bool,
}

impl ExclusiveStagingDir {
    pub fn create_for_target(target: &Path, ext: &str) -> Result<Self, String> {
        Self::create_for_target_with_policy(target, ext, false)
    }

    pub fn create_for_target_with_policy(
        target: &Path,
        ext: &str,
        prevent_overwrite: bool,
    ) -> Result<Self, String> {
        let cache_root = private_cache_dir()?;
        Self::create_in_cache_root(&cache_root, target, ext, prevent_overwrite)
    }

    pub(crate) fn create_in_cache_root(
        cache_root: &Path,
        target: &Path,
        ext: &str,
        prevent_overwrite: bool,
    ) -> Result<Self, String> {
        let file_name = target
            .file_name()
            .ok_or("Target path is missing a file name")?;
        if !prevent_overwrite {
            if let Ok(sym) = fs::symlink_metadata(target) {
                if sym.file_type().is_symlink() {
                    return Err("Refusing to overwrite a symlink target path".to_string());
                }
            }
        }
        let parent = match target.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => std::env::current_dir()
                .map_err(|e| format!("Cannot resolve working directory: {e}"))?,
        };

        let euid = unsafe { libc::geteuid() };
        verify_pre_canonical_target_parent(&parent, euid)?;
        if !parent.exists() {
            fs::create_dir_all(&parent)
                .map_err(|e| format!("Failed creating output directory: {e}"))?;
        }
        verify_pre_canonical_target_parent(&parent, euid)?;

        let canon_parent = fs::canonicalize(&parent)
            .map_err(|e| format!("Failed resolving canonical output directory: {e}"))?;
        if !is_usable_export_dir(&canon_parent) {
            return Err(
                "Output directory failed ownership, group/ACL, writability, or symlink check"
                    .to_string(),
            );
        }

        let dest_dir_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&canon_parent)
            .map_err(|e| format!("Failed pinning output directory descriptor: {e}"))?;
        let dest_meta = dest_dir_file
            .metadata()
            .map_err(|e| format!("Failed inspecting pinned output directory: {e}"))?;
        let dest_parent_dev = dest_meta.dev();
        let dest_parent_ino = dest_meta.ino();
        let dest_dir_fd = OwnedFd::from(dest_dir_file);

        let canon_dest = canon_parent.join(file_name);
        if !prevent_overwrite {
            if let Ok(existing) = fs::symlink_metadata(&canon_dest) {
                if existing.file_type().is_symlink()
                    || !existing.is_file()
                    || existing.uid() != euid
                {
                    return Err(
                        "Refusing to overwrite symlink, special file, or file owned by another user"
                            .to_string(),
                    );
                }
            }
        }

        // Stage inside our private 0700 cache dir so destination group/ACL peers can never touch staged files
        let stage_dir_path = cache_root.join(format!(".qsr-work-{}", random_hex_token()));
        DirBuilder::new()
            .mode(0o700)
            .create(&stage_dir_path)
            .map_err(|e| format!("Failed creating exclusive 0700 staging directory: {e}"))?;

        let stage_dir_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&stage_dir_path)
            .map_err(|e| format!("Failed opening staging directory descriptor: {e}"))?;
        // Hold a kernel advisory lock for the lifetime of this session so cache pruning never deletes active exports
        if unsafe { libc::flock(stage_dir_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let _ = fs::remove_dir_all(&stage_dir_path);
            return Err("Failed acquiring exclusive lock on staging directory".to_string());
        }
        if let Err(e) = clear_fd_extended_acl(stage_dir_file.as_raw_fd()) {
            let _ = fs::remove_dir_all(&stage_dir_path);
            return Err(e);
        }
        let dir_meta = stage_dir_file
            .metadata()
            .map_err(|e| format!("Failed verifying staging directory metadata: {e}"))?;
        if !dir_meta.is_dir()
            || dir_meta.uid() != euid
            || (dir_meta.permissions().mode() & 0o077 != 0)
            || !has_only_safe_acls(&stage_dir_path, euid)
        {
            let _ = fs::remove_dir_all(&stage_dir_path);
            return Err("Staging directory ownership, 0700 mode, or ACL check failed".to_string());
        }
        let stage_dir_fd = OwnedFd::from(stage_dir_file);
        // Pre-open `.qsr-keep` initialized to `b"0"` (crash-prunable) so `retain_staged_in_place()`
        // can flip it to `b"1"` with 0 new FDs and 0 dir writes even under EMFILE/read-only dir.
        let keep_marker_fd = match create_initial_keep_marker(stage_dir_fd.as_raw_fd()) {
            Ok(fd) => fd,
            Err(err) => {
                let _ = fs::remove_dir_all(&stage_dir_path);
                return Err(format!("Failed pre-allocating staging keep marker: {err}"));
            }
        };

        let clean_ext = ext.trim_start_matches('.');
        let staged_name = format!("staged.{clean_ext}");
        let staged_name_c =
            CString::new(staged_name.as_bytes()).map_err(|_| "Invalid extension")?;
        let staged_file = stage_dir_path.join(&staged_name);

        Ok(Self {
            stage_dir_path,
            stage_dir_fd,
            keep_marker_fd,
            staged_name_c,
            staged_file,
            dest_dir_fd,
            dest_parent_dev,
            dest_parent_ino,
            canon_dest,
            keep_staged_on_drop: false,
        })
    }

    pub fn staged_file(&self) -> &Path {
        &self.staged_file
    }

    pub fn commit(&self) -> Result<u64, String> {
        self.commit_with_policy(false).map(|(_, len)| len)
    }

    pub fn commit_with_policy(&self, prevent_overwrite: bool) -> Result<(PathBuf, u64), String> {
        let euid = unsafe { libc::geteuid() };
        let staged_meta = fs::symlink_metadata(&self.staged_file)
            .map_err(|e| format!("Staged output file missing: {e}"))?;
        if staged_meta.file_type().is_symlink()
            || !staged_meta.is_file()
            || staged_meta.uid() != euid
            || staged_meta.len() == 0
        {
            return Err("Staged output file is empty or invalid".to_string());
        }

        let Some(canon_parent) = self.canon_dest.parent() else {
            return Err("Destination parent missing".to_string());
        };
        if !is_usable_export_dir(canon_parent) {
            return Err("Destination directory failed safety re-verification".to_string());
        }
        let live_parent_meta = fs::symlink_metadata(canon_parent)
            .map_err(|e| format!("Destination directory disappeared: {e}"))?;
        if live_parent_meta.dev() != self.dest_parent_dev
            || live_parent_meta.ino() != self.dest_parent_ino
        {
            return Err("Destination directory identity changed during export".to_string());
        }

        if prevent_overwrite {
            fs::set_permissions(&self.staged_file, fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("Failed setting output file permissions: {e}"))?;
            let final_name = self.commit_exclusive_into_pinned_dir()?;
            return Ok((canon_parent.join(final_name), staged_meta.len()));
        }

        let dest_name = self
            .canon_dest
            .file_name()
            .ok_or("Destination filename missing")?;
        let dest_name_c = CString::new(dest_name.as_bytes()).map_err(|_| "Invalid filename")?;
        let desired_mode =
            fstatat_overwrite_mode(self.dest_dir_fd.as_raw_fd(), &dest_name_c, euid)?;
        fs::set_permissions(&self.staged_file, fs::Permissions::from_mode(desired_mode))
            .map_err(|e| format!("Failed setting output file permissions: {e}"))?;

        let rc = unsafe {
            libc::renameat(
                self.stage_dir_fd.as_raw_fd(),
                self.staged_name_c.as_ptr(),
                self.dest_dir_fd.as_raw_fd(),
                dest_name_c.as_ptr(),
            )
        };
        if rc == 0 {
            return Ok((self.canon_dest.clone(), staged_meta.len()));
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EXDEV) {
            copy_into_pinned_dir_overwrite(
                &self.staged_file,
                self.dest_dir_fd.as_raw_fd(),
                &dest_name_c,
                desired_mode,
            )?;
            return Ok((self.canon_dest.clone(), staged_meta.len()));
        }
        Err(format!("Failed moving staged file to destination: {err}"))
    }

    fn commit_exclusive_into_pinned_dir(&self) -> Result<String, String> {
        let initial_name = self
            .canon_dest
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("Invalid UTF-8 output filename")?;
        let mut xdev_guard = XdevTempGuard {
            dir_fd: self.dest_dir_fd.as_raw_fd(),
            name_c: None,
        };
        if self.try_publish_excl(initial_name, &mut xdev_guard)? {
            return Ok(initial_name.to_string());
        }

        let ext = self
            .canon_dest
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp4");
        let raw_stem = self
            .canon_dest
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Quick-Screen-Recorder");

        for suffix in 2..=9999_u32 {
            let candidate = format!("{raw_stem}-{suffix}.{ext}");
            if self.try_publish_excl(&candidate, &mut xdev_guard)? {
                return Ok(candidate);
            }
        }
        let fallback = format!("{raw_stem}-{}.{ext}", random_hex_token());
        if self.try_publish_excl(&fallback, &mut xdev_guard)? {
            Ok(fallback)
        } else {
            Err("Failed finding non-colliding output filename".to_string())
        }
    }

    fn try_publish_excl(
        &self,
        file_name: &str,
        xdev_guard: &mut XdevTempGuard,
    ) -> Result<bool, String> {
        let name_c = CString::new(file_name.as_bytes()).map_err(|_| "Invalid target filename")?;
        if let Some(tmp_c) = xdev_guard.name_c.as_ref() {
            let published = rename_excl_within_dir(self.dest_dir_fd.as_raw_fd(), tmp_c, &name_c)?;
            if published {
                xdev_guard.name_c = None;
            }
            return Ok(published);
        }
        let rc = unsafe {
            libc::renameatx_np(
                self.stage_dir_fd.as_raw_fd(),
                self.staged_name_c.as_ptr(),
                self.dest_dir_fd.as_raw_fd(),
                name_c.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        if rc == 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EEXIST | libc::EISDIR | libc::ENOTEMPTY) => Ok(false),
            Some(libc::EXDEV) => {
                let tmp_c =
                    stage_once_on_dest_volume(&self.staged_file, self.dest_dir_fd.as_raw_fd())?;
                xdev_guard.name_c = Some(tmp_c);
                let published = rename_excl_within_dir(
                    self.dest_dir_fd.as_raw_fd(),
                    xdev_guard.name_c.as_ref().unwrap(),
                    &name_c,
                )?;
                if published {
                    xdev_guard.name_c = None;
                }
                Ok(published)
            }
            _ => Err(format!("Atomic exclusive rename failed: {err}")),
        }
    }

    /// Preserves a completed staged file into a verified private local recovery directory (`0700`
    /// dir, `0600` file, `O_NOFOLLOW`, `RENAME_EXCL`) when committing to the original destination
    /// fails (for example, an external drive disconnected during a multi-hour timelapse).
    pub fn preserve_to_recovery_dir(&self, recovery_dir: &Path) -> Result<(PathBuf, u64), String> {
        let euid = unsafe { libc::geteuid() };
        let staged_meta = fs::symlink_metadata(&self.staged_file)
            .map_err(|e| format!("Staged output file missing: {e}"))?;
        if staged_meta.file_type().is_symlink()
            || !staged_meta.is_file()
            || staged_meta.uid() != euid
            || staged_meta.len() == 0
        {
            return Err("Staged output file is empty or invalid".to_string());
        }

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
        if !rec_meta.is_dir()
            || rec_meta.uid() != euid
            || (rec_meta.permissions().mode() & 0o077 != 0)
        {
            return Err("Recovery directory failed 0700 ownership check".to_string());
        }

        fs::set_permissions(&self.staged_file, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("Failed setting 0600 permissions on recovered file: {e}"))?;

        let ext = self
            .canon_dest
            .extension()
            .and_then(|e| e.to_str())
            .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or("mp4");
        let raw_stem = self
            .canon_dest
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("recording");
        let stem = sanitize_export_stem(raw_stem, ext, "recording");
        let rec_name = format!("{stem}-recovered-{}.{ext}", &random_hex_token()[..12]);
        let rec_name_c = CString::new(rec_name.as_bytes()).map_err(|_| "Invalid recovery name")?;

        let rc = unsafe {
            libc::renameatx_np(
                self.stage_dir_fd.as_raw_fd(),
                self.staged_name_c.as_ptr(),
                rec_dir_file.as_raw_fd(),
                rec_name_c.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        if rc == 0 {
            return Ok((canon_rec.join(rec_name), staged_meta.len()));
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EXDEV) {
            let tmp_c = stage_once_on_dest_volume(&self.staged_file, rec_dir_file.as_raw_fd())?;
            let mut guard = XdevTempGuard {
                dir_fd: rec_dir_file.as_raw_fd(),
                name_c: Some(tmp_c),
            };
            let published = rename_excl_within_dir(
                rec_dir_file.as_raw_fd(),
                guard.name_c.as_ref().unwrap(),
                &rec_name_c,
            )?;
            if published {
                guard.name_c = None;
                return Ok((canon_rec.join(rec_name), staged_meta.len()));
            }
            return Err("Recovery filename unexpectedly collided".to_string());
        }
        Err(format!(
            "Failed moving staged file to recovery directory: {err}"
        ))
    }

    /// Disarms drop cleanup and flips the pre-opened `.qsr-keep` marker to `b"1"` via `pwrite`
    /// (requires 0 new FDs and 0 directory writes, surviving EMFILE and read-only parent dirs).
    pub fn retain_staged_in_place(&mut self) -> PathBuf {
        let _ = fs::set_permissions(&self.staged_file, fs::Permissions::from_mode(0o600));
        let _ =
            unsafe { libc::pwrite(self.keep_marker_fd.as_raw_fd(), b"1".as_ptr().cast(), 1, 0) };
        self.keep_staged_on_drop = true;
        self.staged_file.clone()
    }

    /// Disarms drop cleanup, marks `.qsr-keep`, and renames `.qsr-work-*` -> `qsr-preserved-*`.
    pub fn retain_staged_file(&mut self) -> PathBuf {
        self.retain_staged_in_place();
        if let (Some(parent), Some(old_name), Some(file_name)) = (
            self.stage_dir_path.parent(),
            self.stage_dir_path.file_name().and_then(|s| s.to_str()),
            self.staged_file.file_name(),
        ) {
            if let Some(suffix) = old_name.strip_prefix(".qsr-work-") {
                let preserved_dir = parent.join(format!("qsr-preserved-{suffix}"));
                if let (Ok(parent_fd), Ok(old_c), Ok(new_c)) = (
                    OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                        .open(parent),
                    CString::new(old_name.as_bytes()),
                    CString::new(format!("qsr-preserved-{suffix}").as_bytes()),
                ) {
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
                        self.stage_dir_path = preserved_dir;
                        self.staged_file = new_staged;
                    }
                }
            }
        }
        self.staged_file.clone()
    }
}

impl Drop for ExclusiveStagingDir {
    fn drop(&mut self) {
        if !self.keep_staged_on_drop {
            let _ = fs::remove_dir_all(&self.stage_dir_path);
        }
    }
}

#[cfg(test)]
#[path = "staging_tests.rs"]
mod tests;
