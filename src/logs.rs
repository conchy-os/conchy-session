//! Per-service and session log management (Void Linux has no journald).
//!
//! Layout of `$XDG_STATE_HOME/conchy/logs/` (default `~/.local/state/conchy/logs/`):
//! - `session.log` — `conchy-session` orchestrator journal (service starts, exit reasons,
//!   restart counts, crash-loop protection, and session lifecycle);
//! - `<service>.log` — line-timestamped `stdout`/`stderr` of each supervised service and the
//!   compositor (ANSI sequences stripped), plus `[conchy-session]` lifecycle markers;
//! - `autostart.log` — output of XDG autostart apps, tagged with `[<app>]`.

use anyhow::{bail, Context, Result};
use clap::Args;
use std::collections::VecDeque;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader as TokioBufReader};
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::MakeWriter;

/// Default maximum size of a single log file before rotation (2 MiB).
pub const DEFAULT_MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// Default number of rotated backup log files to retain (`.1`, `.2`, `.3`).
pub const DEFAULT_MAX_ROTATED_FILES: usize = 3;

/// Maximum line length buffered from child pipes so missing newlines cannot exhaust memory.
const MAX_LINE_BYTES: u64 = 16 * 1024;

/// Reserved log stem for `conchy-session`'s own journal.
pub const SESSION_LOG_STEM: &str = "session";

/// Reserved log stem for XDG autostart application output.
pub const AUTOSTART_LOG_STEM: &str = "autostart";

/// Arguments for `conchy-session logs`.
#[derive(Args, Debug, Clone)]
pub struct LogsArgs {
    /// Service name (e.g. conchy-shell, conchy-wm, pipewire), "session", or "autostart".
    /// Omit or pass --list to list available log files.
    #[arg(value_name = "SERVICE")]
    pub service: Option<String>,

    /// Follow log output as new lines are written.
    #[arg(short, long)]
    pub follow: bool,

    /// Show only the last N lines before exiting (0 shows all including rotated archives).
    #[arg(short = 'n', long, value_name = "LINES")]
    pub lines: Option<usize>,

    /// Include rotated archive files (.log.3, .log.2, .log.1) before the current log.
    #[arg(short, long)]
    pub all: bool,

    /// List available log files in the session log directory and exit.
    #[arg(long)]
    pub list: bool,
}

/// Resolves the directory where session and service logs are stored:
/// `$XDG_STATE_HOME/conchy/logs`, falling back to `~/.local/state/conchy/logs`.
/// Per the XDG Base Directory specification, relative paths are ignored.
pub fn resolve_log_dir() -> PathBuf {
    if let Some(state_home) = env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
        if state_home.is_absolute() {
            return state_home.join("conchy").join("logs");
        }
    }

    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        if home.is_absolute() {
            return home
                .join(".local")
                .join("state")
                .join("conchy")
                .join("logs");
        }
    }

    let uid = nix::unistd::getuid().as_raw();
    PathBuf::from(format!("/tmp/conchy-state-{uid}/logs"))
}

/// Ensures the log directory exists with restrictive `0700` permissions.
pub fn ensure_log_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Converts an arbitrary service name into a safe log file stem without overwriting
/// `session.log` or `autostart.log`.
pub fn sanitize_service_log_stem(name: &str) -> String {
    let mut sanitized = sanitize_raw_stem(name);
    if sanitized == SESSION_LOG_STEM || sanitized == AUTOSTART_LOG_STEM {
        sanitized = format!("service-{sanitized}");
    }
    sanitized
}

/// Derives a safe log file stem for the compositor command.
pub fn compositor_log_stem(command: &str) -> String {
    let base = Path::new(command)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("compositor");
    let sanitized = sanitize_raw_stem(base);
    if sanitized == SESSION_LOG_STEM || sanitized == AUTOSTART_LOG_STEM || sanitized == "service" {
        "compositor".to_string()
    } else {
        sanitized
    }
}

fn sanitize_raw_stem(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();

    while out.starts_with('.') {
        out.remove(0);
    }

    if out.is_empty() {
        "service".to_string()
    } else {
        out
    }
}

/// Formats the current local wall-clock timestamp (`YYYY-MM-DD HH:MM:SS.mmm`),
/// matching both `session.log` and per-service logs.
pub fn format_timestamp_now() -> String {
    format_local_timestamp(SystemTime::now())
}

