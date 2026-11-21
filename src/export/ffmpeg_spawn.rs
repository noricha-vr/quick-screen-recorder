use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

use crate::export::{SharedExportProgress, set_progress_permille};
use crate::fs_safety::{has_only_safe_acls, sanitize_terminal_text};

const DARWIN_POSIX_SPAWN_CLOEXEC_DEFAULT: libc::c_short = 0x4000;

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

/// Spawns `bin` with `responsibility_spawnattrs_setdisclaim` so the child process does NOT inherit
/// our app's Screen Recording / Microphone TCC permissions, with a clean environment, `/` cwd, and hard timeout.
pub(crate) fn spawn_disclaimed_with_timeout(
    bin: &Path,
    args: &[String],
    timeout: Duration,
    clip_duration_sec: f64,
    progress: Option<SharedExportProgress>,
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

    let mut out_fds = [0_i32; 2];
    if unsafe { libc::pipe(out_fds.as_mut_ptr()) } != 0 {
        return Err("Failed creating stdout progress pipe for ffmpeg".to_string());
    }
    let (out_read_fd, out_write_fd) = (out_fds[0], out_fds[1]);
    let mut pipe_fds = [0_i32; 2];
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        unsafe {
            libc::close(out_read_fd);
            libc::close(out_write_fd);
        }
        return Err("Failed creating stderr pipe for ffmpeg".to_string());
    }
    let (read_fd, write_fd) = (pipe_fds[0], pipe_fds[1]);
    if let Err(e) = set_fd_cloexec_and_nonblock(out_read_fd, true)
        .and_then(|()| set_fd_cloexec_and_nonblock(out_write_fd, false))
        .and_then(|()| set_fd_cloexec_and_nonblock(read_fd, true))
        .and_then(|()| set_fd_cloexec_and_nonblock(write_fd, false))
    {
        unsafe {
            libc::close(out_read_fd);
            libc::close(out_write_fd);
            libc::close(read_fd);
            libc::close(write_fd);
        }
        return Err(e);
    }

    let mut file_actions: libc::posix_spawn_file_actions_t = ptr::null_mut();
    let mut attrs: libc::posix_spawnattr_t = ptr::null_mut();
    let root_c = CString::new("/").unwrap();
    let devnull_c = CString::new("/dev/null").unwrap();

    let (fa_inited, attr_inited, setup_ok) = unsafe {
        let fa_ok = libc::posix_spawn_file_actions_init(&mut file_actions) == 0;
        let attr_ok = libc::posix_spawnattr_init(&mut attrs) == 0;
        let flags =
            (libc::POSIX_SPAWN_SETPGROUP as libc::c_short) | DARWIN_POSIX_SPAWN_CLOEXEC_DEFAULT;

        let ok = fa_ok
            && attr_ok
            && posix_spawn_file_actions_addchdir_np(&mut file_actions, root_c.as_ptr()) == 0
            && libc::posix_spawn_file_actions_addopen(
                &mut file_actions,
                libc::STDIN_FILENO,
                devnull_c.as_ptr(),
                libc::O_RDONLY,
                0,
            ) == 0
            && libc::posix_spawn_file_actions_adddup2(
                &mut file_actions,
                out_write_fd,
                libc::STDOUT_FILENO,
            ) == 0
            && libc::posix_spawn_file_actions_adddup2(
                &mut file_actions,
                write_fd,
                libc::STDERR_FILENO,
            ) == 0
            && libc::posix_spawnattr_setflags(&mut attrs, flags) == 0
            && libc::posix_spawnattr_setpgroup(&mut attrs, 0) == 0
            && responsibility_spawnattrs_setdisclaim(&mut attrs, 1) == 0;
        (fa_ok, attr_ok, ok)
    };

    if !setup_ok {
        unsafe {
            if fa_inited {
                libc::posix_spawn_file_actions_destroy(&mut file_actions);
            }
            if attr_inited {
                libc::posix_spawnattr_destroy(&mut attrs);
            }
            libc::close(out_read_fd);
            libc::close(out_write_fd);
            libc::close(read_fd);
            libc::close(write_fd);
        }
        return Err("Failed configuring sandboxed posix_spawn attributes for ffmpeg".to_string());
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
        libc::close(out_write_fd);
        libc::close(write_fd);
    }

    if spawn_rc != 0 {
        unsafe {
            libc::close(out_read_fd);
            libc::close(read_fd);
        }
        return Err(format!("posix_spawn failed for ffmpeg: errno {spawn_rc}"));
    }

    let dur = clip_duration_sec.max(0.05);
    let mut out_buf = Vec::with_capacity(4096);
    let mut stderr_bytes = Vec::with_capacity(1024);

    let start = Instant::now();
    let mut status: libc::c_int = 0;
    let wait_err = loop {
        drain_nonblocking_fd(out_read_fd, &mut out_buf, 65_536);
        parse_ffmpeg_progress_lines(&mut out_buf, dur, progress.as_ref());
        drain_nonblocking_fd(read_fd, &mut stderr_bytes, 8_192);

        let w = unsafe { libc::waitpid(child_pid, &mut status, libc::WNOHANG) };
        if w == child_pid {
            break None;
        }
        if w < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break Some("waitpid error waiting for ffmpeg".to_string());
        }
        if start.elapsed() > timeout {
            break Some("Timed out running ffmpeg export".to_string());
        }
        thread::sleep(Duration::from_millis(15));
    };

    unsafe {
        if child_pid > 1 {
            libc::kill(-child_pid, libc::SIGKILL);
        }
        if wait_err.is_some() {
            libc::kill(child_pid, libc::SIGKILL);
            let _ = libc::waitpid(child_pid, &mut status, 0);
        }
    }
    drain_nonblocking_fd(out_read_fd, &mut out_buf, 65_536);
    parse_ffmpeg_progress_lines(&mut out_buf, dur, progress.as_ref());
    drain_nonblocking_fd(read_fd, &mut stderr_bytes, 8_192);
    unsafe {
        libc::close(out_read_fd);
        libc::close(read_fd);
    }

    if let Some(err_msg) = wait_err {
        return Err(err_msg);
    }
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

