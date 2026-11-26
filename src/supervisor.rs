use anyhow::{bail, Context, Result};
use nix::sys::signal::Signal;
use std::env;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tracing::{debug, error, info, warn};

use crate::autostart::{AutostartApp, AutostartManager};
use crate::config::{Config, RestartPolicy, ServiceConfig};
use crate::dbus::DbusManager;
use crate::env::EnvironmentManager;
use crate::logs::{
    compositor_log_stem, resolve_log_dir, sanitize_service_log_stem, spawn_output_pump,
    SharedRotatingWriter,
};
use crate::teardown::TeardownManager;
use crate::wayland::WaylandWatcher;

struct TrackedService {
    config: ServiceConfig,
    child: Option<Child>,
    pid: Option<u32>,
    restarts: Vec<Instant>,
    total_restarts: u32,
    pending_restart_at: Option<Instant>,
    failed: bool,
    log_writer: Option<SharedRotatingWriter>,
}

enum ExitOutcome {
    Status(ExitStatus),
    CollectedBySubreaper,
}

pub struct Supervisor {
    config: Config,
    tracked_pids: Vec<u32>,
    log_dir: PathBuf,
}

impl Supervisor {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            tracked_pids: Vec::new(),
            log_dir: resolve_log_dir(),
        }
    }

    /// Run the full session lifecycle
    pub async fn run(&mut self, dry_run: bool) -> Result<i32> {
        if dry_run {
            self.perform_dry_run()?;
            return Ok(0);
        }

        info!("Session log directory: {}", self.log_dir.display());

        // 1. Ensure XDG_RUNTIME_DIR and base environment
        let runtime_dir = EnvironmentManager::ensure_xdg_runtime_dir()
            .context("Failed to initialize XDG_RUNTIME_DIR")?;
        EnvironmentManager::setup_desktop_environment(&self.config.session.env)?;

        // 2. Ensure D-Bus session bus is running
        DbusManager::ensure_session_bus()?;

        // 3. Prepare services and open per-service rotating log files
        let mut services: Vec<TrackedService> = self
            .config
            .services
            .iter()
            .filter(|s| s.enabled)
            .cloned()
            .map(|cfg| {
                let log_writer = Self::open_log_writer(
                    &self.log_dir,
                    &sanitize_service_log_stem(&cfg.name),
                    &format!("service '{}'", cfg.name),
                );
                TrackedService {
                    config: cfg,
                    child: None,
                    pid: None,
                    restarts: Vec::new(),
                    total_restarts: 0,
                    pending_restart_at: None,
                    failed: false,
                    log_writer,
                }
            })
            .collect();

        let mut log_pumps: Vec<JoinHandle<()>> = Vec::new();

        // 4. Launch pre-Wayland services (e.g. pipewire, wireplumber, conchy-config)
        for s in services.iter_mut() {
            if !s.config.wait_for_wayland {
                Self::spawn_service(s, &mut self.tracked_pids, &mut log_pumps);
            }
        }

        // 5. Launch the compositor with rotating stdout/stderr logging
        let compositor_stem = compositor_log_stem(&self.config.compositor.command);
        let compositor_log = Self::open_log_writer(&self.log_dir, &compositor_stem, "compositor");

        info!(
            "Starting compositor: {} {:?}",
            self.config.compositor.command, self.config.compositor.args
        );
        let mut compositor_cmd = Command::new(&self.config.compositor.command);
        compositor_cmd.args(&self.config.compositor.args);
        for (k, v) in &self.config.compositor.env {
            compositor_cmd.env(k, v);
        }
        if compositor_log.is_some() {
            compositor_cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        }

        let mut compositor_child = compositor_cmd.spawn().with_context(|| {
            if let Some(writer) = &compositor_log {
                let _ = writer.write_marker(&format!(
                    "Failed to spawn compositor '{}'",
                    self.config.compositor.command
                ));
            }
            format!(
                "Failed to spawn compositor: {}",
                self.config.compositor.command
            )
        })?;

        if let Some(writer) = &compositor_log {
            if let Some(stdout) = compositor_child.stdout.take() {
                log_pumps.push(spawn_output_pump(stdout, writer.clone()));
            }
            if let Some(stderr) = compositor_child.stderr.take() {
                log_pumps.push(spawn_output_pump(stderr, writer.clone()));
            }
        }

        let compositor_pid = compositor_child
            .id()
            .context("Failed to get compositor PID")?;
        self.tracked_pids.push(compositor_pid);
        info!("Compositor running with PID {}", compositor_pid);
        if let Some(writer) = &compositor_log {
            let _ = writer.write_marker(&format!(
                "Started compositor '{}' (args: {:?}, pid: {})",
                self.config.compositor.command, self.config.compositor.args, compositor_pid
            ));
        }

        // 6. Wait for Wayland socket to become ready
        let socket_name = match WaylandWatcher::wait_for_display(
            &runtime_dir,
            None,
            self.config.session.socket_wait_timeout_secs,
        )
        .await
        {
            Ok(name) => name,
            Err(e) => {
                error!(
                    "Wayland display failed to initialize: {}. Aborting session.",
                    e
                );
                if let Some(writer) = &compositor_log {
                    let _ = writer.write_marker(&format!(
                        "Wayland display failed to initialize ({}); aborting session",
                        e
                    ));
                }
                let _ = compositor_child.kill().await;
                TeardownManager::shutdown_processes(&self.tracked_pids, Duration::from_secs(2))
                    .await?;
                Self::drain_log_pumps(log_pumps).await;
                bail!("Compositor initialization timed out");
            }
        };

        info!("Wayland compositor is ready on {}", socket_name);

        // 7. Synchronize environment with D-Bus activation
        let activation_vars = EnvironmentManager::get_core_activation_vars();
        if let Err(e) = DbusManager::update_activation_environment(&activation_vars).await {
            warn!("Failed to synchronize D-Bus activation environment: {}", e);
        }

        // 8. Launch post-Wayland services (e.g. conchy-shell, notification daemon)
        for s in services.iter_mut() {
            if s.config.wait_for_wayland {
                Self::spawn_service(s, &mut self.tracked_pids, &mut log_pumps);
            }
        }

        // 9. Launch XDG autostart apps if enabled
        if self.config.session.autostart {
            let autostart_apps = AutostartManager::collect_autostart_apps();
            for app in autostart_apps {
                Self::spawn_autostart_app(app, &mut self.tracked_pids);
            }
        }

        // 10. Main supervision loop
        let mut sigterm = signal(SignalKind::terminate()).context("Failed to hook SIGTERM")?;
        let mut sigint = signal(SignalKind::interrupt()).context("Failed to hook SIGINT")?;
        let mut sighup = signal(SignalKind::hangup()).context("Failed to hook SIGHUP")?;

        let mut exit_status: Option<ExitStatus> = None;

        info!("Session fully active. Entering supervisor monitoring loop.");

        loop {
            let poll_delay = Self::next_poll_delay(&services);
            tokio::select! {
                status = compositor_child.wait() => {
                    match status {
                        Ok(st) => {
                            let reason = describe_exit_status(st);
                            if st.success() {
                                info!("Compositor exited: {}", reason);
                            } else {
                                warn!("Compositor exited abnormally: {}", reason);
                            }
                            if let Some(writer) = &compositor_log {
                                let _ = writer.write_marker(&format!(
                                    "Compositor '{}' {}",
                                    self.config.compositor.command, reason
                                ));
                            }
                            exit_status = Some(st);
                        }
                        Err(e) => {
                            error!("Error waiting on compositor: {}", e);
                            if let Some(writer) = &compositor_log {
                                let _ = writer.write_marker(&format!(
                                    "Error waiting on compositor '{}': {}",
                                    self.config.compositor.command, e
                                ));
                            }
                        }
                    }
                    break;
                }
                _ = sigterm.recv() => {
                    info!("Received SIGTERM: initiating graceful session logout...");
                    break;
                }
                _ = sigint.recv() => {
                    info!("Received SIGINT: initiating graceful session logout...");
                    break;
                }
                _ = sighup.recv() => {
                    info!("Received SIGHUP: initiating graceful session logout...");
                    break;
                }
                _ = sleep(poll_delay) => {
                    Self::check_services(&mut services, &mut self.tracked_pids, &mut log_pumps);
                }
            }
        }

        // 11. Graceful teardown
        info!("Stopping session and all supervised processes...");
        TeardownManager::shutdown_processes(&self.tracked_pids, Duration::from_secs(3)).await?;
        Self::drain_log_pumps(log_pumps).await;

        let code = exit_status.and_then(|s| s.code()).unwrap_or(0);
        info!("conchy-session exiting cleanly with code {}", code);
        Ok(code)
    }

    fn open_log_writer(log_dir: &Path, stem: &str, label: &str) -> Option<SharedRotatingWriter> {
        let log_path = log_dir.join(format!("{stem}.log"));
        match SharedRotatingWriter::open_default(log_path.clone()) {
            Ok(writer) => Some(writer),
            Err(e) => {
                warn!(
                    "Failed to open log file {} for {}: {}. Output will not be saved.",
                    log_path.display(),
                    label,
                    e
                );
                None
            }
        }
    }

    async fn drain_log_pumps(pumps: Vec<JoinHandle<()>>) {
        let _ = timeout(Duration::from_millis(250), async {
            for handle in pumps {
                let _ = handle.await;
            }
        })
        .await;
    }

    fn next_poll_delay(services: &[TrackedService]) -> Duration {
        let now = Instant::now();
        let mut delay = Duration::from_millis(250);
        for s in services {
            if let Some(deadline) = s.pending_restart_at {
                let until = deadline.saturating_duration_since(now);
                if until < delay {
                    delay = until.max(Duration::from_millis(10));
                }
            }
        }
        delay
    }

    /// Spawns a supervised service and pipes stdout/stderr to its rotating log file
    fn spawn_service(
        service: &mut TrackedService,
        tracked_pids: &mut Vec<u32>,
        log_pumps: &mut Vec<JoinHandle<()>>,
    ) {
        if service.failed {
            return;
        }

        // Verify executable exists in PATH or is absolute path
        let cmd_path = &service.config.command;
        if !Self::check_binary_exists(cmd_path) {
            let msg = format!(
                "Service '{}': binary '{}' not found in PATH. Skipping.",
                service.config.name, cmd_path
            );
            warn!("{}", msg);
            if let Some(writer) = &service.log_writer {
                let _ = writer.write_marker(&msg);
            }
            return;
        }

        let mut cmd = Command::new(cmd_path);
        cmd.args(&service.config.args);
        for (k, v) in &service.config.env {
            cmd.env(k, v);
        }
        if service.log_writer.is_some() {
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        }

        match cmd.spawn() {
            Ok(mut child) => {
                if let Some(writer) = &service.log_writer {
                    if let Some(stdout) = child.stdout.take() {
                        log_pumps.push(spawn_output_pump(stdout, writer.clone()));
                    }
                    if let Some(stderr) = child.stderr.take() {
                        log_pumps.push(spawn_output_pump(stderr, writer.clone()));
                    }
                }

                if let Some(pid) = child.id() {
                    if service.total_restarts == 0 {
                        info!(
                            "Started service '{}' ({}) with PID {}",
                            service.config.name, service.config.command, pid
                        );
                    } else {
                        info!(
                            "Restarted service '{}' ({}) with PID {} (total restarts: {})",
                            service.config.name,
                            service.config.command,
                            pid,
                            service.total_restarts
                        );
                    }
                    if let Some(writer) = &service.log_writer {
                        let phase = if service.total_restarts == 0 {
                            format!(
                                "Started service '{}' (command: '{}', args: {:?}, pid: {})",
                                service.config.name,
                                service.config.command,
                                service.config.args,
                                pid
                            )
                        } else {
                            format!(
                                "Restarted service '{}' (command: '{}', args: {:?}, pid: {}, restart #{})",
                                service.config.name,
                                service.config.command,
                                service.config.args,
                                pid,
                                service.total_restarts
                            )
                        };
                        let _ = writer.write_marker(&phase);
                    }
                    tracked_pids.push(pid);
                    service.pid = Some(pid);
                }
                service.child = Some(child);
            }
            Err(e) => {
                let msg = format!(
                    "Failed to spawn service '{}' ({}): {}",
                    service.config.name, service.config.command, e
                );
                warn!("{}", msg);
                if let Some(writer) = &service.log_writer {
                    let _ = writer.write_marker(&msg);
                }
            }
        }
    }

    /// Spawns a standalone autostart app
    fn spawn_autostart_app(app: AutostartApp, tracked_pids: &mut Vec<u32>) {
        let mut cmd = Command::new(&app.command);
        cmd.args(&app.args);

        match cmd.spawn() {
            Ok(child) => {
                if let Some(pid) = child.id() {
                    debug!("Spawned autostart app '{}' with PID {}", app.name, pid);
                    tracked_pids.push(pid);
                }
            }
            Err(e) => {
                warn!(
                    "Failed to spawn autostart app '{}' ({}): {}",
                    app.name, app.command, e
                );
            }
        }
    }

    /// Checks status of running services, logs restart reasons, and triggers scheduled restarts
    fn check_services(
        services: &mut [TrackedService],
        tracked_pids: &mut Vec<u32>,
        log_pumps: &mut Vec<JoinHandle<()>>,
    ) {
        log_pumps.retain(|h| !h.is_finished());
        let now = Instant::now();

        for s in services.iter_mut() {
            if s.failed {
                continue;
            }

            if let Some(deadline) = s.pending_restart_at {
                if now >= deadline {
                    s.pending_restart_at = None;
                    Self::spawn_service(s, tracked_pids, log_pumps);
                }
                continue;
            }

            if let Some(mut child) = s.child.take() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        Self::remove_tracked_pid(tracked_pids, s.pid.take());
                        Self::handle_service_exit(s, ExitOutcome::Status(status), now);
                    }
                    Ok(None) => {
                        // Still running
                        s.child = Some(child);
                    }
                    Err(e) if e.raw_os_error() == Some(libc::ECHILD) => {
                        Self::remove_tracked_pid(tracked_pids, s.pid.take());
                        Self::handle_service_exit(s, ExitOutcome::CollectedBySubreaper, now);
                    }
                    Err(e) => {
                        warn!(
                            "Error checking status for service '{}': {}",
                            s.config.name, e
                        );
                    }
                }
            }
        }
    }

    fn remove_tracked_pid(tracked_pids: &mut Vec<u32>, exited_pid: Option<u32>) {
        if let Some(pid) = exited_pid {
            tracked_pids.retain(|&p| p != pid);
        }
    }

    fn handle_service_exit(service: &mut TrackedService, outcome: ExitOutcome, now: Instant) {
        let (reason, is_failure) = match outcome {
            ExitOutcome::Status(status) => (describe_exit_status(status), !status.success()),
            ExitOutcome::CollectedBySubreaper => (
                "exited and was collected by subreaper (exact exit status unavailable)".to_string(),
                true,
            ),
        };

        let (should_restart, policy_reason) = match service.config.restart {
            RestartPolicy::Always => (true, "restart policy is 'always'"),
            RestartPolicy::OnFailure if is_failure => (
                true,
                "restart policy is 'on-failure' and exit was non-zero/signaled",
            ),
            RestartPolicy::OnFailure => (
                false,
                "restart policy is 'on-failure' and service exited cleanly",
            ),
            RestartPolicy::Never => (false, "restart policy is 'never'"),
        };

        if !should_restart {
            let msg = format!(
                "Service '{}' {}; not restarting ({}; total restarts: {})",
                service.config.name, reason, policy_reason, service.total_restarts
            );
            if is_failure {
                warn!("{}", msg);
            } else {
                info!("{}", msg);
            }
            if let Some(writer) = &service.log_writer {
                let _ = writer.write_marker(&msg);
            }
            return;
        }

        let window = Duration::from_secs(service.config.restart_window_secs);
        service.restarts.retain(|&t| now.duration_since(t) < window);

        if service.restarts.len() < service.config.max_restarts as usize {
            service.restarts.push(now);
            service.total_restarts += 1;
            service.pending_restart_at =
                Some(now + Duration::from_millis(service.config.restart_delay_ms));

            let msg = format!(
                "Service '{}' {}; scheduling restart #{} ({}/{} in {}s window) in {}ms ({})",
                service.config.name,
                reason,
                service.total_restarts,
                service.restarts.len(),
                service.config.max_restarts,
                service.config.restart_window_secs,
                service.config.restart_delay_ms,
                policy_reason
            );
            if is_failure {
                warn!("{}", msg);
            } else {
                info!("{}", msg);
            }
            if let Some(writer) = &service.log_writer {
                let _ = writer.write_marker(&msg);
            }
        } else {
            service.failed = true;
            service.pending_restart_at = None;
            let msg = format!(
                "Crash-loop protection triggered for service '{}': {}; exceeded {} restarts within {}s window (total restarts: {}). Disabling service.",
                service.config.name,
                reason,
                service.config.max_restarts,
                service.config.restart_window_secs,
                service.total_restarts
            );
            error!("{}", msg);
            if let Some(writer) = &service.log_writer {
                let _ = writer.write_marker(&msg);
            }
        }
    }

    /// Validates configuration and environment without launching any processes
    fn perform_dry_run(&self) -> Result<()> {
        info!("Performing dry-run configuration & environment validation...");

        let runtime_dir = EnvironmentManager::ensure_xdg_runtime_dir()?;
        info!("  [OK] XDG_RUNTIME_DIR resolved: {}", runtime_dir.display());
        info!("  [OK] Session logs directory: {}", self.log_dir.display());

        info!("  [OK] Compositor configuration:");
        info!("       Command: {}", self.config.compositor.command);
        info!("       Args:    {:?}", self.config.compositor.args);
        info!(
            "       Binary exists in PATH: {}",
            Self::check_binary_exists(&self.config.compositor.command)
        );

        info!(
            "  [OK] Supervised Services ({}):",
            self.config.services.len()
        );
        for s in &self.config.services {
            let exists = Self::check_binary_exists(&s.command);
            info!(
                "       - {:<12} cmd={:<16} wait_wayland={:<5} restart={:?} (installed: {})",
                s.name, s.command, s.wait_for_wayland, s.restart, exists
            );
        }

        if self.config.session.autostart {
            let apps = AutostartManager::collect_autostart_apps();
            info!(
                "  [OK] Discovered {} XDG autostart applications:",
                apps.len()
            );
            for app in apps {
                info!("       - {:<20} ({})", app.name, app.command);
            }
        } else {
            info!("  [OK] XDG autostart is disabled in configuration");
        }

        info!("Dry run completed successfully. Configuration is valid.");
        Ok(())
    }

    fn check_binary_exists(cmd: &str) -> bool {
        let p = Path::new(cmd);
        if p.is_absolute() {
            return p.is_file();
        }

        if let Ok(path_var) = env::var("PATH") {
            for dir in env::split_paths(&path_var) {
                if dir.join(cmd).is_file() {
                    return true;
                }
            }
        }
        false
    }
}

