# AGENTS.md — `conchy-session`

Guidance for AI coding agents and contributors working in `/home/michael/Projects/conchy/conchy-session`.

---

## 1. Shared Conchy OS Rules

- **Rolling release**: Conchy OS is a rolling distribution with no version numbers or versioned release tags; packages are date-built from `main`. Do not introduce version-tag release workflows or wording implying fixed OS releases. (`Cargo.toml` keeps `version = "0.1.0"` solely because Cargo requires a semver field.)
- **Rust-only for our own code**: Write all Conchy OS components in pure Rust. Never add GTK, Qt, Electron, or C/C++/Python/JS reimplementations for first-party code.
- **One utility = one task**: `conchy-session` handles Wayland user session orchestration only (`XDG_RUNTIME_DIR` / desktop environment setup, D-Bus session bus & activation sync, compositor/service supervision, XDG autostart, Linux subreaper/process group setup, and graceful teardown). Keep configuration schemas in `conchy-config` and compositor/shell logic in their own repositories.
- **Target distributions**: **Void Linux (`glibc` / `runit` / `xbps`)** is the primary target; **Arch Linux** is secondary. Never introduce `systemd --user` or `logind`-only assumptions.
- **Git workflow**: Single long-lived branch `main`. Use short-lived PR branches for changes and delete them after merge. Do not run `git commit` or `git push` unless explicitly instructed.
- **Roadmap workflow**: Project-wide tracking lives in `../conchy-os/ROADMAP.md` and `../conchy-os/roadmap.json`. Check `depends` and architectural `decisions`, verify all `done_when` criteria before marking tasks done, and keep task status synchronized in `conchy-os` when assigned there. Do not modify `../conchy-os` or sibling repositories when assigned only to `conchy-session`.
- **Mandatory Rust verification gate**: Every Rust change must pass with zero warnings:
  ```sh
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
  ```

---

## 2. Safety & Session Isolation Rules (CRITICAL)

- **NEVER start bare `conchy-session` inside a live desktop session**:
  - Without `--dry-run` or `--dump-default-config`, `conchy-session` calls `prctl(PR_SET_CHILD_SUBREAPER, 1)` and `setpgid(0, 0)`, overwrites session desktop environment variables, calls `org.freedesktop.DBus.UpdateActivationEnvironment` on the session D-Bus, and attempts to launch `conchy-wm`, supervised services (`pipewire`, `wireplumber`, `conchy-config daemon`, `conchy-shell daemon`), and XDG autostart apps, then sends `SIGTERM`/`SIGKILL` during teardown.
- **Never touch the live user desktop session**:
  - Do NOT run `hyprctl`, send IPC to the user's running compositor, or kill/restart live session processes (`conchy-wm`, `conchy-shell`, `conchy-session`, `conchy-config`).
  - Do NOT edit `~/.config`, `~/.local`, `/etc`, `/usr`, or sibling repositories (`../config`, `../conchy-config`, etc.) when working in `conchy-session`.
- **Isolate `XDG_CONFIG_HOME` (and `HOME` / D-Bus when needed)**:
  - For manual CLI checks or tests reading configuration/autostart directories, isolate `XDG_CONFIG_HOME` inside a temporary directory via `mktemp -d` (and clean it up afterward).
  - **The `dconf` / `gsettings` trap**: Redirecting `XDG_CONFIG_HOME` alone does **not** isolate GLib/GTK/`dconf`/`gsettings` subprocesses while `DBUS_SESSION_BUS_ADDRESS` points to the host session bus. Always unset `DBUS_SESSION_BUS_ADDRESS` (or run inside `dbus-run-session` + `GSETTINGS_BACKEND=memory`) if any command or child process could touch D-Bus or `dconf`.
- **Never use `pkill -f` matching your own command**: `pkill -f` matches full command lines and can terminate your own shell/agent process or unrelated desktop processes.
- **Never load unverified `.so` files**: Do not test with untrusted shared libraries or `LD_PRELOAD`.

---

## 3. Repository Specifics (`conchy-session`)

### Sibling Path Dependency
- `Cargo.toml` declares:
  ```toml
  conchy-config = { path = "../config" }
  ```
- In the multi-repo Conchy OS workspace, `../config` is a symlink `config -> conchy-config`. Ensure that symlink exists locally (and in CI) before running Cargo commands. Do **not** modify `../config` or `../conchy-config` when working exclusively in `conchy-session`.

### Comment & Documentation Style
- **Comment language**: **English** across all Rust source files (`src/`, `tests/`). Preserve all existing comments and docstrings unrelated to your changes.
- **Configuration format**: `session.kdl` is the primary format (`$XDG_CONFIG_HOME/conchy/session.kdl`, `~/.config/conchy/session.kdl`, `/etc/conchy/session.kdl`), parsed via `conchy_config::schema::session`. `session.toml` is retained only as a legacy fallback.

