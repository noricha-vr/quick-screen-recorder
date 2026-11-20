use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use crate::config::APP_SUPPORT_DIR_NAME;
use crate::observe::now_unix_secs;

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

/// Returns `HOME` only if it is non-empty and an absolute path.
pub fn valid_home_dir() -> Option<PathBuf> {
    let raw = std::env::var_os("HOME")?;
    if raw.is_empty() {
        return None;
    }
    let p = PathBuf::from(raw);
    if p.is_absolute() { Some(p) } else { None }
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

/// An exclusively created (`mkdir` `0700`) staging directory on the same filesystem as the destination.
/// Stores the canonicalized destination path at creation time to prevent parent-symlink TOCTOU swaps.
pub struct ExclusiveStagingDir {
    dir_path: PathBuf,
    staged_file: PathBuf,
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
        fs::create_dir_all(&parent)
            .map_err(|e| format!("Failed creating output directory: {e}"))?;
        let canon_parent = fs::canonicalize(&parent)
            .map_err(|e| format!("Failed resolving canonical output directory: {e}"))?;
        let canon_dest = canon_parent.join(file_name);

        let euid = unsafe { libc::geteuid() };
        if let Ok(existing) = fs::symlink_metadata(&canon_dest) {
            if existing.file_type().is_symlink() || !existing.is_file() || existing.uid() != euid {
                return Err(
                    "Refusing to overwrite symlink, special file, or file owned by another user"
                        .to_string(),
                );
            }
        }

        let dir_path = canon_parent.join(format!(".qsr-work-{}", random_hex_token()));
        DirBuilder::new()
            .mode(0o700)
            .create(&dir_path)
            .map_err(|e| format!("Failed creating exclusive 0700 staging directory: {e}"))?;

        let dir_meta = fs::symlink_metadata(&dir_path)
            .map_err(|e| format!("Failed verifying staging directory metadata: {e}"))?;
        if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() || dir_meta.uid() != euid {
            let _ = fs::remove_dir_all(&dir_path);
            return Err("Staging directory ownership verification failed".to_string());
        }

        let staged_file = dir_path.join(format!("staged.{ext}"));
        Ok(Self {
            dir_path,
            staged_file,
            canon_dest,
        })
    }

    pub fn staged_file(&self) -> &Path {
        &self.staged_file
    }

    pub fn commit(&self) -> Result<u64, String> {
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

        // Only inherit permissions from a regular file owned by the current user; otherwise enforce 0600.
        let desired_mode = match fs::symlink_metadata(&self.canon_dest) {
            Ok(existing) => {
                if existing.file_type().is_symlink()
                    || !existing.is_file()
                    || existing.uid() != euid
                {
                    return Err(
                        "Refusing to overwrite symlink, non-regular file, or file owned by another user"
                            .to_string(),
                    );
                }
                existing.permissions().mode() & 0o777
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0o600,
            Err(e) => return Err(format!("Failed inspecting destination path: {e}")),
        };

        fs::set_permissions(&self.staged_file, fs::Permissions::from_mode(desired_mode))
            .map_err(|e| format!("Failed setting output file permissions: {e}"))?;

        fs::rename(&self.staged_file, &self.canon_dest)
            .map_err(|e| format!("Failed moving staged file to destination: {e}"))?;
        Ok(staged_meta.len())
    }
}

impl Drop for ExclusiveStagingDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir_path);
    }
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
    if !dir.is_absolute() {
        return Err("Private directory path must be absolute".to_string());
    }
    if let Some(parent) = dir.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed creating parent directory: {e}"))?;
        }
        if let Ok(pm) = fs::symlink_metadata(parent) {
            if pm.file_type().is_symlink() {
                return Err("Refusing to use symlinked parent directory".to_string());
            }
        }
    }

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
    let euid = unsafe { libc::geteuid() };
    if verified.file_type().is_symlink() || !verified.is_dir() || verified.uid() != euid {
        return Err("Private directory must be owned by the current user".to_string());
    }

    let dir_fd = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)
        .map_err(|e| format!("Failed opening private directory without following symlinks: {e}"))?;
    dir_fd
        .set_permissions(fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("Failed setting 0700 permissions on private directory: {e}"))?;
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
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&tmp)?;
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
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let expired = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > max_age);
        if expired {
            let _ = fs::remove_file(entry.path());
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
}
