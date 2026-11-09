use anyhow::{Context, Result};
use nix::unistd::getuid;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use tracing::{debug, info, warn};

pub struct EnvironmentManager;

impl EnvironmentManager {
    /// Validates or establishes XDG_RUNTIME_DIR.
    /// On Void Linux (especially with runit or minimal PAM), XDG_RUNTIME_DIR might not be set by default.
    pub fn ensure_xdg_runtime_dir() -> Result<PathBuf> {
        let uid = getuid().as_raw();

        if let Ok(existing) = env::var("XDG_RUNTIME_DIR") {
            let path = PathBuf::from(&existing);
            if path.is_dir() {
                // Verify directory permissions are 0700
                if let Ok(metadata) = fs::metadata(&path) {
                    let mode = metadata.permissions().mode() & 0o777;
                    if mode != 0o700 {
                        warn!(
                            "XDG_RUNTIME_DIR ({}) permissions are {:o}, expected 0700. Attempting to fix.",
                            path.display(),
                            mode
                        );
                        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o700));
                    }
                }
                debug!("Using existing XDG_RUNTIME_DIR: {}", path.display());
                return Ok(path);
            }
        }

        // Check if /run/user/<uid> exists
        let run_user = PathBuf::from(format!("/run/user/{}", uid));
        if run_user.is_dir() {
            info!("Found /run/user/{}, setting as XDG_RUNTIME_DIR", uid);
            env::set_var("XDG_RUNTIME_DIR", &run_user);
            return Ok(run_user);
        }

        // Fallback: create secure directory in /tmp/conchy-runtime-<uid>
        let fallback = PathBuf::from(format!("/tmp/conchy-runtime-{}", uid));
        if !fallback.exists() {
            fs::create_dir_all(&fallback)
                .with_context(|| format!("Failed to create fallback runtime dir at {:?}", fallback))?;
            fs::set_permissions(&fallback, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("Failed to set permissions 0700 on {:?}", fallback))?;
            info!(
                "Created secure fallback runtime directory at: {}",
                fallback.display()
            );
        } else {
            // Ensure permissions
            let _ = fs::set_permissions(&fallback, fs::Permissions::from_mode(0o700));
        }

        env::set_var("XDG_RUNTIME_DIR", &fallback);
        Ok(fallback)
    }

    /// Sets up all standard Wayland desktop environment variables in the current process
    pub fn setup_desktop_environment(custom_env: &HashMap<String, String>) -> Result<()> {
        let default_vars: &[(&str, &str)] = &[
            ("XDG_CURRENT_DESKTOP", "conchy"),
            ("XDG_SESSION_DESKTOP", "conchy"),
            ("XDG_SESSION_TYPE", "wayland"),
            ("DESKTOP_SESSION", "conchy"),
            ("QT_QPA_PLATFORM", "wayland;xcb"),
            ("QT_WAYLAND_DISABLE_WINDOWDECORATION", "1"),
            ("GDK_BACKEND", "wayland,x11,*"),
            ("SDL_VIDEODRIVER", "wayland"),
            ("CLUTTER_BACKEND", "wayland"),
            ("MOZ_ENABLE_WAYLAND", "1"),
            ("ELECTRON_OZONE_PLATFORM_HINT", "auto"),
            ("_JAVA_AWT_WM_NONREPARENTING", "1"),
        ];

        for &(key, val) in default_vars {
            env::set_var(key, val);
            debug!("Set session env var: {}={}", key, val);
        }

        // Apply custom configuration overrides
        for (key, val) in custom_env {
            env::set_var(key, val);
            debug!("Applied custom env var: {}={}", key, val);
        }

        info!("Session desktop environment configured (XDG_CURRENT_DESKTOP=conchy, WAYLAND)");
        Ok(())
    }

    /// Returns list of key-value pairs representing the core session environment to sync to D-Bus
    pub fn get_core_activation_vars() -> Vec<(String, String)> {
        let tracked_keys = [
            "XDG_CURRENT_DESKTOP",
            "XDG_SESSION_DESKTOP",
            "XDG_SESSION_TYPE",
            "DESKTOP_SESSION",
            "WAYLAND_DISPLAY",
            "DISPLAY",
            "XDG_RUNTIME_DIR",
            "QT_QPA_PLATFORM",
            "GDK_BACKEND",
            "MOZ_ENABLE_WAYLAND",
            "ELECTRON_OZONE_PLATFORM_HINT",
        ];

        let mut vars = Vec::new();
        for key in tracked_keys {
            if let Ok(val) = env::var(key) {
                vars.push((key.to_string(), val));
            }
        }
        vars
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ensure_runtime_dir() {
        let dir = EnvironmentManager::ensure_xdg_runtime_dir().expect("Should determine runtime dir");
        assert!(dir.is_dir());
    }

    #[test]
    fn test_setup_desktop_environment() {
        let mut custom = HashMap::new();
        custom.insert("CONCHY_TEST_VAR".to_string(), "active".to_string());
        EnvironmentManager::setup_desktop_environment(&custom).expect("Setup should succeed");

        assert_eq!(env::var("XDG_CURRENT_DESKTOP").unwrap(), "conchy");
        assert_eq!(env::var("XDG_SESSION_TYPE").unwrap(), "wayland");
        assert_eq!(env::var("CONCHY_TEST_VAR").unwrap(), "active");
    }
}
