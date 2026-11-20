use std::ffi::CString;
use std::fs;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

use crate::export::{ExportFormat, ExportRequest};
use crate::fs_safety::sanitize_terminal_text;

const ALLOWLISTED_FFMPEG_PATHS: [&str; 3] = [
    "/opt/homebrew/bin/ffmpeg",
    "/usr/local/bin/ffmpeg",
    "/usr/bin/ffmpeg",
];

const FFMPEG_TIMEOUT: Duration = Duration::from_secs(180);

unsafe extern "C" {
    fn responsibility_spawnattrs_setdisclaim(
        attrs: *mut libc::posix_spawnattr_t,
        disclaim: libc::c_int,
    ) -> libc::c_int;
    fn posix_spawn_file_actions_addchdir_np(
        file_actions: *mut libc::posix_spawn_file_actions_t,
        path: *const libc::c_char,
    ) -> libc::c_int;
}

pub fn detect_ffmpeg_binary() -> Option<PathBuf> {
    let euid = unsafe { libc::geteuid() };
    for candidate in ALLOWLISTED_FFMPEG_PATHS {
        if let Some(verified) = verify_trusted_binary(Path::new(candidate), euid) {
            return Some(verified);
        }
    }
    None
}

pub fn export_with_ffmpeg(req: &ExportRequest) -> Result<(), String> {
    let ffmpeg = detect_ffmpeg_binary().ok_or_else(|| {
        format!(
            "{} export requires a trusted 'ffmpeg' binary at /opt/homebrew/bin/ffmpeg or /usr/local/bin/ffmpeg. MP4, HEVC, GIF, and APNG work natively without ffmpeg.",
            req.format.label()
        )
    })?;

    let local_in = verified_local_file_arg(&req.input, true)?;
    let local_out = verified_local_file_arg(&req.output, false)?;

    match req.format {
        ExportFormat::Webm => run_ffmpeg_encode(
            &ffmpeg,
            &local_in,
            &local_out,
            req,
            &[
                "-c:v",
                "libvpx-vp9",
                "-crf",
                "30",
                "-b:v",
                "0",
                "-row-mt",
                "1",
                "-cpu-used",
                "4",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "libopus",
            ],
            "webm",
        ),
        ExportFormat::Av1 => {
            let svt = run_ffmpeg_encode(
                &ffmpeg,
                &local_in,
                &local_out,
                req,
                &[
                    "-c:v",
                    "libsvtav1",
                    "-crf",
                    "32",
                    "-preset",
                    "8",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                ],
                "mp4",
            );
            if svt.is_ok() {
                return Ok(());
            }
            run_ffmpeg_encode(
                &ffmpeg,
                &local_in,
                &local_out,
                req,
                &[
                    "-c:v",
                    "libaom-av1",
                    "-crf",
                    "34",
                    "-cpu-used",
                    "6",
                    "-row-mt",
                    "1",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                ],
                "mp4",
            )
        }
        other => Err(format!(
            "Internal error: {} should use native exporter",
            other.label()
        )),
    }
}

fn run_ffmpeg_encode(
    ffmpeg_bin: &Path,
    input_file_url: &str,
    output_file_url: &str,
    req: &ExportRequest,
    codec_args: &[&str],
    container: &str,
) -> Result<(), String> {
    let mut args: Vec<String> = vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-protocol_whitelist".into(),
        "file".into(),
    ];

    if let Some(start) = req.start_sec {
        if start > 0.005 {
            args.push("-ss".into());
            args.push(format!("{start:.3}"));
        }
    }
    if let Some(end) = req.end_sec {
        args.push("-to".into());
        args.push(format!("{end:.3}"));
    }

    // Restrict demuxer to MP4/MOV family and disable external MOV data references (`dref`)
    args.extend([
        "-f".into(),
        "mov,mp4,m4a,3gp,3g2,mj2".into(),
        "-enable_drefs".into(),
        "0".into(),
        "-use_absolute_path".into(),
        "0".into(),
        "-i".into(),
        input_file_url.into(),
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "0:a:0?".into(),
        "-sn".into(),
        "-dn".into(),
        "-map_metadata".into(),
        "-1".into(),
        "-map_chapters".into(),
        "-1".into(),
        "-r".into(),
        req.fps.clamp(1, 120).to_string(),
    ]);

    if let Some(max_w) = req.max_width {
        let safe_w = (max_w.max(32)) & !1;
        args.push("-vf".into());
        args.push(format!("scale='min({safe_w},iw)':-2:flags=lanczos"));
    } else {
        args.push("-vf".into());
        args.push("scale=trunc(iw/2)*2:trunc(ih/2)*2".into());
    }

    for &c in codec_args {
        args.push(c.into());
    }
    args.extend([
        "-protocol_whitelist".into(),
        "file".into(),
        "-f".into(),
        container.into(),
        output_file_url.into(),
    ]);

    spawn_disclaimed_with_timeout(ffmpeg_bin, &args, FFMPEG_TIMEOUT)
}

