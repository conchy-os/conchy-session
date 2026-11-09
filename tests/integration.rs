use std::process::Command;

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
fn test_cli_dry_run() {
    let output = Command::new(env!("CARGO_BIN_EXE_conchy-session"))
        .arg("--dry-run")
        .output()
        .expect("Failed to run --dry-run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let full = format!("{}\n{}", stdout, stderr);
    assert!(full.contains("Performing dry-run") || full.contains("dry-run"));
    assert!(full.contains("Compositor configuration"));
}
