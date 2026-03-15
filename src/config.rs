use std::collections::HashMap;
use std::path::PathBuf;

use chrono::Weekday;
use chrono_tz::Tz;

#[derive(Debug, Clone)]
pub struct Config {
    pub claude_code_root: String,
    pub db_path: PathBuf,
    pub tz: Option<Tz>,
    pub retention_days: u32,
    pub rollup_retention_days: u32,
    pub daemon_sock: PathBuf,
    pub no_cost: bool,
    pub output_format: String,
    pub start_of_week: Weekday,
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

impl Config {
    pub fn new() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));

        let mut config = Config {
            claude_code_root: home.join(".claude").to_string_lossy().to_string(),
            db_path: home.join(".config").join("toki").join("toki.fjall"),
            tz: None,
            retention_days: 0,
            rollup_retention_days: 0,
            daemon_sock: crate::daemon::default_sock_path(),
            no_cost: false,
            output_format: "table".to_string(),
            start_of_week: Weekday::Mon,
        };

        // Auto-load from settings file
        config.load_from_settings_file();
        config
    }

    pub fn with_db_path(mut self, path: PathBuf) -> Self {
        self.db_path = path;
        self
    }

    pub fn with_tz(mut self, tz: Option<Tz>) -> Self {
        self.tz = tz;
        self
    }

    /// Load overrides from settings file (~/.config/toki/settings.json).
    pub fn load_from_settings_file(&mut self) {
        let settings = match load_settings_file() {
            Some(s) => s,
            None => return,
        };

        if let Some(v) = settings.get("claude_code_root").and_then(|v| v.as_str()) {
            self.claude_code_root = v.to_string();
        }
        if let Some(v) = settings.get("retention_days").and_then(|v| v.as_str()) {
            if let Ok(n) = v.parse::<u32>() { self.retention_days = n; }
        }
        if let Some(v) = settings.get("rollup_retention_days").and_then(|v| v.as_str()) {
            if let Ok(n) = v.parse::<u32>() { self.rollup_retention_days = n; }
        }
        if let Some(v) = settings.get("daemon_sock").and_then(|v| v.as_str()) {
            self.daemon_sock = PathBuf::from(v);
        }
        if let Some(v) = settings.get("timezone").and_then(|v| v.as_str()) {
            if !v.is_empty() {
                if let Ok(tz) = v.parse::<Tz>() {
                    self.tz = Some(tz);
                }
            }
        }
        if let Some(v) = settings.get("no_cost").and_then(|v| v.as_str()) {
            self.no_cost = v == "true";
        }
        if let Some(v) = settings.get("output_format").and_then(|v| v.as_str()) {
            if v == "table" || v == "json" {
                self.output_format = v.to_string();
            }
        }
        if let Some(v) = settings.get("start_of_week").and_then(|v| v.as_str()) {
            if let Some(w) = parse_weekday(v) {
                self.start_of_week = w;
            }
        }
    }
}

// ── File-based settings ──

/// Default settings file path.
pub fn settings_file_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("toki").join("settings.json")
}

/// Load settings from JSON file.
fn load_settings_file() -> Option<HashMap<String, serde_json::Value>> {
    let path = settings_file_path();
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Save a single setting to the settings file (read-modify-write with flock).
pub fn set_setting(key: &str, value: &str) -> Result<(), String> {
    let path = settings_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    // File lock to prevent concurrent read-modify-write races
    let lock_path = path.with_extension("lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true).truncate(false).read(true).write(true)
        .open(&lock_path).map_err(|e| e.to_string())?;
    use fs2::FileExt;
    lock_file.lock_exclusive().map_err(|e| format!("settings lock: {}", e))?;

    let mut settings: HashMap<String, serde_json::Value> = if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|data| serde_json::from_str(&data).ok())
            .unwrap_or_default()
    } else {
        HashMap::new()
    };

    settings.insert(key.to_string(), serde_json::Value::String(value.to_string()));

    let tmp = path.with_extension("tmp");
    let json = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, &json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;

    lock_file.unlock().ok();
    Ok(())
}

/// Get a single setting from the settings file.
pub fn get_setting(key: &str) -> Option<String> {
    let settings = load_settings_file()?;
    settings.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// List all settings as (key, value) pairs.
pub fn list_settings() -> HashMap<String, String> {
    let settings = load_settings_file().unwrap_or_default();
    settings.into_iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
        .collect()
}

pub fn parse_weekday(s: &str) -> Option<Weekday> {
    match s {
        "mon" => Some(Weekday::Mon), "tue" => Some(Weekday::Tue),
        "wed" => Some(Weekday::Wed), "thu" => Some(Weekday::Thu),
        "fri" => Some(Weekday::Fri), "sat" => Some(Weekday::Sat),
        "sun" => Some(Weekday::Sun), _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let config = Config {
            claude_code_root: "~/.claude".to_string(),
            db_path: PathBuf::from("toki.fjall"),
            tz: None,
            retention_days: 0,
            rollup_retention_days: 0,
            daemon_sock: PathBuf::from("daemon.sock"),
            no_cost: false,
            output_format: "table".to_string(),
            start_of_week: Weekday::Mon,
        };
        assert_eq!(config.retention_days, 0);
        assert!(!config.no_cost);
        assert_eq!(config.output_format, "table");
        assert_eq!(config.start_of_week, Weekday::Mon);
    }

    #[test]
    fn test_config_builder() {
        let mut config = Config {
            claude_code_root: String::new(),
            db_path: PathBuf::new(),
            tz: None,
            retention_days: 0,
            rollup_retention_days: 0,
            daemon_sock: PathBuf::new(),
            no_cost: false,
            output_format: "table".to_string(),
            start_of_week: Weekday::Mon,
        };
        config = config.with_db_path("/custom/db.fjall".into());
        assert_eq!(config.db_path, PathBuf::from("/custom/db.fjall"));
    }

    #[test]
    fn test_settings_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        // Write
        let mut settings = HashMap::new();
        settings.insert("claude_code_root".to_string(), serde_json::Value::String("/test".to_string()));
        settings.insert("no_cost".to_string(), serde_json::Value::String("true".to_string()));
        let json = serde_json::to_string_pretty(&settings).unwrap();
        std::fs::write(&path, &json).unwrap();

        // Read back
        let data = std::fs::read_to_string(&path).unwrap();
        let loaded: HashMap<String, serde_json::Value> = serde_json::from_str(&data).unwrap();
        assert_eq!(loaded["claude_code_root"].as_str(), Some("/test"));
        assert_eq!(loaded["no_cost"].as_str(), Some("true"));
    }
}
