use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn test_cli_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_conchy-session"))
        .arg("--help")
        .output()
        .expect("Failed to run conchy-session --help");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("conchy-session"));
    assert!(stdout.contains("--compositor"));
    assert!(stdout.contains("--dry-run"));
    assert!(stdout.contains("--dump-default-config"));
    assert!(stdout.contains("logs"));
}

#[test]
fn test_cli_dump_default_config() {
    let output = Command::new(env!("CARGO_BIN_EXE_conchy-session"))
        .arg("--dump-default-config")
        .output()
        .expect("Failed to run --dump-default-config");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("conchy-wm"));
    assert!(stdout.contains("pipewire"));
    assert!(stdout.contains("wireplumber"));
}

#[test]
fn test_cli_dry_run_does_not_create_session_log() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let state_dir = tmp.path().join("state");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&cfg_dir).unwrap();
    fs::create_dir_all(&home_dir).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_conchy-session"))
        .arg("--dry-run")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env("XDG_CONFIG_HOME", &cfg_dir)
        .env("XDG_STATE_HOME", &state_dir)
        .env("HOME", &home_dir)
        .output()
        .expect("Failed to run --dry-run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let full = format!("{stdout}\n{stderr}");
    assert!(full.contains("Performing dry-run") || full.contains("dry-run"));
    assert!(full.contains("Compositor configuration"));
    assert!(
        !state_dir
            .join("conchy")
            .join("logs")
            .join("session.log")
            .exists(),
        "--dry-run must not write session.log"
    );
}

#[test]
fn test_logs_list_and_missing_service() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");

    // Empty state directory
    let list_empty = Command::new(env!("CARGO_BIN_EXE_conchy-session"))
        .arg("logs")
        .env("XDG_STATE_HOME", &state_dir)
        .output()
        .unwrap();
    assert!(list_empty.status.success());
    assert!(String::from_utf8_lossy(&list_empty.stdout).contains("No logs found"));

    // Populate logs
    let log_dir = state_dir.join("conchy").join("logs");
    fs::create_dir_all(&log_dir).unwrap();
    fs::write(log_dir.join("session.log"), "session started\n").unwrap();
    fs::write(log_dir.join("conchy-shell.log"), "shell ok\n").unwrap();
    fs::write(log_dir.join("conchy-shell.log.1"), "older shell\n").unwrap();

    let list_populated = Command::new(env!("CARGO_BIN_EXE_conchy-session"))
        .arg("logs")
        .env("XDG_STATE_HOME", &state_dir)
        .output()
        .unwrap();
    assert!(list_populated.status.success());
    let stdout = String::from_utf8_lossy(&list_populated.stdout);
    assert!(stdout.contains("conchy-shell"));
    assert!(stdout.contains("+1 rotated"));
    assert!(stdout.contains("session"));

    let missing = Command::new(env!("CARGO_BIN_EXE_conchy-session"))
        .args(["logs", "does-not-exist"])
        .env("XDG_STATE_HOME", &state_dir)
        .output()
        .unwrap();
    assert!(!missing.status.success());
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(stderr.contains("does-not-exist"));
    assert!(stderr.contains("conchy-shell"));
}

#[test]
fn test_logs_read_lines_and_rotated_archives() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");
    let log_dir = state_dir.join("conchy").join("logs");
    fs::create_dir_all(&log_dir).unwrap();

    fs::write(log_dir.join("conchy-shell.log.2"), "oldest-1\noldest-2\n").unwrap();
    fs::write(log_dir.join("conchy-shell.log.1"), "prev-1\nprev-2\n").unwrap();
    fs::write(log_dir.join("conchy-shell.log"), "cur-1\ncur-2\n").unwrap();

    let current_only = Command::new(env!("CARGO_BIN_EXE_conchy-session"))
        .args(["logs", "conchy-shell"])
        .env("XDG_STATE_HOME", &state_dir)
        .output()
        .unwrap();
    assert!(current_only.status.success());
    assert_eq!(
        String::from_utf8_lossy(&current_only.stdout),
        "cur-1\ncur-2\n"
    );

    let all = Command::new(env!("CARGO_BIN_EXE_conchy-session"))
        .args(["logs", "conchy-shell", "--all"])
        .env("XDG_STATE_HOME", &state_dir)
        .output()
        .unwrap();
    assert!(all.status.success());
    assert_eq!(
        String::from_utf8_lossy(&all.stdout),
        "oldest-1\noldest-2\nprev-1\nprev-2\ncur-1\ncur-2\n"
    );

    let tail = Command::new(env!("CARGO_BIN_EXE_conchy-session"))
        .args(["logs", "conchy-shell", "--all", "-n", "3"])
        .env("XDG_STATE_HOME", &state_dir)
        .output()
        .unwrap();
    assert!(tail.status.success());
    assert_eq!(
        String::from_utf8_lossy(&tail.stdout),
        "prev-2\ncur-1\ncur-2\n"
    );

    let zero_means_all = Command::new(env!("CARGO_BIN_EXE_conchy-session"))
        .args(["logs", "conchy-shell.log", "-n", "0"])
        .env("XDG_STATE_HOME", &state_dir)
        .output()
        .unwrap();
    assert!(zero_means_all.status.success());
    assert_eq!(
        String::from_utf8_lossy(&zero_means_all.stdout),
        "oldest-1\noldest-2\nprev-1\nprev-2\ncur-1\ncur-2\n"
    );
}

