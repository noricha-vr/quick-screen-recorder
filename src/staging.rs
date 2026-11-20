use std::ffi::CString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::fs_safety::{
    is_usable_export_dir, private_cache_dir, random_hex_token, strip_extended_acl,
    verify_pre_canonical_target_parent,
};

/// Exclusively created (`0700`) staging directory inside our private cache directory, paired with
/// a pinned `O_DIRECTORY | O_NOFOLLOW` descriptor for the verified destination directory.
/// Encoders never write temporary files into the destination directory itself.
pub struct ExclusiveStagingDir {
    stage_dir_path: PathBuf,
    stage_dir_fd: OwnedFd,
    staged_name_c: CString,
    staged_file: PathBuf,
    dest_dir_fd: OwnedFd,
    dest_parent_dev: u64,
    dest_parent_ino: u64,
    canon_dest: PathBuf,
}

impl ExclusiveStagingDir {
    pub fn create_for_target(target: &Path, ext: &str) -> Result<Self, String> {
        let file_name = target
            .file_name()
            .ok_or("Target path is missing a file name")?;
        if let Ok(sym) = fs::symlink_metadata(target) {
            if sym.file_type().is_symlink() {
                return Err("Refusing to overwrite a symlink target path".to_string());
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
        if let Ok(existing) = fs::symlink_metadata(&canon_dest) {
            if existing.file_type().is_symlink() || !existing.is_file() || existing.uid() != euid {
                return Err(
                    "Refusing to overwrite symlink, special file, or file owned by another user"
                        .to_string(),
                );
            }
        }

        // Stage inside our private 0700 cache dir so destination group/ACL peers can never touch staged files
        let cache_root = private_cache_dir()?;
        let stage_dir_path = cache_root.join(format!(".qsr-work-{}", random_hex_token()));
        DirBuilder::new()
            .mode(0o700)
            .create(&stage_dir_path)
            .map_err(|e| format!("Failed creating exclusive 0700 staging directory: {e}"))?;
        strip_extended_acl(&stage_dir_path);

        let stage_dir_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&stage_dir_path)
            .map_err(|e| format!("Failed opening staging directory descriptor: {e}"))?;
        let dir_meta = stage_dir_file
            .metadata()
            .map_err(|e| format!("Failed verifying staging directory metadata: {e}"))?;
        if !dir_meta.is_dir()
            || dir_meta.uid() != euid
            || (dir_meta.permissions().mode() & 0o077 != 0)
        {
            let _ = fs::remove_dir_all(&stage_dir_path);
            return Err("Staging directory ownership or 0700 mode check failed".to_string());
        }
        let stage_dir_fd = OwnedFd::from(stage_dir_file);

        let clean_ext = ext.trim_start_matches('.');
        let staged_name = format!("staged.{clean_ext}");
        let staged_name_c =
            CString::new(staged_name.as_bytes()).map_err(|_| "Invalid extension")?;
        let staged_file = stage_dir_path.join(&staged_name);

        Ok(Self {
            stage_dir_path,
            stage_dir_fd,
            staged_name_c,
            staged_file,
            dest_dir_fd,
            dest_parent_dev,
            dest_parent_ino,
            canon_dest,
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
        if self.try_move_or_copy_excl(initial_name)? {
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
            .unwrap_or("Recording");
        let base_stem = strip_numeric_collision_suffix(raw_stem);

        for suffix in 2..=9999_u32 {
            let candidate = format!("{base_stem}-{suffix}.{ext}");
            if self.try_move_or_copy_excl(&candidate)? {
                return Ok(candidate);
            }
        }
        let fallback = format!("{base_stem}-{}.{ext}", random_hex_token());
        if self.try_move_or_copy_excl(&fallback)? {
            Ok(fallback)
        } else {
            Err("Failed finding non-colliding output filename".to_string())
        }
    }

    fn try_move_or_copy_excl(&self, file_name: &str) -> Result<bool, String> {
        let name_c = CString::new(file_name.as_bytes()).map_err(|_| "Invalid target filename")?;
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
            Some(libc::EEXIST) => Ok(false),
            Some(libc::EXDEV) => {
                copy_into_pinned_dir_excl(&self.staged_file, self.dest_dir_fd.as_raw_fd(), &name_c)
            }
            _ => Err(format!("Atomic exclusive rename failed: {err}")),
        }
    }
}

impl Drop for ExclusiveStagingDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.stage_dir_path);
    }
}

fn strip_numeric_collision_suffix(stem: &str) -> &str {
    if let Some((prefix, tail)) = stem.rsplit_once('-') {
        if (1..=4).contains(&tail.len()) && tail.chars().all(|c| c.is_ascii_digit()) {
            let looks_like_date =
                prefix.len() >= 7 && prefix.as_bytes()[prefix.len() - 3] == b'-' && tail.len() == 2;
            if !looks_like_date {
                return prefix;
            }
        }
    }
    stem
}

fn fstatat_overwrite_mode(dir_fd: libc::c_int, name_c: &CString, euid: u32) -> Result<u32, String> {
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

fn copy_into_pinned_dir_excl(
    staged: &Path,
    dest_dir_fd: libc::c_int,
    name_c: &CString,
) -> Result<bool, String> {
    let fd = unsafe {
        libc::openat(
            dest_dir_fd,
            name_c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600_u32,
        )
    };
    if fd < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EEXIST) {
            return Ok(false);
        }
        return Err(format!("Failed creating cross-volume output file: {err}"));
    }
    let mut out_file = unsafe { File::from_owned_fd_compat(fd) };
    let mut src = File::open(staged).map_err(|e| format!("Failed opening staged file: {e}"))?;
    if let Err(e) = io::copy(&mut src, &mut out_file).and_then(|_| out_file.sync_all()) {
        unsafe { libc::unlinkat(dest_dir_fd, name_c.as_ptr(), 0) };
        return Err(format!("Failed writing cross-volume output file: {e}"));
    }
    Ok(true)
}

fn copy_into_pinned_dir_overwrite(
    staged: &Path,
    dest_dir_fd: libc::c_int,
    name_c: &CString,
    mode: u32,
) -> Result<(), String> {
    let tmp_name = format!(".qsr-xdev-{}", random_hex_token());
    let tmp_c = CString::new(tmp_name.as_bytes()).unwrap();
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
    let mut out_file = unsafe { File::from_owned_fd_compat(fd) };
    let mut src = File::open(staged).map_err(|e| format!("Failed opening staged file: {e}"))?;
    if let Err(e) = io::copy(&mut src, &mut out_file).and_then(|_| out_file.sync_all()) {
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
