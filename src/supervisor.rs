use anyhow::{bail, Context, Result};
use std::env;
use std::path::Path;
use std::process::ExitStatus;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::autostart::{AutostartApp, AutostartManager};
use crate::config::{Config, RestartPolicy, ServiceConfig};
use crate::dbus::DbusManager;
use crate::env::EnvironmentManager;
use crate::teardown::TeardownManager;
use crate::wayland::WaylandWatcher;

struct TrackedService {
    config: ServiceConfig,
    child: Option<Child>,
    pid: Option<u32>,
    restarts: Vec<Instant>,
    failed: bool,
}

pub struct Supervisor {
    config: Config,
    tracked_pids: Vec<u32>,
}

impl Supervisor {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            tracked_pids: Vec::new(),
        }
    }

    /// Run the full session lifecycle
    pub async fn run(&mut self, dry_run: bool) -> Result<i32> {
        if dry_run {
            self.perform_dry_run()?;
            return Ok(0);
        }

        // 1. Ensure XDG_RUNTIME_DIR and base environment
        let runtime_dir = EnvironmentManager::ensure_xdg_runtime_dir()
            .context("Failed to initialize XDG_RUNTIME_DIR")?;
        EnvironmentManager::setup_desktop_environment(&self.config.session.env)?;

        // 2. Ensure D-Bus session bus is running
        DbusManager::ensure_session_bus()?;

        // 3. Prepare services
        let mut services: Vec<TrackedService> = self
            .config
            .services
            .iter()
            .filter(|s| s.enabled)
            .cloned()
            .map(|cfg| TrackedService {
                config: cfg,
                child: None,
                pid: None,
                restarts: Vec::new(),
                failed: false,
            })
            .collect();

        // 4. Launch pre-Wayland services (e.g. pipewire, wireplumber)
        for s in services.iter_mut() {
            if !s.config.wait_for_wayland {
                Self::spawn_service(s, &mut self.tracked_pids);
            }
        }

        // 5. Launch the compositor
        info!(
            "Starting compositor: {} {:?}",
            self.config.compositor.command, self.config.compositor.args
        );
        let mut compositor_cmd = Command::new(&self.config.compositor.command);
        compositor_cmd.args(&self.config.compositor.args);
        for (k, v) in &self.config.compositor.env {
            compositor_cmd.env(k, v);
        }

        let mut compositor_child = compositor_cmd
            .spawn()
            .with_context(|| format!("Failed to spawn compositor: {}", self.config.compositor.command))?;

        let compositor_pid = compositor_child.id().context("Failed to get compositor PID")?;
        self.tracked_pids.push(compositor_pid);
        info!("Compositor running with PID {}", compositor_pid);

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
                error!("Wayland display failed to initialize: {}. Aborting session.", e);
                let _ = compositor_child.kill().await;
                TeardownManager::shutdown_processes(&self.tracked_pids, Duration::from_secs(2)).await?;
                bail!("Compositor initialization timed out");
            }
        };

        info!("Wayland compositor is ready on {}", socket_name);

        // 7. Synchronize environment with D-Bus activation
        let activation_vars = EnvironmentManager::get_core_activation_vars();
        if let Err(e) = DbusManager::update_activation_environment(&activation_vars).await {
            warn!("Failed to synchronize D-Bus activation environment: {}", e);
        }

        // 8. Launch post-Wayland services (e.g. shell, notification daemon)
        for s in services.iter_mut() {
            if s.config.wait_for_wayland {
                Self::spawn_service(s, &mut self.tracked_pids);
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
            tokio::select! {
                status = compositor_child.wait() => {
                    match status {
                        Ok(st) => {
                            info!("Compositor exited with status: {}", st);
                            exit_status = Some(st);
                        }
                        Err(e) => {
                            error!("Error waiting on compositor: {}", e);
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
                _ = sleep(Duration::from_millis(1000)) => {
                    Self::check_services(&mut services, &mut self.tracked_pids).await;
                }
            }
        }

        // 11. Graceful teardown
        info!("Stopping session and all supervised processes...");
        TeardownManager::shutdown_processes(&self.tracked_pids, Duration::from_secs(3)).await?;

        let code = exit_status.and_then(|s| s.code()).unwrap_or(0);
        info!("conchy-session exiting cleanly with code {}", code);
        Ok(code)
    }

    /// Spawns a supervised service
    fn spawn_service(service: &mut TrackedService, tracked_pids: &mut Vec<u32>) {
        if service.failed {
            return;
        }

        // Verify executable exists in PATH or is absolute path
        let cmd_path = &service.config.command;
        if !Self::check_binary_exists(cmd_path) {
            warn!(
                "Service '{}': binary '{}' not found in PATH. Skipping.",
                service.config.name, cmd_path
            );
            return;
        }

        let mut cmd = Command::new(cmd_path);
        cmd.args(&service.config.args);
        for (k, v) in &service.config.env {
            cmd.env(k, v);
        }

        match cmd.spawn() {
            Ok(child) => {
                if let Some(pid) = child.id() {
                    info!(
                        "Started service '{}' ({}) with PID {}",
                        service.config.name, service.config.command, pid
                    );
                    tracked_pids.push(pid);
                    service.pid = Some(pid);
                }
                service.child = Some(child);
            }
            Err(e) => {
                warn!(
                    "Failed to spawn service '{}' ({}): {}",
                    service.config.name, service.config.command, e
                );
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
                warn!("Failed to spawn autostart app '{}' ({}): {}", app.name, app.command, e);
            }
        }
    }

    /// Checks status of running services and restarts if necessary
    async fn check_services(services: &mut [TrackedService], tracked_pids: &mut Vec<u32>) {
        let now = Instant::now();

        for s in services.iter_mut() {
            if s.failed {
                continue;
            }

            if let Some(mut child) = s.child.take() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        warn!("Service '{}' exited with status: {}", s.config.name, status);
                        s.pid = None;

                        // Check if restart is desired
                        let should_restart = match s.config.restart {
                            RestartPolicy::Always => true,
                            RestartPolicy::OnFailure => !status.success(),
                            RestartPolicy::Never => false,
                        };

                        if should_restart {
                            // Prune restart window
                            let window = Duration::from_secs(s.config.restart_window_secs);
                            s.restarts.retain(|&t| now.duration_since(t) < window);

                            if s.restarts.len() < s.config.max_restarts as usize {
                                s.restarts.push(now);
                                info!(
                                    "Scheduling restart for service '{}' in {}ms (restart {}/{})",
                                    s.config.name,
                                    s.config.restart_delay_ms,
                                    s.restarts.len(),
                                    s.config.max_restarts
                                );
                                sleep(Duration::from_millis(s.config.restart_delay_ms)).await;
                                Self::spawn_service(s, tracked_pids);
                            } else {
                                error!(
                                    "Service '{}' exceeded max restarts ({} in {}s). Disabling.",
                                    s.config.name, s.config.max_restarts, s.config.restart_window_secs
                                );
                                s.failed = true;
                            }
                        }
                    }
                    Ok(None) => {
                        // Still running
                        s.child = Some(child);
                    }
                    Err(e) if e.raw_os_error() == Some(libc::ECHILD) => {
                        warn!("Service '{}' exited and was collected by subreaper", s.config.name);
                        s.pid = None;

                        let should_restart = match s.config.restart {
                            RestartPolicy::Always | RestartPolicy::OnFailure => true,
                            RestartPolicy::Never => false,
                        };

                        if should_restart {
                            let window = Duration::from_secs(s.config.restart_window_secs);
                            s.restarts.retain(|&t| now.duration_since(t) < window);

                            if s.restarts.len() < s.config.max_restarts as usize {
                                s.restarts.push(now);
                                info!(
                                    "Scheduling restart for service '{}' in {}ms (restart {}/{})",
                                    s.config.name,
                                    s.config.restart_delay_ms,
                                    s.restarts.len(),
                                    s.config.max_restarts
                                );
                                sleep(Duration::from_millis(s.config.restart_delay_ms)).await;
                                Self::spawn_service(s, tracked_pids);
                            } else {
                                error!(
                                    "Service '{}' exceeded max restarts ({} in {}s). Disabling.",
                                    s.config.name, s.config.max_restarts, s.config.restart_window_secs
                                );
                                s.failed = true;
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Error checking status for service '{}': {}", s.config.name, e);
                    }
                }
            }
        }
    }

    /// Validates configuration and environment without launching any processes
    fn perform_dry_run(&self) -> Result<()> {
        info!("Performing dry-run configuration & environment validation...");

        let runtime_dir = EnvironmentManager::ensure_xdg_runtime_dir()?;
        info!("  [OK] XDG_RUNTIME_DIR resolved: {}", runtime_dir.display());

        info!("  [OK] Compositor configuration:");
        info!("       Command: {}", self.config.compositor.command);
        info!("       Args:    {:?}", self.config.compositor.args);
        info!("       Binary exists in PATH: {}", Self::check_binary_exists(&self.config.compositor.command));

        info!("  [OK] Supervised Services ({}):", self.config.services.len());
        for s in &self.config.services {
            let exists = Self::check_binary_exists(&s.command);
            info!(
                "       - {:<12} cmd={:<16} wait_wayland={:<5} restart={:?} (installed: {})",
                s.name, s.command, s.wait_for_wayland, s.restart, exists
            );
        }

        if self.config.session.autostart {
            let apps = AutostartManager::collect_autostart_apps();
            info!("  [OK] Discovered {} XDG autostart applications:", apps.len());
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
