use std::path::PathBuf;

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
}

impl Config {
    pub fn new() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));

        Config {
            claude_code_root: home.join(".claude").to_string_lossy().to_string(),
            db_path: home.join(".config").join("clitrace").join("clitrace.db"),
            full_rescan: false,
            session_filter: None,
            project_filter: None,
            tz: None,
        }
    }

    pub fn with_claude_root(mut self, root: String) -> Self {
        self.claude_code_root = root;
        self
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
    /// Priority: env var > DB > default (env already applied before this call).
    pub fn load_from_db(&mut self, db: &Database) {
        if std::env::var("CLITRACE_CLAUDE_ROOT").is_err() {
            if let Ok(Some(root)) = db.get_setting("claude_code_root") {
                self.claude_code_root = root;
            }
        }
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
        assert!(config.db_path.ends_with("clitrace.db"));
    }

    #[test]
    fn test_config_builder() {
        let config = Config::new()
            .with_claude_root("/custom/root".to_string())
            .with_db_path("/custom/db.redb".into())
            .with_full_rescan(true);

        assert_eq!(config.claude_code_root, "/custom/root");
        assert_eq!(config.db_path, PathBuf::from("/custom/db.redb"));
        assert!(config.full_rescan);
    }

    #[test]
    fn test_config_db_override() {
        let _guard = env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();

        db.set_setting("claude_code_root", "/from/db").unwrap();
        // Remove env var to allow DB override
        std::env::remove_var("CLITRACE_CLAUDE_ROOT");

        let mut config = Config::new();
        config.load_from_db(&db);

        assert_eq!(config.claude_code_root, "/from/db");
    }

    #[test]
    fn test_config_env_overrides_db() {
        let _guard = env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();

        db.set_setting("claude_code_root", "/from/db").unwrap();

        std::env::set_var("CLITRACE_CLAUDE_ROOT", "/from/env");

        let mut config = Config::new()
            .with_claude_root("/from/env".to_string());
        config.load_from_db(&db);

        assert_eq!(config.claude_code_root, "/from/env");

        std::env::remove_var("CLITRACE_CLAUDE_ROOT");
    }
}