/// Validates both the candidate entry/ancestors BEFORE symlink resolution and the canonical target/ancestors AFTER.
/// Only `wheel` (0) and macOS `admin` (80) are accepted for group-writable directories (never shared `staff`=20).
pub(crate) fn verify_trusted_binary(candidate: &Path, euid: u32) -> Option<PathBuf> {
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
    if !has_only_safe_acls(&canon, euid) {
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
        if !has_only_safe_acls(dir, euid) {
            return None;
        }
        cur = dir.parent();
    }
    Some(())
}

const fn is_trusted_admin_group(gid: u32) -> bool {
    gid == 0 || gid == 80
}

fn set_fd_cloexec_and_nonblock(fd: libc::c_int, nonblock: bool) -> Result<(), String> {
    unsafe {
        let fd_flags = libc::fcntl(fd, libc::F_GETFD);
        if fd_flags < 0 || libc::fcntl(fd, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC) < 0 {
            return Err("Failed setting FD_CLOEXEC on ffmpeg pipe".to_string());
        }
        if nonblock {
            let fl = libc::fcntl(fd, libc::F_GETFL);
            if fl < 0 || libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) < 0 {
                return Err("Failed setting O_NONBLOCK on ffmpeg pipe".to_string());
            }
        }
    }
    Ok(())
}

/// Reads at most 32 chunks (64 KiB) per poll tick so continuous pipe output can never starve timeout checks.
fn drain_nonblocking_fd(fd: libc::c_int, dst: &mut Vec<u8>, max_keep: usize) {
    let mut chunk = [0_u8; 2048];
    for _ in 0..32 {
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        let slice = &chunk[..(n as usize)];
        let room = max_keep.saturating_sub(dst.len());
        if room > 0 {
            dst.extend_from_slice(&slice[..slice.len().min(room)]);
        }
    }
}

fn parse_ffmpeg_progress_lines(
    buf: &mut Vec<u8>,
    dur: f64,
    progress: Option<&SharedExportProgress>,
) {
    while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
        let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
        if let Ok(line) = std::str::from_utf8(&line_bytes) {
            if let Some(us_raw) = line.trim().strip_prefix("out_time_us=") {
                if let Ok(us) = us_raw.parse::<i64>() {
                    if us >= 0 {
                        let frac = ((us as f64 / 1_000_000.0) / dur).clamp(0.0, 1.0);
                        set_progress_permille(progress, 50 + (frac * 910.0) as u32);
                    }
                }
            }
        }
    }
    if buf.len() > 32_768 {
        buf.clear();
    }
}
