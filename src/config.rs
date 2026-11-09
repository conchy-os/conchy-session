use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{debug, info};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    OnFailure,
    Never,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self::OnFailure
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub restart: RestartPolicy,
    #[serde(default = "default_restart_delay_ms")]
    pub restart_delay_ms: u64,
    #[serde(default = "default_max_restarts")]
    pub max_restarts: u32,
    #[serde(default = "default_restart_window_secs")]
    pub restart_window_secs: u64,
    #[serde(default = "default_wait_for_wayland")]
    pub wait_for_wayland: bool,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_restart_delay_ms() -> u64 {
    500
}

fn default_max_restarts() -> u32 {
    5
}

fn default_restart_window_secs() -> u64 {
    60
}

fn default_wait_for_wayland() -> bool {
    true
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositorConfig {
    #[serde(default = "default_compositor_command")]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn default_compositor_command() -> String {
    "conchy-wm".to_string()
}

impl Default for CompositorConfig {
    fn default() -> Self {
        Self {
            command: default_compositor_command(),
            args: Vec::new(),
            env: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSettings {
    #[serde(default = "default_socket_wait_timeout")]
    pub socket_wait_timeout_secs: u64,
    #[serde(default = "default_true")]
    pub autostart: bool,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn default_socket_wait_timeout() -> u64 {
    10
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            socket_wait_timeout_secs: default_socket_wait_timeout(),
            autostart: true,
            env: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub session: SessionSettings,
    #[serde(default)]
    pub compositor: CompositorConfig,
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            session: SessionSettings::default(),
            compositor: CompositorConfig::default(),
            services: vec![
                ServiceConfig {
                    name: "pipewire".to_string(),
                    command: "pipewire".to_string(),
                    args: Vec::new(),
                    restart: RestartPolicy::Always,
                    restart_delay_ms: 1000,
                    max_restarts: 10,
                    restart_window_secs: 60,
                    wait_for_wayland: false,
                    env: HashMap::new(),
                    enabled: true,
                },
                ServiceConfig {
                    name: "wireplumber".to_string(),
                    command: "wireplumber".to_string(),
                    args: Vec::new(),
                    restart: RestartPolicy::Always,
                    restart_delay_ms: 1000,
                    max_restarts: 10,
                    restart_window_secs: 60,
                    wait_for_wayland: false,
                    env: HashMap::new(),
                    enabled: true,
                },
                ServiceConfig {
                    name: "conchy-shell".to_string(),
                    command: "conchy-shell".to_string(),
                    args: Vec::new(),
                    restart: RestartPolicy::Always,
                    restart_delay_ms: 1000,
                    max_restarts: 5,
                    restart_window_secs: 60,
                    wait_for_wayland: true,
                    env: HashMap::new(),
                    enabled: true,
                },
            ],
        }
    }
}

impl Config {
    /// Attempt to load configuration from given path, or fallback to standard XDG/system paths
    pub fn load(custom_path: Option<&Path>) -> Result<Self> {
        if let Some(path) = custom_path {
            info!("Loading configuration from explicit path: {:?}", path);
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read config file {:?}", path))?;
            let config: Config = toml::from_str(&content)
                .with_context(|| format!("Failed to parse TOML configuration from {:?}", path))?;
            return Ok(config);
        }

        // Standard paths:
        // 1. $XDG_CONFIG_HOME/conchy/session.toml
        // 2. ~/.config/conchy/session.toml
        // 3. /etc/conchy/session.toml
        let mut candidates = Vec::new();
        if let Ok(xdg_config) = std::env::var("XDG_CONFIG_HOME") {
            candidates.push(PathBuf::from(xdg_config).join("conchy").join("session.toml"));
        }
        if let Ok(home) = std::env::var("HOME") {
            candidates.push(PathBuf::from(home).join(".config").join("conchy").join("session.toml"));
        }
        candidates.push(PathBuf::from("/etc/conchy/session.toml"));

        for path in candidates {
            if path.is_file() {
                info!("Found session configuration at: {:?}", path);
                let content = std::fs::read_to_string(&path)
                    .with_context(|| format!("Failed to read config file {:?}", path))?;
                let config: Config = toml::from_str(&content)
                    .with_context(|| format!("Failed to parse TOML configuration from {:?}", path))?;
                return Ok(config);
            }
        }

        debug!("No config file found in standard search paths. Using built-in defaults.");
        Ok(Config::default())
    }

    /// Serialize configuration to pretty TOML string
    pub fn to_toml_string(&self) -> Result<String> {
        toml::to_string_pretty(self).context("Failed to serialize config to TOML")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_serialization() {
        let config = Config::default();
        let toml_str = config.to_toml_string().expect("Failed to serialize");
        assert!(toml_str.contains("conchy-wm"));
        assert!(toml_str.contains("pipewire"));
        assert!(toml_str.contains("wireplumber"));

        let parsed: Config = toml::from_str(&toml_str).expect("Failed to parse back");
        assert_eq!(parsed.compositor.command, "conchy-wm");
        assert_eq!(parsed.services.len(), 3);
    }

    #[test]
    fn test_custom_config_deserialization() {
        let raw = r#"
[compositor]
command = "/custom/path/conchy-wm"
args = ["--config", "/custom/niri.kdl"]

[session]
socket_wait_timeout_secs = 15
autostart = false

[[services]]
name = "mako"
command = "mako"
restart = "always"
wait_for_wayland = true
"#;
        let config: Config = toml::from_str(raw).expect("Failed to parse");
        assert_eq!(config.compositor.command, "/custom/path/conchy-wm");
        assert_eq!(config.compositor.args, vec!["--config", "/custom/niri.kdl"]);
        assert_eq!(config.session.socket_wait_timeout_secs, 15);
        assert!(!config.session.autostart);
        assert_eq!(config.services.len(), 1);
        assert_eq!(config.services[0].name, "mako");
        assert_eq!(config.services[0].restart, RestartPolicy::Always);
    }
}
