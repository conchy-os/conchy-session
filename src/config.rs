use anyhow::{Context, Result};
use kdl::KdlDocument;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{debug, info};

use conchy_config::schema::session::{default_session_kdl, parse_session_config, SessionConfig as KdlSessionConfig};

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

impl From<KdlSessionConfig> for Config {
    fn from(s: KdlSessionConfig) -> Self {
        Self {
            session: SessionSettings {
                socket_wait_timeout_secs: s.socket_wait_timeout_secs,
                autostart: s.autostart,
                env: s.env,
            },
            compositor: CompositorConfig {
                command: s.compositor,
                args: s.compositor_args,
                env: HashMap::new(),
            },
            services: s
                .services
                .into_iter()
                .map(|svc| ServiceConfig {
                    name: svc.name,
                    command: svc.command,
                    args: svc.args,
                    restart: match svc.restart.as_str() {
                        "always" => RestartPolicy::Always,
                        "never" => RestartPolicy::Never,
                        _ => RestartPolicy::OnFailure,
                    },
                    restart_delay_ms: 1000,
                    max_restarts: 10,
                    restart_window_secs: 60,
                    wait_for_wayland: svc.wait_for_wayland,
                    env: HashMap::new(),
                    enabled: true,
                })
                .collect(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let doc: KdlDocument = default_session_kdl().parse().unwrap();
        let kdl_cfg = parse_session_config(&doc).unwrap();
        kdl_cfg.into()
    }
}

impl Config {
    /// Attempt to load configuration from given path, or fallback to standard XDG/system paths.
    /// Supports both session.kdl (standard) and session.toml (legacy).
    pub fn load(custom_path: Option<&Path>) -> Result<Self> {
        if let Some(path) = custom_path {
            info!("Loading configuration from explicit path: {:?}", path);
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read config file {:?}", path))?;

            if path.extension().and_then(|s| s.to_str()) == Some("kdl") {
                let doc: KdlDocument = content
                    .parse()
                    .with_context(|| format!("Failed to parse KDL syntax in {:?}", path))?;
                let kdl_cfg = parse_session_config(&doc)
                    .with_context(|| format!("Failed to validate KDL session config in {:?}", path))?;
                return Ok(kdl_cfg.into());
            }

            let config: Config = toml::from_str(&content)
                .with_context(|| format!("Failed to parse TOML configuration from {:?}", path))?;
            return Ok(config);
        }

        // Standard search paths: prefer .kdl, fallback to .toml
        let mut candidates = Vec::new();
        if let Ok(xdg_config) = std::env::var("XDG_CONFIG_HOME") {
            candidates.push(PathBuf::from(&xdg_config).join("conchy").join("session.kdl"));
            candidates.push(PathBuf::from(&xdg_config).join("conchy").join("session.toml"));
        }
        if let Ok(home) = std::env::var("HOME") {
            candidates.push(PathBuf::from(&home).join(".config").join("conchy").join("session.kdl"));
            candidates.push(PathBuf::from(&home).join(".config").join("conchy").join("session.toml"));
        }
        candidates.push(PathBuf::from("/etc/conchy/session.kdl"));
        candidates.push(PathBuf::from("/etc/conchy/session.toml"));

        for path in candidates {
            if path.is_file() {
                info!("Found session configuration at: {:?}", path);
                let content = std::fs::read_to_string(&path)
                    .with_context(|| format!("Failed to read config file {:?}", path))?;

                if path.extension().and_then(|s| s.to_str()) == Some("kdl") {
                    let doc: KdlDocument = content
                        .parse()
                        .with_context(|| format!("Failed to parse KDL syntax in {:?}", path))?;
                    let kdl_cfg = parse_session_config(&doc)
                        .with_context(|| format!("Failed to validate KDL session config in {:?}", path))?;
                    return Ok(kdl_cfg.into());
                }

                let config: Config = toml::from_str(&content)
                    .with_context(|| format!("Failed to parse TOML configuration from {:?}", path))?;
                return Ok(config);
            }
        }

        debug!("No config file found in standard search paths. Using built-in KDL defaults.");
        Ok(Config::default())
    }

    /// Returns standard default KDL string representation
    pub fn to_kdl_string(&self) -> String {
        default_session_kdl().to_string()
    }

    /// Serialize configuration to pretty TOML string (backwards compatibility)
    #[allow(dead_code)]
    pub fn to_toml_string(&self) -> Result<String> {
        toml::to_string_pretty(self).context("Failed to serialize config to TOML")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_loading() {
        let config = Config::default();
        assert_eq!(config.compositor.command, "conchy-wm");
        assert_eq!(config.services.len(), 4);
        assert_eq!(config.services[0].name, "pipewire");
        assert_eq!(config.services[2].name, "conchy-config");
    }

    #[test]
    fn test_custom_kdl_parsing() {
        let raw = r#"
session {
    socket-wait-timeout 20
    autostart false
}

compositor "/custom/conchy-wm"

service "mako" {
    command "mako"
    restart "always"
    wait-for-wayland true
}
"#;
        let doc: KdlDocument = raw.parse().unwrap();
        let kdl_cfg = parse_session_config(&doc).unwrap();
        let config: Config = kdl_cfg.into();

        assert_eq!(config.compositor.command, "/custom/conchy-wm");
        assert_eq!(config.session.socket_wait_timeout_secs, 20);
        assert!(!config.session.autostart);
        assert_eq!(config.services.len(), 1);
        assert_eq!(config.services[0].name, "mako");
    }
}