/// Formats a Unix `ExitStatus` with exit code or signal name and core-dump flag.
pub fn describe_exit_status(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        if code == 0 {
            "exited cleanly (code 0)".to_string()
        } else {
            format!("exited with non-zero code {code}")
        }
    } else if let Some(sig) = status.signal() {
        let sig_name = Signal::try_from(sig)
            .map(|s| s.as_str())
            .unwrap_or("UNKNOWN");
        if status.core_dumped() {
            format!("killed by signal {sig} ({sig_name}) (core dumped)")
        } else {
            format!("killed by signal {sig} ({sig_name})")
        }
    } else {
        format!("exited with status {status}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_service(
        restart: RestartPolicy,
        max_restarts: u32,
        log_path: PathBuf,
    ) -> TrackedService {
        TrackedService {
            config: ServiceConfig {
                name: "conchy-shell".to_string(),
                command: "conchy-shell".to_string(),
                args: vec!["daemon".to_string()],
                restart,
                restart_delay_ms: 100,
                max_restarts,
                restart_window_secs: 60,
                wait_for_wayland: true,
                env: Default::default(),
                enabled: true,
            },
            child: None,
            pid: None,
            restarts: Vec::new(),
            total_restarts: 0,
            pending_restart_at: None,
            failed: false,
            log_writer: Some(SharedRotatingWriter::open_with_limits(log_path, 4096, 2).unwrap()),
        }
    }

    #[test]
    fn test_describe_exit_status_codes_and_signals() {
        let ok = ExitStatus::from_raw(0);
        assert_eq!(describe_exit_status(ok), "exited cleanly (code 0)");

        let fail = ExitStatus::from_raw(42 << 8);
        assert_eq!(describe_exit_status(fail), "exited with non-zero code 42");

        let segv = ExitStatus::from_raw(libc::SIGSEGV);
        assert_eq!(
            describe_exit_status(segv),
            format!("killed by signal {} (SIGSEGV)", libc::SIGSEGV)
        );
    }

    #[test]
    fn test_crash_loop_protection_logs_reason_and_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("conchy-shell.log");
        let mut svc = make_service(RestartPolicy::Always, 2, log_path.clone());
        let now = Instant::now();

        // Exit 1 -> restart #1
        Supervisor::handle_service_exit(
            &mut svc,
            ExitOutcome::Status(ExitStatus::from_raw(1 << 8)),
            now,
        );
        assert!(!svc.failed);
        assert_eq!(svc.total_restarts, 1);
        assert!(svc.pending_restart_at.is_some());

        // Exit 2 (SIGSEGV) -> restart #2
        Supervisor::handle_service_exit(
            &mut svc,
            ExitOutcome::Status(ExitStatus::from_raw(libc::SIGSEGV)),
            now + Duration::from_millis(200),
        );
        assert!(!svc.failed);
        assert_eq!(svc.total_restarts, 2);
        assert!(svc.pending_restart_at.is_some());

        // Exit 3 -> crash-loop protection triggers
        Supervisor::handle_service_exit(
            &mut svc,
            ExitOutcome::Status(ExitStatus::from_raw(libc::SIGABRT)),
            now + Duration::from_millis(400),
        );
        assert!(svc.failed);
        assert_eq!(svc.total_restarts, 2);
        assert!(svc.pending_restart_at.is_none());

        let log_contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(log_contents.contains("exited with non-zero code 1"));
        assert!(log_contents.contains("scheduling restart #1 (1/2 in 60s window)"));
        assert!(log_contents.contains("SIGSEGV"));
        assert!(log_contents.contains("scheduling restart #2 (2/2 in 60s window)"));
        assert!(log_contents.contains("Crash-loop protection triggered"));
        assert!(log_contents.contains("SIGABRT"));
        assert!(log_contents.contains("total restarts: 2"));
    }

    #[test]
    fn test_on_failure_does_not_restart_clean_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("conchy-shell.log");
        let mut svc = make_service(RestartPolicy::OnFailure, 3, log_path.clone());

        Supervisor::handle_service_exit(
            &mut svc,
            ExitOutcome::Status(ExitStatus::from_raw(0)),
            Instant::now(),
        );
        assert!(!svc.failed);
        assert_eq!(svc.total_restarts, 0);
        assert!(svc.pending_restart_at.is_none());

        let log_contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(log_contents.contains("exited cleanly (code 0)"));
        assert!(log_contents.contains("not restarting"));
    }
}
