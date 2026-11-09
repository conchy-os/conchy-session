use anyhow::{Context, Result};
use nix::unistd::{getpid, setpgid, Pid};
use tracing::{debug, info};

/// Configures the current process as a Linux subreaper and process group leader.
///
/// Setting PR_SET_CHILD_SUBREAPER ensures that when grandchild processes orphan
/// (e.g. background daemons, detached GUI processes), they reparent to conchy-session
/// instead of init (PID 1).
///
/// Creating a dedicated process group (`setpgid(0, 0)`) allows atomic teardown of
/// the entire session with `killpg`.
pub fn setup_subreaper_and_process_group() -> Result<()> {
    let pid = getpid();

    // Set as process group leader if not already
    // If setpgid fails with EPERM, it might already be the group leader or session leader
    if let Err(err) = setpgid(Pid::from_raw(0), Pid::from_raw(0)) {
        debug!(
            "setpgid(0, 0) returned {}: may already be process/session leader",
            err
        );
    } else {
        info!("Established dedicated session process group with PGID {}", pid);
    }

    // Set PR_SET_CHILD_SUBREAPER
    unsafe {
        let ret = libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            return Err(err).context("Failed to set PR_SET_CHILD_SUBREAPER via prctl");
        }

        let mut is_subreaper: libc::c_int = 0;
        let get_ret = libc::prctl(
            libc::PR_GET_CHILD_SUBREAPER,
            &mut is_subreaper as *mut libc::c_int,
            0,
            0,
            0,
        );
        if get_ret == 0 && is_subreaper == 1 {
            info!("Linux child subreaper active for PID {}", pid);
        } else {
            debug!("PR_GET_CHILD_SUBREAPER verification returned {}", is_subreaper);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subreaper_setup() {
        // prctl(PR_SET_CHILD_SUBREAPER) is safe to call on Linux
        let res = setup_subreaper_and_process_group();
        assert!(res.is_ok(), "Subreaper setup should succeed on Linux");
    }
}
