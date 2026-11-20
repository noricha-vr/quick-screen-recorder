use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::APP_SUPPORT_DIR_NAME;
use crate::fs_safety::{ensure_private_dir, sanitize_terminal_text, valid_home_dir};

const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 4096;

/// Local-only performance telemetry entry.
/// Never stores file paths, window titles, user names, or any personal identifiers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PerfEvent {
    pub ts_unix: u64,
    pub kind: PerfKind,
    pub format: String,
    pub fps: u32,
    pub input_duration_ms: u64,
    pub elapsed_ms: u64,
    pub output_bytes: u64,
    pub ok: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PerfKind {
    Record,
    Export,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PerfSummary {
    pub total_events: usize,
    pub success_events: usize,
    pub by_format: BTreeMap<String, FormatStats>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct FormatStats {
    pub count: usize,
    pub ok_count: usize,
    pub elapsed_ms_values: Vec<u64>,
    pub total_output_bytes: u64,
}

impl FormatStats {
    pub fn p50_ms(&self) -> u64 {
        percentile(&self.elapsed_ms_values, 50)
    }

    pub fn p95_ms(&self) -> u64 {
        percentile(&self.elapsed_ms_values, 95)
    }
}

pub fn default_perf_log_path() -> Option<PathBuf> {
    // Only allow custom override if it is an absolute `.jsonl` file inside a non-symlink directory
    if let Some(custom) = std::env::var_os("QSR_PERF_LOG") {
        let p = PathBuf::from(custom);
        if p.is_absolute() && p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            return Some(p);
        }
    }
    let home = valid_home_dir()?;
    Some(
        home.join("Library")
            .join("Logs")
            .join(APP_SUPPORT_DIR_NAME)
            .join("perf.jsonl"),
    )
}

pub fn record_perf_event(event: &PerfEvent) {
    if let Some(path) = default_perf_log_path() {
        if let Some(parent) = path.parent() {
            if ensure_private_dir(parent).is_err() {
                return;
            }
        }
        let _ = append_perf_event(&path, event);
    }
}

pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn append_perf_event(path: &Path, event: &PerfEvent) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
            let pm = fs::symlink_metadata(parent)?;
            if pm.file_type().is_symlink() || !pm.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Refusing to append telemetry in symlinked directory",
                ));
            }
        }
    }

    let mut line =
        serde_json::to_string(event).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push('\n');

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(path)?;
    let fm = file.metadata()?;
    let euid = unsafe { libc::geteuid() };
    if !fm.is_file() || fm.uid() != euid || fm.nlink() > 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Telemetry log must be a single-link regular file owned by current user",
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(line.as_bytes())
}

pub fn summarize_perf_log(path: &Path) -> io::Result<PerfSummary> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(PerfSummary::default()),
        Err(e) => return Err(e),
    };
    let fm = file.metadata()?;
    let euid = unsafe { libc::geteuid() };
    if !fm.is_file() || fm.uid() != euid || fm.nlink() > 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Refusing to read untrusted or multi-linked telemetry log",
        ));
    }

    let mut reader = BufReader::new(file.take(MAX_LOG_BYTES));
    let mut summary = PerfSummary::default();
    let mut buf = String::new();

    loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            break;
        }
        if buf.len() > MAX_LINE_BYTES {
            continue;
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<PerfEvent>(trimmed) else {
            continue;
        };
        summary.total_events += 1;
        if ev.ok {
            summary.success_events += 1;
        }
        let key = format!(
            "{}:{}",
            match ev.kind {
                PerfKind::Record => "record",
                PerfKind::Export => "export",
            },
            ev.format
        );
        let entry = summary.by_format.entry(key).or_default();
        entry.count += 1;
        if ev.ok {
            entry.ok_count += 1;
            entry.elapsed_ms_values.push(ev.elapsed_ms);
            entry.total_output_bytes += ev.output_bytes;
        }
    }

    for stats in summary.by_format.values_mut() {
        stats.elapsed_ms_values.sort_unstable();
    }
    Ok(summary)
}

pub fn format_perf_summary(summary: &PerfSummary) -> String {
    if summary.total_events == 0 {
        return "No local performance events recorded yet.".to_string();
    }
    let mut out = format!(
        "Total events: {} (succeeded: {})\n",
        summary.total_events, summary.success_events
    );
    out.push_str("Kind:Format          Count    OK   p50(ms)   p95(ms)   Total KB\n");
    for (key, st) in &summary.by_format {
        let safe_key = sanitize_terminal_text(key, false);
        out.push_str(&format!(
            "{:<20} {:>5} {:>5} {:>9} {:>9} {:>10}\n",
            safe_key,
            st.count,
            st.ok_count,
            st.p50_ms(),
            st.p95_ms(),
            st.total_output_bytes / 1024
        ));
    }
    out
}

fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) * pct) / 100;
    sorted[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perf_log_records_and_summarizes_cleanly() {
        let dir = std::env::temp_dir().join(format!(
            "qsr-perf-dir-{}",
            crate::fs_safety::random_hex_token()
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        let tmp = dir.join("perf.jsonl");

        for ms in [120_u64, 180, 240] {
            append_perf_event(
                &tmp,
                &PerfEvent {
                    ts_unix: 1_700_000_000,
                    kind: PerfKind::Export,
                    format: "gif".to_string(),
                    fps: 30,
                    input_duration_ms: 2000,
                    elapsed_ms: ms,
                    output_bytes: 65_536,
                    ok: true,
                },
            )
            .expect("append perf");
        }

        let summary = summarize_perf_log(&tmp).expect("summarize");
        assert_eq!(summary.total_events, 3);
        assert_eq!(summary.success_events, 3);
        let gif = summary.by_format.get("export:gif").expect("gif entry");
        assert_eq!(gif.p50_ms(), 180);
        assert_eq!(gif.p95_ms(), 180);

        let rendered = format_perf_summary(&summary);
        assert!(rendered.contains("export:gif"));
        let _ = fs::remove_dir_all(&dir);
    }
}