### Code & Resource Layout
- `src/main.rs` — entrypoint, `logs` CLI dispatch, tracing initialization (`stderr` + rotating `session.log`), `--dump-default-config`, subreaper setup, config loading, CLI overrides, and supervisor invocation.
- `src/cli.rs` — `clap` CLI definitions (`logs` subcommand, `-c`/`--config`, `--compositor`, `--no-autostart`, `--dry-run`, `--dump-default-config`, `-l`/`--log-level`).
- `src/logs.rs` — rotating `session.log`, `autostart.log`, and per-service/compositor line-timestamped logs under `$XDG_STATE_HOME/conchy/logs` (`0700`/`0600`), ANSI stripping, lifecycle markers, and `conchy-session logs <service> [-f]`.
- `src/config.rs` — `Config::load` (`.kdl` primary, `.toml` legacy fallback), `From<KdlSessionConfig>`, `RestartPolicy`, and `to_kdl_string()`.
- `src/env.rs` — `EnvironmentManager`: `XDG_RUNTIME_DIR` validation/fallback, `XDG_CURRENT_DESKTOP=conchy` and Wayland toolkit variables, and D-Bus activation var list (including `CONCHY_SOCKET`/`NIRI_SOCKET`).
- `src/dbus.rs` — `DbusManager`: session bus discovery/launch (`dbus-daemon --session`) and `org.freedesktop.DBus.UpdateActivationEnvironment`.
- `src/subreaper.rs` — `setpgid(0, 0)` and `prctl(PR_SET_CHILD_SUBREAPER, 1)`.
- `src/supervisor.rs` — `Supervisor`: `--dry-run` validation, phased service startup (pre-Wayland, compositor, post-Wayland), XDG autostart launch, crash-loop-protected restart loop, signal handling (`SIGTERM`/`SIGINT`/`SIGHUP`).
- `src/wayland.rs` — `WaylandWatcher`: polls `$XDG_RUNTIME_DIR/wayland-*` for socket readiness and exports compositor IPC socket (`CONCHY_SOCKET`/`NIRI_SOCKET`).
- `src/autostart.rs` — `AutostartManager`: XDG Desktop Application Autostart spec parser (`OnlyShowIn` `conchy`/`wlroots`, `NotShowIn`, `TryExec`, `Hidden`, `%` field-code stripping).
- `src/teardown.rs` — `TeardownManager`: `SIGTERM` grace period -> `SIGKILL` escalation and `waitpid(-1, ..., WNOHANG)` zombie reaping.
- `tests/integration.rs` — CLI and isolated end-to-end session integration tests (`--help`, `--dump-default-config`, `--dry-run`, `logs`, rotation `-f`, IPC socket export, crash-loop logging).
- `resources/conchy.desktop` — `/usr/share/wayland-sessions/conchy.desktop` (`Exec=conchy-session`, `DesktopNames=conchy`).
- `resources/session.kdl` — reference default KDL session config; `resources/session.toml` — legacy TOML reference.

---

## 4. Build, Verification & Safe Testing Commands

```sh
# Build
cargo build --workspace
cargo build --release

# Format (apply / check)
cargo fmt --all
cargo fmt --all -- --check

# Clippy (strict, all targets)
cargo clippy --workspace --all-targets -- -D warnings

# Run all unit and integration tests (isolated from host config & D-Bus)
TMP_CFG="$(mktemp -d)"
env -u DBUS_SESSION_BUS_ADDRESS XDG_CONFIG_HOME="$TMP_CFG" cargo test --workspace
rm -rf "$TMP_CFG"
```

### Safe Manual Checks (Non-Destructive)

```sh
# Print built-in default KDL configuration to stdout and exit
cargo run -- --dump-default-config

# Validate config, XDG_RUNTIME_DIR, binaries in PATH, and autostart without launching a session
TMP_CFG="$(mktemp -d)"
env -u DBUS_SESSION_BUS_ADDRESS XDG_CONFIG_HOME="$TMP_CFG" cargo run -- --dry-run
rm -rf "$TMP_CFG"

# Dry-run against an explicit temporary session.kdl
TMP_CFG="$(mktemp -d)"
cargo run -- --dump-default-config > "$TMP_CFG/session.kdl"
env -u DBUS_SESSION_BUS_ADDRESS XDG_CONFIG_HOME="$TMP_CFG" cargo run -- -c "$TMP_CFG/session.kdl" --dry-run --no-autostart
rm -rf "$TMP_CFG"
```

Void Linux CI runs in `.github/workflows/ci.yml` inside `ghcr.io/void-linux/void-glibc-full:latest` and enforces `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`.
