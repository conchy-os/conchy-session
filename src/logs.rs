use anyhow::{bail, Context, Result};
use clap::Args;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing_subscriber::fmt::MakeWriter;

/// Default maximum size of a single log file before rotation (2 MiB).
pub const DEFAULT_MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// Default number of rotated backup log files to retain (`.1`, `.2`, `.3`).
pub const DEFAULT_MAX_ROTATED_FILES: usize = 3;

/// Arguments for `conchy-session logs`.
#[derive(Args, Debug, Clone)]
pub struct LogsArgs {
    /// Service name (e.g. conchy-shell, conchy-wm, pipewire) or "session".
    /// Omit or pass --list to list available log files.
    #[arg(value_name = "SERVICE")]
    pub service: Option<String>,

    /// Follow log output as new lines are written.
    #[arg(short, long)]
    pub follow: bool,

    /// Show only the last N lines before exiting (or before following).
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
pub fn resolve_log_dir() -> PathBuf {
    if let Some(state_home) = env::var_os("XDG_STATE_HOME") {
        if !state_home.is_empty() {
            return PathBuf::from(state_home).join("conchy").join("logs");
        }
    }

    if let Some(home) = env::var_os("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("conchy")
                .join("logs");
        }
    }

    let uid = nix::unistd::getuid().as_raw();
    PathBuf::from(format!("/tmp/conchy-state-{uid}/logs"))
}

/// Converts an arbitrary service name into a safe log file stem without overwriting `session.log`.
pub fn sanitize_service_log_stem(name: &str) -> String {
    let mut sanitized = sanitize_raw_stem(name);
    if sanitized == "session" {
        sanitized = "service-session".to_string();
    }
    sanitized
}

/// Derives a safe log file stem for the compositor command.
pub fn compositor_log_stem(command: &str) -> String {
    let base = Path::new(command)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("compositor");
    let sanitized = sanitize_raw_stem(base);
    if sanitized == "session" || sanitized == "service" {
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

/// Formats the current UTC wall-clock timestamp in ISO-8601 format (`YYYY-MM-DDTHH:MM:SSZ`).
pub fn format_timestamp_now() -> String {
    format_unix_timestamp(SystemTime::now())
}

fn format_unix_timestamp(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Converts days since 1970-01-01 to `(year, month, day)` in the proleptic Gregorian calendar.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + i64::from(m <= 2);
    (y, m, d)
}

fn rotated_path(base: &Path, index: usize) -> PathBuf {
    PathBuf::from(format!("{}.{index}", base.display()))
}

struct RotatingWriterInner {
    path: PathBuf,
    file: Option<File>,
    current_size: u64,
    max_bytes: u64,
    max_files: usize,
    last_byte_was_newline: bool,
}

impl RotatingWriterInner {
    fn open(path: PathBuf, max_bytes: u64, max_files: usize) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let current_size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let last_byte_was_newline = if current_size == 0 {
            true
        } else {
            file_ends_with_newline(&path).unwrap_or(false)
        };

        Ok(Self {
            path,
            file: Some(file),
            current_size,
            max_bytes,
            max_files,
            last_byte_was_newline,
        })
    }

    fn write_all_bytes(&mut self, buf: &[u8]) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }

        self.rotate_if_needed(buf.len() as u64)?;

        if self.file.is_none() {
            self.file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?,
            );
        }

        let file = self
            .file
            .as_mut()
            .expect("log file must be open before writing");
        file.write_all(buf)?;
        file.flush()?;
        self.current_size = self.current_size.saturating_add(buf.len() as u64);
        self.last_byte_was_newline = buf.last() == Some(&b'\n');
        Ok(())
    }

    fn write_marker(&mut self, message: &str) -> io::Result<()> {
        let prefix = if !self.last_byte_was_newline && self.current_size > 0 {
            "\n"
        } else {
            ""
        };
        let ts = format_timestamp_now();
        let line = format!("{prefix}[{ts}] [conchy-session] {message}\n");
        self.write_all_bytes(line.as_bytes())
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
        }

        let fresh = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        self.file = Some(fresh);
        self.current_size = 0;
        self.last_byte_was_newline = true;
        Ok(())
    }
}

fn file_ends_with_newline(path: &Path) -> io::Result<bool> {
    let mut reader = File::open(path)?;
    let len = reader.metadata()?.len();
    if len == 0 {
        return Ok(true);
    }
    reader.seek(SeekFrom::End(-1))?;
    let mut byte = [0u8; 1];
    reader.read_exact(&mut byte)?;
    Ok(byte[0] == b'\n')
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

    /// Writes raw child output bytes, rotating when needed.
    pub fn write_all_bytes(&self, buf: &[u8]) -> io::Result<()> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.write_all_bytes(buf)
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

/// Spawns an asynchronous task that pumps a child stdout or stderr pipe into a rotating log.
pub fn spawn_output_pump<R>(mut reader: R, writer: SharedRotatingWriter) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if writer.write_all_bytes(&buf[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    })
}

