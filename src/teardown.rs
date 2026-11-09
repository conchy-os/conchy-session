use anyhow::Result;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, info, warn};

pub struct TeardownManager;

impl TeardownManager {
    /// Gracefully stops all child processes:
    /// 1. Sends SIGTERM to all child processes.
    /// 2. Waits up to `grace_period` for processes to exit.
    /// 3. Sends SIGKILL to any remaining processes.
    /// 4. Reaps all zombie child processes.
    pub async fn shutdown_processes(child_pids: &[u32], grace_period: Duration) -> Result<()> {
        info!(
            "Initiating graceful teardown of {} session processes...",
            child_pids.len()
        );

        // Ignore SIGTERM in current process so broadcast doesn't terminate us mid-cleanup
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }

        // Send SIGTERM to all tracked child processes
        for &pid in child_pids {
            let nix_pid = Pid::from_raw(pid as i32);
            debug!("Sending SIGTERM to child process {}", pid);
            let _ = kill(nix_pid, Signal::SIGTERM);
        }

        // Wait for processes to exit within grace period
        let deadline = Instant::now() + grace_period;
        let mut remaining = child_pids.to_vec();

        while Instant::now() < deadline && !remaining.is_empty() {
            Self::reap_zombies();

            remaining.retain(|&pid| {
                // Check if process still exists (kill with signal 0 checks existence)
                let nix_pid = Pid::from_raw(pid as i32);
                kill(nix_pid, None).is_ok()
            });

            if remaining.is_empty() {
                break;
            }

            sleep(Duration::from_millis(100)).await;
        }

        // Any processes still running after grace period receive SIGKILL
        if !remaining.is_empty() {
            warn!(
                "{} processes did not exit after grace period, sending SIGKILL: {:?}",
                remaining.len(),
                remaining
            );
            for &pid in &remaining {
                let nix_pid = Pid::from_raw(pid as i32);
                let _ = kill(nix_pid, Signal::SIGKILL);
            }
        }

        // Final reap of all terminated processes
        sleep(Duration::from_millis(50)).await;
        Self::reap_zombies();

        info!("Session teardown completed cleanly.");
        Ok(())
    }

    /// Non-blocking reap of all deceased child/orphan processes (subreaper duty)
    pub fn reap_zombies() {
        loop {
            let mut status: libc::c_int = 0;
            let res = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if res <= 0 {
                break;
            }
            debug!("Reaped exited child process PID {}", res);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[tokio::test]
    async fn test_teardown_processes() {
        // Spawn a dummy sleep process
        let child = Command::new("sleep")
            .arg("10")
            .spawn()
            .expect("Failed to spawn sleep");
        let pid = child.id();

        let res = TeardownManager::shutdown_processes(&[pid], Duration::from_millis(500)).await;
        assert!(res.is_ok());

        // Process should no longer exist
        let check = kill(Pid::from_raw(pid as i32), None);
        assert!(check.is_err());
    }
}
