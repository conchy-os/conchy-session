use anyhow::{bail, Result};
use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::sleep;
use tracing::{debug, info};

pub struct WaylandWatcher;

impl WaylandWatcher {
    /// Waits until the Wayland compositor socket appears in XDG_RUNTIME_DIR and accepts connections.
    /// Once connected, exports WAYLAND_DISPLAY into process environment.
    pub async fn wait_for_display(
        runtime_dir: &Path,
        preferred_display: Option<&str>,
        timeout_secs: u64,
    ) -> Result<String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);

        debug!(
            "Waiting for Wayland socket in {} (timeout {}s)...",
            runtime_dir.display(),
            timeout_secs
        );

        while std::time::Instant::now() < deadline {
            // If preferred display is specified, check that one first
            if let Some(name) = preferred_display {
                let path = runtime_dir.join(name);
                if path.exists() && Self::can_connect(&path).await {
                    info!("Connected to preferred Wayland display: {}", name);
                    env::set_var("WAYLAND_DISPLAY", name);
                    return Ok(name.to_string());
                }
            } else if let Ok(existing) = env::var("WAYLAND_DISPLAY") {
                let path = runtime_dir.join(&existing);
                if path.exists() && Self::can_connect(&path).await {
                    info!("Connected to existing WAYLAND_DISPLAY: {}", existing);
                    return Ok(existing);
                }
            }

            // Otherwise, search for wayland-0, wayland-1, etc.
            if let Ok(mut entries) = tokio::fs::read_dir(runtime_dir).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let file_name = entry.file_name();
                    let name_str = file_name.to_string_lossy();

                    // Check for standard wayland-N sockets (and skip .lock files)
                    if name_str.starts_with("wayland-") && !name_str.ends_with(".lock") {
                        let path = entry.path();
                        if Self::can_connect(&path).await {
                            info!("Detected active Wayland socket: {}", name_str);
                            env::set_var("WAYLAND_DISPLAY", &*name_str);
                            return Ok(name_str.into_owned());
                        }
                    }
                }
            }

            sleep(Duration::from_millis(50)).await;
        }

        bail!(
            "Timed out after {}s waiting for Wayland socket to appear in {}",
            timeout_secs,
            runtime_dir.display()
        )
    }

    /// Waits for conchy-wm's IPC socket (`conchy.<wayland-socket>.<pid>.sock` in the runtime
    /// dir) and exports it as `CONCHY_SOCKET` and `NIRI_SOCKET`. The compositor sets these only
    /// for its own children, while the shell and other services are children of the session.
    /// Returns `None` for compositors without this IPC (after `timeout`).
    pub async fn export_compositor_ipc(
        runtime_dir: &Path,
        wayland_socket: &str,
        compositor_pid: u32,
        timeout: Duration,
    ) -> Option<PathBuf> {
        let exact = runtime_dir.join(format!("conchy.{wayland_socket}.{compositor_pid}.sock"));
        // A wrapper script that does not `exec` changes the PID, so also accept any
        // socket bound to this Wayland display.
        let prefix = format!("conchy.{wayland_socket}.");
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let mut found = None;
            if exact.exists() && Self::can_connect(&exact).await {
                found = Some(exact.clone());
            } else if let Ok(mut entries) = tokio::fs::read_dir(runtime_dir).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with(&prefix)
                        && name.ends_with(".sock")
                        && Self::can_connect(&entry.path()).await
                    {
                        found = Some(entry.path());
                        break;
                    }
                }
            }
            if let Some(path) = found {
                info!("Compositor IPC socket: {}", path.display());
                env::set_var("CONCHY_SOCKET", &path);
                env::set_var("NIRI_SOCKET", &path);
                return Some(path);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// Tests if a unix socket path is connectable
    async fn can_connect(socket_path: &PathBuf) -> bool {
        match UnixStream::connect(socket_path).await {
            Ok(_) => true,
            Err(e) => {
                debug!(
                    "Socket exists at {} but connect failed: {}",
                    socket_path.display(),
                    e
                );
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn test_wayland_socket_detection() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let socket_path = tmp_dir.path().join("wayland-99");

        // Spawn a dummy unix listener to emulate compositor socket
        let listener = UnixListener::bind(&socket_path).unwrap();

        let handle = tokio::spawn(async move {
            let res = WaylandWatcher::wait_for_display(tmp_dir.path(), Some("wayland-99"), 2).await;
            assert!(res.is_ok());
            assert_eq!(res.unwrap(), "wayland-99");
        });

        // Accept connection in background
        if let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_compositor_ipc_socket_found_by_display_name() {
        let tmp_dir = tempfile::tempdir().unwrap();
        // Different PID than the one we pass: found through the display-name prefix.
        let sock = tmp_dir.path().join("conchy.wayland-7.4242.sock");
        let _listener = UnixListener::bind(&sock).unwrap();
        let _other = UnixListener::bind(tmp_dir.path().join("conchy.wayland-8.1.sock")).unwrap();

        let found = WaylandWatcher::export_compositor_ipc(
            tmp_dir.path(),
            "wayland-7",
            1,
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(found.as_deref(), Some(sock.as_path()));

        let none = WaylandWatcher::export_compositor_ipc(
            tmp_dir.path(),
            "wayland-9",
            1,
            Duration::from_millis(100),
        )
        .await;
        assert!(none.is_none());
    }
}
