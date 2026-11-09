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
}