/// Spawns `bin` with `responsibility_spawnattrs_setdisclaim` so the child process does NOT inherit
/// our app's Screen Recording / Microphone TCC permissions, with a clean environment, `/` cwd, and hard timeout.
fn spawn_disclaimed_with_timeout(
    bin: &Path,
    args: &[String],
    timeout: Duration,
) -> Result<(), String> {
    let prog_c = CString::new(bin.as_os_str().as_bytes())
        .map_err(|_| "Executable path contains NUL byte")?;
    let mut argv_c: Vec<CString> = Vec::with_capacity(args.len() + 1);
    argv_c.push(prog_c.clone());
    for a in args {
        argv_c.push(CString::new(a.as_bytes()).map_err(|_| "Argument contains NUL byte")?);
    }
    let mut argv_ptrs: Vec<*mut libc::c_char> =
        argv_c.iter().map(|s| s.as_ptr().cast_mut()).collect();
    argv_ptrs.push(ptr::null_mut());

    let env_c = [
        CString::new("PATH=/usr/bin:/bin").unwrap(),
        CString::new("LANG=C").unwrap(),
    ];
    let mut env_ptrs: Vec<*mut libc::c_char> =
        env_c.iter().map(|s| s.as_ptr().cast_mut()).collect();
    env_ptrs.push(ptr::null_mut());

    let mut pipe_fds = [0_i32; 2];
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        return Err("Failed creating stderr pipe for ffmpeg".to_string());
    }
    let (read_fd, write_fd) = (pipe_fds[0], pipe_fds[1]);

    let mut file_actions: libc::posix_spawn_file_actions_t = ptr::null_mut();
    let mut attrs: libc::posix_spawnattr_t = ptr::null_mut();
    let root_c = CString::new("/").unwrap();
    let devnull_c = CString::new("/dev/null").unwrap();

    unsafe {
        libc::posix_spawn_file_actions_init(&mut file_actions);
        posix_spawn_file_actions_addchdir_np(&mut file_actions, root_c.as_ptr());
        libc::posix_spawn_file_actions_addopen(
            &mut file_actions,
            libc::STDIN_FILENO,
            devnull_c.as_ptr(),
            libc::O_RDONLY,
            0,
        );
        libc::posix_spawn_file_actions_addopen(
            &mut file_actions,
            libc::STDOUT_FILENO,
            devnull_c.as_ptr(),
            libc::O_WRONLY,
            0,
        );
        libc::posix_spawn_file_actions_adddup2(&mut file_actions, write_fd, libc::STDERR_FILENO);
        libc::posix_spawn_file_actions_addclose(&mut file_actions, read_fd);
        libc::posix_spawn_file_actions_addclose(&mut file_actions, write_fd);

        libc::posix_spawnattr_init(&mut attrs);
        let _ = responsibility_spawnattrs_setdisclaim(&mut attrs, 1);
    }

    let mut child_pid: libc::pid_t = 0;
    let spawn_rc = unsafe {
        libc::posix_spawn(
            &mut child_pid,
            prog_c.as_ptr(),
            &file_actions,
            &attrs,
            argv_ptrs.as_ptr(),
            env_ptrs.as_ptr(),
        )
    };

    unsafe {
        libc::posix_spawn_file_actions_destroy(&mut file_actions);
        libc::posix_spawnattr_destroy(&mut attrs);
        libc::close(write_fd);
    }

    if spawn_rc != 0 {
        unsafe { libc::close(read_fd) };
        return Err(format!("posix_spawn failed for ffmpeg: errno {spawn_rc}"));
    }

    let stderr_handle = thread::spawn(move || {
        let mut reader = unsafe { fs::File::from_raw_fd(read_fd) };
        let mut buf = Vec::new();
        let _ = (&mut reader).take(8192).read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let mut status: libc::c_int = 0;
    loop {
        let w = unsafe { libc::waitpid(child_pid, &mut status, libc::WNOHANG) };
        if w == child_pid {
            break;
        }
        if w < 0 {
            return Err("waitpid error waiting for ffmpeg".to_string());
        }
        if start.elapsed() > timeout {
            unsafe {
                libc::kill(child_pid, libc::SIGKILL);
                libc::waitpid(child_pid, &mut status, 0);
            }
            return Err("Timed out running ffmpeg export".to_string());
        }
        thread::sleep(Duration::from_millis(15));
    }

    let stderr_bytes = stderr_handle.join().unwrap_or_default();
    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
        Ok(())
    } else {
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            -1
        };
        let brief: String = String::from_utf8_lossy(&stderr_bytes)
            .lines()
            .take(4)
            .map(|l| sanitize_terminal_text(l, false))
            .collect::<Vec<_>>()
            .join(" | ");
        Err(format!("ffmpeg exited with code {code}: {brief}"))
    }
}