fn format_local_timestamp(time: SystemTime) -> String {
    let dur = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as libc::time_t;
    let millis = dur.subsec_millis();
    // SAFETY: `libc::tm` is plain old data and `localtime_r` initializes all fields.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if !ok {
        return format!("@{}.{:03}", dur.as_secs(), millis);
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        millis
    )
}

/// Local-time formatter for `tracing_subscriber` so `session.log` timestamps match service logs.
pub struct LocalTime;

impl FormatTime for LocalTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", format_timestamp_now())
    }
}

/// Strips ANSI CSI (`ESC [ ...`) and two-byte escape sequences from child output so logs stay
/// readable in any viewer.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&c) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

fn rotated_path(base: &Path, index: usize) -> PathBuf {
    PathBuf::from(format!("{}.{index}", base.display()))
}

fn open_append_0600(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

struct RotatingWriterInner {
    path: PathBuf,
    file: Option<File>,
    current_size: u64,
    max_bytes: u64,
    max_files: usize,
}

impl RotatingWriterInner {
    fn open(path: PathBuf, max_bytes: u64, max_files: usize) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            ensure_log_dir(parent)?;
        }

        let file = open_append_0600(&path)?;
        let current_size = file.metadata().map(|m| m.len()).unwrap_or(0);

        Ok(Self {
            path,
            file: Some(file),
            current_size,
            max_bytes,
            max_files,
        })
    }

    fn write_all_bytes(&mut self, buf: &[u8]) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }

        self.rotate_if_needed(buf.len() as u64)?;

        if self.file.is_none() {
            self.file = Some(open_append_0600(&self.path)?);
        }

        let file = self
            .file
            .as_mut()
            .expect("log file must be open before writing");
        file.write_all(buf)?;
        file.flush()?;
        self.current_size = self.current_size.saturating_add(buf.len() as u64);
        Ok(())
    }

    fn write_line(&mut self, line: &str) -> io::Result<()> {
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        self.write_all_bytes(&buf)
    }

    fn write_marker(&mut self, message: &str) -> io::Result<()> {
        let ts = format_timestamp_now();
        self.write_line(&format!("{ts} [conchy-session] {message}"))
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.flush()?;
        }
        Ok(())
    }

    fn rotate_if_needed(&mut self, incoming_len: u64) -> io::Result<()> {
        if self.max_bytes == 0
            || self.current_size == 0
            || self.current_size.saturating_add(incoming_len) <= self.max_bytes
        {
            return Ok(());
        }

        if let Some(mut file) = self.file.take() {
            let _ = file.flush();
        }

        if self.max_files > 0 {
            let oldest = rotated_path(&self.path, self.max_files);
            match fs::remove_file(&oldest) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }

            for idx in (1..self.max_files).rev() {
                let src = rotated_path(&self.path, idx);
                if src.exists() {
                    let dst = rotated_path(&self.path, idx + 1);
                    fs::rename(&src, &dst)?;
                }
            }

            if self.path.exists() {
                fs::rename(&self.path, rotated_path(&self.path, 1))?;
            }
        } else {
            let _ = fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&self.path)?;
        }

        self.file = Some(open_append_0600(&self.path)?);
        self.current_size = 0;
        Ok(())
    }
}

/// Thread-safe size-rotating log writer shared across stdout/stderr pumps, lifecycle markers,
/// and `tracing_subscriber`.
#[derive(Clone)]
pub struct SharedRotatingWriter {
    inner: Arc<Mutex<RotatingWriterInner>>,
}

impl SharedRotatingWriter {
    /// Opens or creates a rotating log file with default size and archive limits.
    pub fn open_default(path: PathBuf) -> io::Result<Self> {
        Self::open_with_limits(path, DEFAULT_MAX_LOG_BYTES, DEFAULT_MAX_ROTATED_FILES)
    }

    /// Opens or creates a rotating log file with explicit size and archive limits.
    pub fn open_with_limits(path: PathBuf, max_bytes: u64, max_files: usize) -> io::Result<Self> {
        let inner = RotatingWriterInner::open(path, max_bytes, max_files)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    /// Writes raw already-formatted bytes (used by `tracing_subscriber`), rotating when needed.
    pub fn write_all_bytes(&self, buf: &[u8]) -> io::Result<()> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.write_all_bytes(buf)
    }

    /// Appends a single complete line, rotating cleanly on line boundaries.
    pub fn write_line(&self, line: &str) -> io::Result<()> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.write_line(line)
    }

    /// Appends a timestamped `[conchy-session]` lifecycle marker line to the service log.
    pub fn write_marker(&self, message: &str) -> io::Result<()> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.write_marker(message)
    }
}

