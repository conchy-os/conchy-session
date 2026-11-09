use anyhow::Result;
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::process::Command;
use tracing::{debug, info, warn};

pub struct DbusManager;

impl DbusManager {
    /// Ensures that a valid D-Bus session bus is available and sets DBUS_SESSION_BUS_ADDRESS.
    /// In Void Linux (runit), a session bus might not be automatically started upon login.
    pub fn ensure_session_bus() -> Result<()> {
        if env::var("DBUS_SESSION_BUS_ADDRESS").is_ok() {
            debug!("DBUS_SESSION_BUS_ADDRESS already set");
            return Ok(());
        }

        // Check if standard $XDG_RUNTIME_DIR/bus socket exists
        if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
            let bus_path = PathBuf::from(&runtime_dir).join("bus");
            if bus_path.exists() {
                let addr = format!("unix:path={}", bus_path.display());
                info!("Found existing D-Bus socket at {}", bus_path.display());
                env::set_var("DBUS_SESSION_BUS_ADDRESS", &addr);
                return Ok(());
            }
        }

        // Try to spawn dbus-daemon --session --fork --print-address
        info!("DBUS_SESSION_BUS_ADDRESS not found, launching dbus-daemon --session...");
        let output = Command::new("dbus-daemon")
            .args(["--session", "--fork", "--print-address"])
            .output();

        match output {
            Ok(out) if out.status.success() => {
                let addr = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !addr.is_empty() {
                    info!("Started session dbus-daemon: {}", addr);
                    env::set_var("DBUS_SESSION_BUS_ADDRESS", &addr);
                    return Ok(());
                }
            }
            Ok(out) => {
                warn!(
                    "dbus-daemon exited with status {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            Err(e) => {
                warn!("Could not execute dbus-daemon (is dbus installed?): {}", e);
            }
        }

        // If launching dbus-daemon directly didn't work, don't fail hard; session can still run
        Ok(())
    }

    /// Broadcasts session environment variables to D-Bus activation environment.
    /// This ensures that applications launched via D-Bus activation (such as xdg-desktop-portal,
    /// notification daemons, and polkit agents) inherit WAYLAND_DISPLAY and other desktop variables.
    pub async fn update_activation_environment(vars: &[(String, String)]) -> Result<()> {
        if vars.is_empty() {
            return Ok(());
        }

        let conn = match zbus::Connection::session().await {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "Cannot connect to D-Bus session bus to update activation environment: {}",
                    e
                );
                return Ok(());
            }
        };

        let mut map: HashMap<&str, &str> = HashMap::new();
        for (k, v) in vars {
            map.insert(k.as_str(), v.as_str());
        }

        debug!(
            "Calling org.freedesktop.DBus.UpdateActivationEnvironment with {} variables",
            map.len()
        );

        let reply = conn
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "UpdateActivationEnvironment",
                &(map,),
            )
            .await;

        match reply {
            Ok(_) => {
                info!("Successfully updated D-Bus activation environment");
            }
            Err(e) => {
                warn!("Failed to update D-Bus activation environment: {}", e);
            }
        }

        Ok(())
    }
}
