use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "conchy-session",
    author = "Conchy OS Team",
    version,
    about = "Lightweight Wayland user session orchestrator and process supervisor for Conchy OS / Void Linux",
    long_about = "conchy-session manages a complete Wayland user session without depending on systemd --user.\n\
                  It configures desktop environment variables, manages the D-Bus activation environment,\n\
                  launches the conchy-wm compositor, supervises essential services (pipewire, shell, polkit),\n\
                  processes XDG autostart applications, acts as a Linux subreaper, and guarantees clean\n\
                  process group teardown on exit."
)]
pub struct Cli {
    /// Path to session configuration file [default: $XDG_CONFIG_HOME/conchy/session.toml]
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Override the compositor command (e.g. /usr/bin/conchy-wm or "conchy-wm --verbose")
    #[arg(long, value_name = "COMMAND")]
    pub compositor: Option<String>,

    /// Disable processing XDG autostart entries (~/.config/autostart, /etc/xdg/autostart)
    #[arg(long)]
    pub no_autostart: bool,

    /// Validate configuration and environment without launching the session
    #[arg(long)]
    pub dry_run: bool,

    /// Print default session configuration (TOML) to stdout and exit
    #[arg(long)]
    pub dump_default_config: bool,

    /// Log level filter (trace, debug, info, warn, error) [default: info]
    #[arg(short, long, env = "CONCHY_LOG", default_value = "info")]
    pub log_level: String,
}