/// Write handle returned by `MakeWriter` for `tracing_subscriber`.
pub struct RotatingWriteHandle(SharedRotatingWriter);

impl Write for RotatingWriteHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_all_bytes(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut guard = self
            .0
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.flush()
    }
}

impl<'a> MakeWriter<'a> for SharedRotatingWriter {
    type Writer = RotatingWriteHandle;

    fn make_writer(&'a self) -> Self::Writer {
        RotatingWriteHandle(self.clone())
    }
}

/// Spawns an asynchronous task that reads a child's stdout or stderr line by line,
/// strips ANSI escape sequences, prepends a local timestamp (and optional `[tag]`),
/// and writes to the rotating log file.
pub fn spawn_output_pump<R>(
    reader: R,
    writer: SharedRotatingWriter,
    tag: Option<String>,
) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = TokioBufReader::new(reader);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match (&mut reader)
                .take(MAX_LINE_BYTES)
                .read_until(b'\n', &mut buf)
                .await
            {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    while matches!(buf.last(), Some(b'\n' | b'\r')) {
                        buf.pop();
                    }
                    let clean = strip_ansi(&String::from_utf8_lossy(&buf));
                    let ts = format_timestamp_now();
                    let formatted = match &tag {
                        Some(tag) => format!("{ts} [{tag}] {clean}"),
                        None => format!("{ts} {clean}"),
                    };
                    if writer.write_line(&formatted).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

/// Executes the `conchy-session logs` CLI subcommand.
pub async fn run_logs_command(args: &LogsArgs) -> Result<()> {
    let log_dir = resolve_log_dir();

    if args.list || args.service.is_none() {
        if args.follow {
            bail!("--follow requires a service name, e.g. `conchy-session logs conchy-shell -f`");
        }
        return print_available_logs(&log_dir);
    }

    let service = args
        .service
        .as_deref()
        .expect("service presence checked above");
    let log_path = resolve_target_log_path(&log_dir, service);

    // `-n 0` or `--all` pulls in rotated archive files as well.
    let include_rotated = args.all || args.lines == Some(0);
    let tail_limit = match args.lines {
        Some(0) | None => None,
        Some(n) => Some(n),
    };

    // If `-n <N>` is requested without `--all`, still reach into rotated archives when the
    // current file has fewer than N lines.
    let files = if tail_limit.is_some() && !include_rotated {
        collect_log_files(&log_path, true)
    } else {
        collect_log_files(&log_path, include_rotated)
    };

    if files.is_empty() {
        let available = discover_log_entries(&log_dir)?;
        if available.is_empty() {
            bail!(
                "Log file for '{}' not found ({}). No logs exist in {} yet.",
                service,
                log_path.display(),
                log_dir.display()
            );
        } else {
            let names: Vec<String> = available.into_iter().map(|e| e.name).collect();
            bail!(
                "Log file for '{}' not found ({}). Available logs: {}",
                service,
                log_path.display(),
                names.join(", ")
            );
        }
    }

    let initial_offset = match print_initial_logs(&files, &log_path, tail_limit) {
        Ok(offset) => offset,
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| format!("Failed to read logs for '{service}'"))
        }
    };

    if args.follow {
        if let Err(err) = follow_log_file(&log_path, initial_offset).await {
            if err.kind() == io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(err)
                .with_context(|| format!("Failed while following {}", log_path.display()));
        }
    }

    Ok(())
}

fn resolve_target_log_path(log_dir: &Path, service: &str) -> PathBuf {
    let trimmed = service.strip_suffix(".log").unwrap_or(service);
    if trimmed == SESSION_LOG_STEM {
        return log_dir.join("session.log");
    }
    if trimmed == AUTOSTART_LOG_STEM {
        return log_dir.join("autostart.log");
    }

    let direct = log_dir.join(format!("{}.log", sanitize_service_log_stem(trimmed)));
    if direct.exists() {
        return direct;
    }

    let raw = log_dir.join(format!("{}.log", sanitize_raw_stem(trimmed)));
    if raw.exists() {
        return raw;
    }

    direct
}

struct LogEntryInfo {
    name: String,
    size_bytes: u64,
    rotated_count: usize,
    modified: Option<SystemTime>,
}

