use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A single usage event extracted from a JSONL log line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEvent {
    pub event_key: String,
    pub source_file: String,
    pub model: String,
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
}

/// File checkpoint for incremental reading.
/// Uses line-length pre-filter + xxHash3-64 for fast reverse-scan matching.
/// No byte offset stored — immune to compaction shifting file contents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCheckpoint {
    pub file_path: String,
    pub last_line_len: u64,
    pub last_line_hash: u64,
}

/// Aggregated usage per model.
#[derive(Debug, Clone, Default)]
pub struct ModelUsageSummary {
    pub model: String,
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
    pub event_count: u64,
}

impl ModelUsageSummary {
    pub fn accumulate(&mut self, event: &UsageEvent) {
        self.input_tokens += event.input_tokens;
        self.cache_creation_input_tokens += event.cache_creation_input_tokens;
        self.cache_read_input_tokens += event.cache_read_input_tokens;
        self.output_tokens += event.output_tokens;
        self.event_count += 1;
    }
}

/// A session group: parent JSONL + subagent JSONLs.
#[derive(Debug, Clone)]
pub struct SessionGroup {
    pub session_id: String,
    pub parent_jsonl: PathBuf,
    pub subagent_jsonls: Vec<PathBuf>,
}

/// Trait that provider parsers must implement.
pub trait LogParser: Send + Sync {
    fn parse_line(&self, line: &str, source_file: &str) -> Option<UsageEvent>;
    fn file_patterns(&self, root_dir: &str) -> Vec<String>;
    fn discover_sessions(&self, root_dir: &str) -> Vec<SessionGroup>;
}

/// Webtrace error types.
#[derive(Debug)]
pub enum WebtraceError {
    Db(redb::Error),
    Io(std::io::Error),
    Watcher(notify::Error),
}

impl std::fmt::Display for WebtraceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebtraceError::Db(e) => write!(f, "database error: {}", e),
            WebtraceError::Io(e) => write!(f, "io error: {}", e),
            WebtraceError::Watcher(e) => write!(f, "watcher error: {}", e),
        }
    }
}

impl std::error::Error for WebtraceError {}

impl From<redb::Error> for WebtraceError {
    fn from(e: redb::Error) -> Self {
        WebtraceError::Db(e)
    }
}

impl From<std::io::Error> for WebtraceError {
    fn from(e: std::io::Error) -> Self {
        WebtraceError::Io(e)
    }
}

impl From<notify::Error> for WebtraceError {
    fn from(e: notify::Error) -> Self {
        WebtraceError::Watcher(e)
    }
}