#[test]
fn test_logs_follow_handles_append_and_rotation() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");
    let log_dir = state_dir.join("conchy").join("logs");
    fs::create_dir_all(&log_dir).unwrap();

    let log_file = log_dir.join("conchy-shell.log");
    fs::write(&log_file, "initial-line\n").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_conchy-session"))
        .args(["logs", "conchy-shell", "-f"])
        .env("XDG_STATE_HOME", &state_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    assert_eq!(first, "initial-line\n");

    // Append to existing file
    fs::write(&log_file, "initial-line\nappended-line\n").unwrap();
    let mut second = String::new();
    reader.read_line(&mut second).unwrap();
    assert_eq!(second, "appended-line\n");

    // Rotate file and write to new inode
    fs::rename(&log_file, log_dir.join("conchy-shell.log.1")).unwrap();
    fs::write(&log_file, "after-rotation-line\n").unwrap();
    let mut third = String::new();
    reader.read_line(&mut third).unwrap();
    assert_eq!(third, "after-rotation-line\n");

    let _ = child.kill();
    let _ = child.wait();
}

fn write_executable_script(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn test_session_captures_service_output_restart_reasons_and_crash_loop() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("runtime");
    let cfg_dir = tmp.path().join("config");
    let state_dir = tmp.path().join("state");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir_all(&cfg_dir).unwrap();
    fs::create_dir_all(&home_dir).unwrap();

    // Bind mock wayland-0 and compositor IPC sockets in the isolated XDG_RUNTIME_DIR
    let wayland_socket = runtime_dir.join("wayland-0");
    let listener = UnixListener::bind(&wayland_socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let ipc_socket = runtime_dir.join("conchy.wayland-0.99999.sock");
    let _ipc_listener = UnixListener::bind(&ipc_socket).unwrap();
    let stop_accept = Arc::new(AtomicBool::new(false));
    let stop_accept_thread = stop_accept.clone();
    let accept_handle = std::thread::spawn(move || {
        while !stop_accept_thread.load(Ordering::Relaxed) {
            if let Ok((stream, _)) = listener.accept() {
                drop(stream);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });

    let stop_wm_file = tmp.path().join("stop-wm");
    let wm_script = tmp.path().join("fake-wm");
    write_executable_script(
        &wm_script,
        &format!(
            "#!/bin/sh\necho 'fake-wm stdout ready'\necho 'fake-wm stderr notice' >&2\nwhile [ ! -f '{}' ]; do sleep 0.05; done\nexit 0\n",
            stop_wm_file.display()
        ),
    );

    let shell_script = tmp.path().join("fake-shell");
    write_executable_script(
        &shell_script,
        "#!/bin/sh\necho \"shell stdout booting socket=$CONCHY_SOCKET niri=$NIRI_SOCKET\"\necho 'shell stderr fatal error' >&2\nexit 42\n",
    );

    let sig_script = tmp.path().join("fake-sig");
    write_executable_script(
        &sig_script,
        "#!/bin/sh\necho 'sig service before abort'\nkill -ABRT $$\n",
    );

    let cfg_path = tmp.path().join("session.toml");
    let toml_cfg = format!(
        r#"
[session]
socket_wait_timeout_secs = 5
autostart = false

[compositor]
command = "{}"

[[services]]
name = "conchy-shell"
command = "{}"
restart = "always"
restart_delay_ms = 20
max_restarts = 2
restart_window_secs = 60
wait_for_wayland = true

[[services]]
name = "sig-tester"
command = "{}"
restart = "never"
wait_for_wayland = true
"#,
        wm_script.display(),
        shell_script.display(),
        sig_script.display()
    );
    fs::write(&cfg_path, toml_cfg).unwrap();

    // Use a nonexistent private bus path so DbusManager neither spawns a host dbus-daemon nor touches host bus.
    let fake_bus = format!("unix:path={}", tmp.path().join("unused-bus").display());
    let mut child = Command::new(env!("CARGO_BIN_EXE_conchy-session"))
        .args(["-c", cfg_path.to_str().unwrap(), "--no-autostart"])
        .env_remove("WAYLAND_DISPLAY")
        .env("DBUS_SESSION_BUS_ADDRESS", &fake_bus)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("XDG_CONFIG_HOME", &cfg_dir)
        .env("XDG_STATE_HOME", &state_dir)
        .env("HOME", &home_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn isolated conchy-session");

    let log_dir = state_dir.join("conchy").join("logs");
    let session_log_path = log_dir.join("session.log");
    let shell_log_path = log_dir.join("conchy-shell.log");
    let sig_log_path = log_dir.join("sig-tester.log");

    // Wait up to 8 seconds for crash-loop protection and signal exit to be logged
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut observed = false;
    while Instant::now() < deadline {
        let session_text = fs::read_to_string(&session_log_path).unwrap_or_default();
        let sig_text = fs::read_to_string(&sig_log_path).unwrap_or_default();
        if session_text.contains("Crash-loop protection triggered for service 'conchy-shell'")
            && sig_text.contains("SIGABRT")
        {
            observed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    fs::write(&stop_wm_file, "stop").unwrap();
    let status = child.wait().expect("Failed waiting on conchy-session");
    stop_accept.store(true, Ordering::Relaxed);
    accept_handle.join().unwrap();

    assert!(
        observed,
        "Expected crash-loop protection & SIGABRT logs before timeout.\nsession.log:\n{}\nshell.log:\n{}\nsig.log:\n{}",
        fs::read_to_string(&session_log_path).unwrap_or_default(),
        fs::read_to_string(&shell_log_path).unwrap_or_default(),
        fs::read_to_string(&sig_log_path).unwrap_or_default(),
    );
    assert!(
        status.success(),
        "conchy-session should exit cleanly: {status}"
    );

    // Verify `conchy-session logs conchy-shell`
    let shell_logs_cmd = Command::new(env!("CARGO_BIN_EXE_conchy-session"))
        .args(["logs", "conchy-shell"])
        .env("XDG_STATE_HOME", &state_dir)
        .output()
        .unwrap();
    assert!(shell_logs_cmd.status.success());
    let shell_logs = String::from_utf8_lossy(&shell_logs_cmd.stdout);
    let expected_ipc = ipc_socket.to_string_lossy().into_owned();
    assert!(shell_logs.contains(&format!("socket={expected_ipc} niri={expected_ipc}")));
    assert!(shell_logs.contains("shell stderr fatal error"));
    assert!(shell_logs.contains("exited with non-zero code 42"));
    assert!(shell_logs.contains("scheduling restart #1 (1/2 in 60s window)"));
    assert!(shell_logs.contains("scheduling restart #2 (2/2 in 60s window)"));
    assert!(shell_logs.contains("Crash-loop protection triggered for service 'conchy-shell'"));
    assert!(shell_logs.contains("total restarts: 2"));

    // Verify `conchy-session logs session`
    let session_logs_cmd = Command::new(env!("CARGO_BIN_EXE_conchy-session"))
        .args(["logs", "session"])
        .env("XDG_STATE_HOME", &state_dir)
        .output()
        .unwrap();
    assert!(session_logs_cmd.status.success());
    let session_logs = String::from_utf8_lossy(&session_logs_cmd.stdout);
    assert!(session_logs.contains("Starting conchy-session"));
    assert!(session_logs.contains("exited with non-zero code 42"));
    assert!(session_logs.contains("Crash-loop protection triggered for service 'conchy-shell'"));
    assert!(session_logs.contains("SIGABRT"));

    // Verify compositor log captured stdout and stderr
    let wm_logs_cmd = Command::new(env!("CARGO_BIN_EXE_conchy-session"))
        .args(["logs", "fake-wm"])
        .env("XDG_STATE_HOME", &state_dir)
        .output()
        .unwrap();
    assert!(wm_logs_cmd.status.success());
    let wm_logs = String::from_utf8_lossy(&wm_logs_cmd.stdout);
    assert!(wm_logs.contains("fake-wm stdout ready"));
    assert!(wm_logs.contains("fake-wm stderr notice"));
}
