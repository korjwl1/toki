use std::path::PathBuf;

use chrono::Weekday;
use chrono_tz::Tz;
use crate::db::Database;

#[derive(Debug, Clone)]
pub struct Config {
    pub claude_code_root: String,
    pub db_path: PathBuf,
    pub full_rescan: bool,
    pub session_filter: Option<String>,
    pub project_filter: Option<String>,
    pub tz: Option<Tz>,
    pub retention_days: u32,
    pub rollup_retention_days: u32,
    pub daemon_sock: PathBuf,
    pub no_cost: bool,
    pub output_format: String,
    pub start_of_week: Weekday,
}

impl Config {
    pub fn new() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));

        Config {
            claude_code_root: home.join(".claude").to_string_lossy().to_string(),
            db_path: home.join(".config").join("clitrace").join("clitrace.fjall"),
            full_rescan: false,
            session_filter: None,
            project_filter: None,
            tz: None,
            retention_days: 0,
            rollup_retention_days: 0,
            daemon_sock: crate::daemon::default_sock_path(),
            no_cost: false,
            output_format: "table".to_string(),
            start_of_week: Weekday::Mon,
        }
    }

    pub fn with_db_path(mut self, path: PathBuf) -> Self {
        self.db_path = path;
        self
    }

    pub fn with_full_rescan(mut self, enabled: bool) -> Self {
        self.full_rescan = enabled;
        self
    }

    pub fn with_session_filter(mut self, filter: Option<String>) -> Self {
        self.session_filter = filter;
        self
    }

    pub fn with_project_filter(mut self, filter: Option<String>) -> Self {
        self.project_filter = filter;
        self
    }

    pub fn with_tz(mut self, tz: Option<Tz>) -> Self {
        self.tz = tz;
        self
    }

    /// Load overrides from DB settings table.
    /// Priority: CLI arg > DB setting > default.
    pub fn load_from_db(&mut self, db: &Database) {
        if let Ok(Some(v)) = db.get_setting("claude_code_root") {
            self.claude_code_root = v;
        }
        if let Ok(Some(v)) = db.get_setting("retention_days") {
            if let Ok(n) = v.parse::<u32>() { self.retention_days = n; }
        }
        if let Ok(Some(v)) = db.get_setting("rollup_retention_days") {
            if let Ok(n) = v.parse::<u32>() { self.rollup_retention_days = n; }
        }
        if let Ok(Some(v)) = db.get_setting("daemon_sock") {
            self.daemon_sock = PathBuf::from(v);
        }
        if let Ok(Some(v)) = db.get_setting("timezone") {
            if !v.is_empty() {
                if let Ok(tz) = v.parse::<Tz>() {
                    self.tz = Some(tz);
                }
            }
        }
        if let Ok(Some(v)) = db.get_setting("no_cost") {
            self.no_cost = v == "true";
        }
        if let Ok(Some(v)) = db.get_setting("output_format") {
            if v == "table" || v == "json" {
                self.output_format = v;
            }
        }
        if let Ok(Some(v)) = db.get_setting("start_of_week") {
            if let Some(w) = parse_weekday(&v) {
                self.start_of_week = w;
            }
        }
    }
}

fn parse_weekday(s: &str) -> Option<Weekday> {
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
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn test_config_defaults() {
        let config = Config::new();
        assert!(config.claude_code_root.ends_with(".claude"));
        assert!(config.db_path.ends_with("clitrace.fjall"));
        assert_eq!(config.retention_days, 0);
        assert_eq!(config.rollup_retention_days, 0);
        assert!(!config.no_cost);
        assert_eq!(config.output_format, "table");
        assert_eq!(config.start_of_week, Weekday::Mon);
    }

    #[test]
    fn test_config_builder() {
        let config = Config::new()
            .with_db_path("/custom/db.fjall".into())
            .with_full_rescan(true);

        assert_eq!(config.db_path, PathBuf::from("/custom/db.fjall"));
        assert!(config.full_rescan);
    }

    #[test]
    fn test_config_db_override() {
        let _guard = env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();

        db.set_setting("claude_code_root", "/from/db").unwrap();
        db.set_setting("retention_days", "30").unwrap();
        db.set_setting("no_cost", "true").unwrap();
        db.set_setting("output_format", "json").unwrap();
        db.set_setting("start_of_week", "fri").unwrap();

        let mut config = Config::new();
        config.load_from_db(&db);

        assert_eq!(config.claude_code_root, "/from/db");
        assert_eq!(config.retention_days, 30);
        assert!(config.no_cost);
        assert_eq!(config.output_format, "json");
        assert_eq!(config.start_of_week, Weekday::Fri);
    }
}