/// Executes the `conchy-session logs` CLI subcommand.
pub async fn run_logs_command(args: &LogsArgs) -> Result<()> {
    let log_dir = resolve_log_dir();

    if args.list || args.service.is_none() {
        return print_available_logs(&log_dir);
    }

    let service = args
        .service
        .as_deref()
        .expect("service presence checked above");
    let log_path = resolve_target_log_path(&log_dir, service);

    let files = collect_log_files(&log_path, args.all);
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

    if let Err(err) = print_initial_logs(&files, args.lines) {
        if err.kind() == io::ErrorKind::BrokenPipe {
            return Ok(());
        }
        return Err(err).with_context(|| format!("Failed to read logs for '{service}'"));
    }

    if args.follow {
        if let Err(err) = follow_log_file(&log_path).await {
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
    if service == "session" {
        return log_dir.join("session.log");
    }

    let direct = log_dir.join(format!("{}.log", sanitize_service_log_stem(service)));
    if direct.exists() {
        return direct;
    }

    // Also allow exact filename stem match (e.g. `service-session` or `compositor`)
    let raw = log_dir.join(format!("{}.log", sanitize_raw_stem(service)));
    if raw.exists() {
        return raw;
    }

    direct
}

struct LogEntryInfo {
    name: String,
    size_bytes: u64,
    rotated_count: usize,
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
        let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let rotated_count = count_rotated_files(&path);
        entries.push(LogEntryInfo {
            name: stem.to_string(),
            size_bytes,
            rotated_count,
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
        writeln!(out, "No logs found in {}", log_dir.display())?;
        return Ok(());
    }

    writeln!(out, "Available logs in {}:", log_dir.display())?;
    for entry in entries {
        if entry.rotated_count > 0 {
            writeln!(
                out,
                "  {:<20} {} (+{} rotated)",
                entry.name,
                format_size(entry.size_bytes),
                entry.rotated_count
            )?;
        } else {
            writeln!(
                out,
                "  {:<20} {}",
                entry.name,
                format_size(entry.size_bytes)
            )?;
        }
    }
    Ok(())
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

fn print_initial_logs(files: &[PathBuf], tail_lines: Option<usize>) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    match tail_lines {
        None => {
            for path in files {
                let mut f = File::open(path)?;
                io::copy(&mut f, &mut out)?;
            }
        }
        Some(0) => {}
        Some(limit) => {
            let mut ring: std::collections::VecDeque<Vec<u8>> =
                std::collections::VecDeque::with_capacity(limit);
            for path in files {
                let f = File::open(path)?;
                let mut reader = BufReader::new(f);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    let read = reader.read_until(b'\n', &mut line)?;
                    if read == 0 {
                        break;
                    }
                    if ring.len() == limit {
                        ring.pop_front();
                    }
                    ring.push_back(line.clone());
                }
            }
            for line in ring {
                out.write_all(&line)?;
            }
        }
    }

    out.flush()
}

async fn follow_log_file(log_path: &Path) -> io::Result<()> {
    let mut file = File::open(log_path)?;
    let meta = file.metadata()?;
    let mut current_id = (meta.dev(), meta.ino());
    let mut offset = meta.len();
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
    fn test_format_unix_timestamp_known_dates() {
        assert_eq!(format_unix_timestamp(UNIX_EPOCH), "1970-01-01T00:00:00Z");

        // 2024-02-29T23:59:59Z = 1709251199
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_251_199);
        assert_eq!(format_unix_timestamp(leap), "2024-02-29T23:59:59Z");
    }

    #[test]
    fn test_rotating_writer_rotates_by_size() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("conchy-shell.log");
        let writer = SharedRotatingWriter::open_with_limits(log_path.clone(), 40, 2).unwrap();

        writer
            .write_all_bytes(b"line-1-01234567890123456789\n")
            .unwrap();
        writer
            .write_all_bytes(b"line-2-01234567890123456789\n")
            .unwrap();
        writer
            .write_all_bytes(b"line-3-01234567890123456789\n")
            .unwrap();
        writer
            .write_all_bytes(b"line-4-01234567890123456789\n")
            .unwrap();

        assert!(log_path.is_file());
        assert!(rotated_path(&log_path, 1).is_file());
        assert!(rotated_path(&log_path, 2).is_file());
        assert!(!rotated_path(&log_path, 3).exists());

        let current = fs::read_to_string(&log_path).unwrap();
        let rot1 = fs::read_to_string(rotated_path(&log_path, 1)).unwrap();
        let rot2 = fs::read_to_string(rotated_path(&log_path, 2)).unwrap();

        assert!(current.contains("line-4-"));
        assert!(rot1.contains("line-3-"));
        assert!(rot2.contains("line-2-"));
    }

    #[test]
    fn test_write_marker_starts_on_new_line() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("pipewire.log");
        let writer = SharedRotatingWriter::open_with_limits(log_path.clone(), 4096, 2).unwrap();

        writer
            .write_all_bytes(b"partial line without newline")
            .unwrap();
        writer
            .write_marker("service 'pipewire' exited: killed by signal 11 (SIGSEGV)")
            .unwrap();

        let text = fs::read_to_string(&log_path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "partial line without newline");
        assert!(lines[1].contains("[conchy-session] service 'pipewire' exited"));
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
}