pub fn verified_local_file_arg(path: &Path, must_exist: bool) -> Result<String, String> {
    let resolved = if must_exist {
        fs::canonicalize(path).map_err(|e| format!("Invalid local input file: {e}"))?
    } else {
        let file_name = path
            .file_name()
            .ok_or("Output path is missing a file name")?;
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => std::env::current_dir().map_err(|e| format!("cwd error: {e}"))?,
        };
        let canon_parent =
            fs::canonicalize(&parent).map_err(|e| format!("Invalid output directory: {e}"))?;
        canon_parent.join(file_name)
    };

    if !resolved.is_absolute() {
        return Err("Resolved path must be an absolute local path".to_string());
    }
    let s = resolved.to_str().ok_or("Path is not valid UTF-8")?;
    Ok(format!("file:{s}"))
}

/// Validates both the candidate entry/ancestors BEFORE symlink resolution and the canonical target/ancestors AFTER.
/// Only `wheel` (0) and macOS `admin` (80) are accepted for group-writable directories (never shared `staff`=20).
fn verify_trusted_binary(candidate: &Path, euid: u32) -> Option<PathBuf> {
    let lstat = fs::symlink_metadata(candidate).ok()?;
    if lstat.uid() != 0 && lstat.uid() != euid {
        return None;
    }
    verify_ancestor_chain(candidate.parent(), euid)?;

    let canon = fs::canonicalize(candidate).ok()?;
    let meta = fs::metadata(&canon).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mode = meta.permissions().mode();
    if (mode & 0o111 == 0) || (mode & 0o002 != 0) {
        return None;
    }
    if (mode & 0o020 != 0) && !is_trusted_admin_group(meta.gid()) {
        return None;
    }
    if meta.uid() != 0 && meta.uid() != euid {
        return None;
    }

    verify_ancestor_chain(canon.parent(), euid)?;
    Some(canon)
}

fn verify_ancestor_chain(mut cur: Option<&Path>, euid: u32) -> Option<()> {
    while let Some(dir) = cur {
        if dir.as_os_str().is_empty() {
            break;
        }
        let dm = fs::metadata(dir).ok()?;
        let dmode = dm.permissions().mode();
        if (dmode & 0o002 != 0) && (dmode & 0o1000 == 0) {
            return None;
        }
        if (dmode & 0o020 != 0) && !is_trusted_admin_group(dm.gid()) {
            return None;
        }
        if dm.uid() != 0 && dm.uid() != euid {
            return None;
        }
        cur = dir.parent();
    }
    Some(())
}

const fn is_trusted_admin_group(gid: u32) -> bool {
    gid == 0 || gid == 80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_local_file_arg_always_uses_file_scheme_on_canonical_path() {
        assert!(
            verified_local_file_arg(Path::new("http://127.0.0.1:9999/evil.webm"), false).is_err()
        );
        let valid = std::env::temp_dir().join("safe-out.webm");
        let arg = verified_local_file_arg(&valid, false).expect("valid local target");
        assert!(arg.starts_with("file:/"));
    }
}