fn discover_log_entries(log_dir: &Path) -> Result<Vec<LogEntryInfo>> {
    if !log_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(log_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }

        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let meta = entry.metadata().ok();
        let size_bytes = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta.and_then(|m| m.modified().ok());
        let rotated_count = count_rotated_files(&path);
        entries.push(LogEntryInfo {
            name: stem.to_string(),
            size_bytes,
            rotated_count,
            modified,
        });
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

fn count_rotated_files(base: &Path) -> usize {
    let mut count = 0;
    for idx in 1..=32 {
        if rotated_path(base, idx).is_file() {
            count += 1;
        } else {
            break;
        }
    }
    count
}

fn print_available_logs(log_dir: &Path) -> Result<()> {
    let entries = discover_log_entries(log_dir)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();

    if entries.is_empty() {
        return quiet_pipe(writeln!(out, "No logs found in {}", log_dir.display()));
    }

    quiet_pipe(writeln!(out, "Available logs in {}:", log_dir.display()))?;
    for entry in entries {
        let age = entry
            .modified
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .map(format_age)
            .unwrap_or_default();
        if entry.rotated_count > 0 {
            quiet_pipe(writeln!(
                out,
                "  {:<22} {:>9}  {} (+{} rotated)",
                entry.name,
                format_size(entry.size_bytes),
                age,
                entry.rotated_count
            ))?;
        } else {
            quiet_pipe(writeln!(
                out,
                "  {:<22} {:>9}  {}",
                entry.name,
                format_size(entry.size_bytes),
                age
            ))?;
        }
    }
    Ok(())
}

fn quiet_pipe(res: io::Result<()>) -> Result<()> {
    match res {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => Ok(other?),
    }
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn format_age(age: Duration) -> String {
    let secs = age.as_secs();
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3_599 => format!("{}m ago", secs / 60),
        3_600..=86_399 => format!("{}h ago", secs / 3_600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

fn collect_log_files(base: &Path, include_rotated: bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if include_rotated {
        let mut rotated = Vec::new();
        for idx in 1..=32 {
            let p = rotated_path(base, idx);
            if p.is_file() {
                rotated.push(p);
            } else {
                break;
            }
        }
        rotated.reverse();
        files.extend(rotated);
    }

    if base.is_file() {
        files.push(base.to_path_buf());
    }
    files
}

fn print_initial_logs(
    files: &[PathBuf],
    primary_path: &Path,
    tail_lines: Option<usize>,
) -> io::Result<u64> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut primary_offset = 0;

    match tail_lines {
        None => {
            for path in files {
                let mut f = File::open(path)?;
                let copied = io::copy(&mut f, &mut out)?;
                if path == primary_path {
                    primary_offset = copied;
                }
            }
        }
        Some(limit) => {
            let mut ring: VecDeque<Vec<u8>> = VecDeque::with_capacity(limit);
            for path in files {
                let f = File::open(path)?;
                let mut file_bytes = 0u64;
                let mut reader = BufReader::new(f);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    let read = reader.read_until(b'\n', &mut line)?;
                    if read == 0 {
                        break;
                    }
                    file_bytes = file_bytes.saturating_add(read as u64);
                    if ring.len() == limit {
                        ring.pop_front();
                    }
                    ring.push_back(line.clone());
                }
                if path == primary_path {
                    primary_offset = file_bytes;
                }
            }
            for line in ring {
                out.write_all(&line)?;
            }
        }
    }

    out.flush()?;
    Ok(primary_offset)
}

async fn follow_log_file(log_path: &Path, initial_offset: u64) -> io::Result<()> {
    let mut file = File::open(log_path)?;
    let meta = file.metadata()?;
    let mut current_id = (meta.dev(), meta.ino());
    let mut offset = initial_offset.min(meta.len());
    file.seek(SeekFrom::Start(offset))?;

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut buf = [0u8; 8192];

    loop {
        loop {
            let read = file.read(&mut buf)?;
            if read == 0 {
                break;
            }
            offset = offset.saturating_add(read as u64);
            let stdout = io::stdout();
            let mut out = stdout.lock();
            out.write_all(&buf[..read])?;
            out.flush()?;
        }

        if let Ok(new_meta) = fs::metadata(log_path) {
            let new_id = (new_meta.dev(), new_meta.ino());
            if new_id != current_id {
                // Drain any remaining bytes on the old rotated file descriptor first.
                loop {
                    let read = file.read(&mut buf)?;
                    if read == 0 {
                        break;
                    }
                    let stdout = io::stdout();
                    let mut out = stdout.lock();
                    out.write_all(&buf[..read])?;
                    out.flush()?;
                }
                if let Ok(reopened) = File::open(log_path) {
                    file = reopened;
                    current_id = new_id;
                    offset = 0;
                    continue;
                }
            } else if new_meta.len() < offset {
                file.seek(SeekFrom::Start(0))?;
                offset = 0;
                continue;
            }
        }

        tokio::select! {
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
            _ = sleep(Duration::from_millis(100)) => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_service_log_stem() {
        assert_eq!(sanitize_service_log_stem("conchy-shell"), "conchy-shell");
        assert_eq!(sanitize_service_log_stem("session"), "service-session");
        assert_eq!(sanitize_service_log_stem("autostart"), "service-autostart");
        assert_eq!(sanitize_service_log_stem("../foo/bar"), "_foo_bar");
        assert_eq!(sanitize_service_log_stem("..."), "service");
    }

    #[test]
    fn test_compositor_log_stem() {
        assert_eq!(compositor_log_stem("/usr/bin/conchy-wm"), "conchy-wm");
        assert_eq!(compositor_log_stem("conchy-wm"), "conchy-wm");
        assert_eq!(compositor_log_stem("/tmp/session"), "compositor");
    }

    #[test]
    fn test_strip_ansi_sequences() {
        assert_eq!(
            strip_ansi("\x1b[2m12:00\x1b[0m \x1b[32mINFO\x1b[0m conchy_wm: ok"),
            "12:00 INFO conchy_wm: ok"
        );
        assert_eq!(strip_ansi("plain [text]"), "plain [text]");
    }

    #[test]
    fn test_rotating_writer_rotates_by_size_and_reopens() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("conchy-shell.log");
        let writer = SharedRotatingWriter::open_with_limits(log_path.clone(), 30, 2).unwrap();

        writer.write_line("line-1-0123456789").unwrap();
        writer.write_line("line-2-0123456789").unwrap();
        writer.write_line("line-3-0123456789").unwrap();

        assert!(log_path.is_file());
        assert!(rotated_path(&log_path, 1).is_file());
        assert!(rotated_path(&log_path, 2).is_file());
        assert!(!rotated_path(&log_path, 3).exists());

        // Reopen existing file and ensure existing length counts toward rotation limit
        let reopened = SharedRotatingWriter::open_with_limits(log_path.clone(), 30, 2).unwrap();
        reopened.write_line("line-4-0123456789").unwrap();

        let current = fs::read_to_string(&log_path).unwrap();
        let rot1 = fs::read_to_string(rotated_path(&log_path, 1)).unwrap();
        assert!(current.contains("line-4-"));
        assert!(rot1.contains("line-3-"));
    }

    #[test]
    fn test_collect_and_tail_across_rotated_files() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("conchy-shell.log");
        fs::write(rotated_path(&log_path, 2), "a1\na2\n").unwrap();
        fs::write(rotated_path(&log_path, 1), "b1\nb2\n").unwrap();
        fs::write(&log_path, "c1\nc2\n").unwrap();

        let only_current = collect_log_files(&log_path, false);
        assert_eq!(only_current, vec![log_path.clone()]);

        let all = collect_log_files(&log_path, true);
        assert_eq!(
            all,
            vec![
                rotated_path(&log_path, 2),
                rotated_path(&log_path, 1),
                log_path,
            ]
        );
    }

    #[tokio::test]
    async fn test_capture_writes_both_streams_with_timestamps_and_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let log_dir = tmp.path().join("logs");
        let log_path = log_dir.join("demo.log");
        let writer = SharedRotatingWriter::open_default(log_path.clone()).unwrap();

        let mut child = tokio::process::Command::new("sh")
            .args([
                "-c",
                "printf '\\033[32mout-line\\033[0m\\n'; echo err-line >&2; printf 'no-newline'",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();

        let p1 = spawn_output_pump(child.stdout.take().unwrap(), writer.clone(), None);
        let p2 = spawn_output_pump(
            child.stderr.take().unwrap(),
            writer.clone(),
            Some("tag".into()),
        );
        child.wait().await.unwrap();
        p1.await.unwrap();
        p2.await.unwrap();
        writer.write_marker("exited cleanly (code 0)").unwrap();

        let text = fs::read_to_string(&log_path).unwrap();
        assert!(text.contains(" out-line\n"), "{text}");
        assert!(!text.contains("\x1b["), "ANSI must be stripped: {text}");
        assert!(text.contains(" [tag] err-line\n"), "{text}");
        assert!(text.contains(" no-newline\n"), "{text}");
        assert!(
            text.contains("[conchy-session] exited cleanly (code 0)"),
            "{text}"
        );

        let dir_mode = fs::metadata(&log_dir).unwrap().permissions().mode() & 0o777;
        let file_mode = fs::metadata(&log_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }
}
