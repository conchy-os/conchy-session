use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutostartApp {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

pub struct AutostartManager;

impl AutostartManager {
    /// Collects and parses all autostart entries according to XDG Desktop Application Autostart Specification
    pub fn collect_autostart_apps() -> Vec<AutostartApp> {
        let mut search_dirs = Vec::new();

        // 1. User autostart: $XDG_CONFIG_HOME/autostart or ~/.config/autostart
        if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
            search_dirs.push(PathBuf::from(config_home).join("autostart"));
        } else if let Ok(home) = env::var("HOME") {
            search_dirs.push(PathBuf::from(home).join(".config").join("autostart"));
        }

        // 2. System autostart: $XDG_CONFIG_DIRS/autostart or /etc/xdg/autostart
        if let Ok(config_dirs) = env::var("XDG_CONFIG_DIRS") {
            for dir in config_dirs.split(':') {
                if !dir.is_empty() {
                    search_dirs.push(PathBuf::from(dir).join("autostart"));
                }
            }
        }
        search_dirs.push(PathBuf::from("/etc/xdg/autostart"));

        let mut processed_files = HashSet::new();
        let mut apps = Vec::new();

        for dir in search_dirs {
            if !dir.is_dir() {
                continue;
            }

            debug!("Scanning autostart directory: {}", dir.display());
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) != Some("desktop") {
                        continue;
                    }

                    let file_name = entry.file_name().to_string_lossy().into_owned();
                    if processed_files.contains(&file_name) {
                        // Already processed by higher priority directory
                        continue;
                    }
                    processed_files.insert(file_name.clone());

                    match Self::parse_desktop_file(&path) {
                        Ok(Some(app)) => {
                            info!("Autostart enabled: {} ({})", app.name, app.command);
                            apps.push(app);
                        }
                        Ok(None) => {
                            debug!("Skipping autostart file {}: filtered or hidden", path.display());
                        }
                        Err(e) => {
                            warn!("Failed to parse autostart file {}: {}", path.display(), e);
                        }
                    }
                }
            }
        }

        apps
    }

    /// Parses a single .desktop file according to Desktop Entry standard
    pub fn parse_desktop_file(path: &Path) -> Result<Option<AutostartApp>, std::io::Error> {
        let content = fs::read_to_string(path)?;
        let mut in_desktop_entry = false;
        let mut key_values = HashMap::new();

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                in_desktop_entry = trimmed == "[Desktop Entry]";
                continue;
            }

            if in_desktop_entry {
                if let Some((k, v)) = trimmed.split_once('=') {
                    key_values.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
        }

        // Type must be Application
        if key_values.get("Type").map(|s| s.as_str()) != Some("Application") {
            return Ok(None);
        }

        // Hidden=true skips application
        if key_values.get("Hidden").map(|s| s.as_str()) == Some("true") {
            return Ok(None);
        }

        // Check OnlyShowIn / NotShowIn
        // Our desktop identifier is "conchy"
        let current_desktop = "conchy";
        if let Some(only_show) = key_values.get("OnlyShowIn") {
            let matched = only_show
                .split(';')
                .any(|d| d.trim().eq_ignore_ascii_case(current_desktop) || d.trim().eq_ignore_ascii_case("wlroots"));
            if !matched {
                return Ok(None);
            }
        }

        if let Some(not_show) = key_values.get("NotShowIn") {
            let excluded = not_show
                .split(';')
                .any(|d| d.trim().eq_ignore_ascii_case(current_desktop));
            if excluded {
                return Ok(None);
            }
        }

        // TryExec: if present, binary must exist in PATH
        if let Some(try_exec) = key_values.get("TryExec") {
            if !Self::binary_exists(try_exec) {
                debug!("TryExec '{}' not found in PATH, skipping", try_exec);
                return Ok(None);
            }
        }

        // Exec: command to execute
        let exec_str = match key_values.get("Exec") {
            Some(e) => e,
            None => return Ok(None),
        };

        let (cmd, args) = Self::parse_exec_line(exec_str);
        if cmd.is_empty() {
            return Ok(None);
        }

        let name = key_values
            .get("Name")
            .cloned()
            .unwrap_or_else(|| path.file_stem().unwrap().to_string_lossy().into_owned());

        Ok(Some(AutostartApp {
            name,
            command: cmd,
            args,
        }))
    }

    /// Cleans field codes like %u, %F and parses argv tokens
    fn parse_exec_line(exec_str: &str) -> (String, Vec<String>) {
        let mut tokens = Vec::new();
        let mut current_token = String::new();
        let mut in_quotes = false;
        let mut escape_next = false;

        for ch in exec_str.chars() {
            if escape_next {
                current_token.push(ch);
                escape_next = false;
                continue;
            }

            match ch {
                '\\' => {
                    escape_next = true;
                }
                '"' => {
                    in_quotes = !in_quotes;
                }
                c if c.is_whitespace() && !in_quotes => {
                    if !current_token.is_empty() {
                        tokens.push(current_token);
                        current_token = String::new();
                    }
                }
                c => {
                    current_token.push(c);
                }
            }
        }

        if !current_token.is_empty() {
            tokens.push(current_token);
        }

        if tokens.is_empty() {
            return (String::new(), Vec::new());
        }

        // Filter out Desktop Entry field codes (%f, %u, etc.)
        let command = tokens.remove(0);
        let args = tokens
            .into_iter()
            .filter(|arg| !arg.starts_with('%'))
            .collect();

        (command, args)
    }

    /// Checks if a binary name exists in PATH or is an existing absolute executable
    fn binary_exists(bin_name: &str) -> bool {
        let p = Path::new(bin_name);
        if p.is_absolute() {
            return p.is_file();
        }

        if let Ok(path_var) = env::var("PATH") {
            for dir in env::split_paths(&path_var) {
                let full = dir.join(bin_name);
                if full.is_file() {
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_exec_line() {
        let line = r#"my-app --flag "arg with spaces" %u %F"#;
        let (cmd, args) = AutostartManager::parse_exec_line(line);
        assert_eq!(cmd, "my-app");
        assert_eq!(args, vec!["--flag", "arg with spaces"]);
    }

    #[test]
    fn test_parse_desktop_file_valid() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            "[Desktop Entry]\nType=Application\nName=Test App\nExec=echo test %f\nHidden=false\n"
        )
        .unwrap();

        let app = AutostartManager::parse_desktop_file(file.path())
            .unwrap()
            .expect("Should parse");
        assert_eq!(app.name, "Test App");
        assert_eq!(app.command, "echo");
        assert_eq!(app.args, vec!["test"]);
    }

    #[test]
    fn test_parse_desktop_file_hidden() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            "[Desktop Entry]\nType=Application\nName=Hidden App\nExec=echo\nHidden=true\n"
        )
        .unwrap();

        let app = AutostartManager::parse_desktop_file(file.path()).unwrap();
        assert!(app.is_none());
    }

    #[test]
    fn test_parse_desktop_file_only_show_in() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            "[Desktop Entry]\nType=Application\nName=KDE App\nExec=echo\nOnlyShowIn=KDE;GNOME;\n"
        )
        .unwrap();

        let app = AutostartManager::parse_desktop_file(file.path()).unwrap();
        assert!(app.is_none(), "Should be skipped because conchy not in OnlyShowIn");
    }
}
