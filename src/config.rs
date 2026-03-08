use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub claude_code_root: String,
    pub db_path: PathBuf,
    pub poll_interval_secs: u64,
    pub flush_interval_secs: u64,
}

impl Config {
    pub fn new() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));

        Config {
            claude_code_root: home.join(".claude").to_string_lossy().to_string(),
            db_path: home.join(".config").join("webtrace").join("webtrace.db"),
            poll_interval_secs: 30,
            flush_interval_secs: 10,
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
}
