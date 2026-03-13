use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

/// A single usage event with timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEventWithTs {
    pub event_key: String,
    pub source_file: String,
    pub model: String,
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
    pub timestamp: String,
}

impl UsageEventWithTs {
    /// Convert to UsageEvent, consuming self to avoid cloning strings.
    pub fn into_usage_event(self) -> (UsageEvent, String) {
        let ts = self.timestamp;
        let event = UsageEvent {
            event_key: self.event_key,
            source_file: self.source_file,
            model: self.model,
            input_tokens: self.input_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
            output_tokens: self.output_tokens,
        };
        (event, ts)
    }
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
        self.input_tokens = self.input_tokens.saturating_add(event.input_tokens);
        self.cache_creation_input_tokens = self.cache_creation_input_tokens.saturating_add(event.cache_creation_input_tokens);
        self.cache_read_input_tokens = self.cache_read_input_tokens.saturating_add(event.cache_read_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(event.output_tokens);
        self.event_count = self.event_count.saturating_add(1);
    }

    pub fn accumulate_with_ts(&mut self, event: &UsageEventWithTs) {
        self.input_tokens = self.input_tokens.saturating_add(event.input_tokens);
        self.cache_creation_input_tokens = self.cache_creation_input_tokens.saturating_add(event.cache_creation_input_tokens);
        self.cache_read_input_tokens = self.cache_read_input_tokens.saturating_add(event.cache_read_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(event.output_tokens);
        self.event_count = self.event_count.saturating_add(1);
    }
}

/// A stored event in the TSDB (events keyspace).
/// Uses dictionary-compressed IDs for repeated strings (model, session, source_file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvent {
    pub model_id: u32,
    pub session_id: u32,
    pub source_file_id: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

/// Hourly rollup aggregation per model (rollups keyspace).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RollupValue {
    pub input: u64,
    pub output: u64,
    pub cache_create: u64,
    pub cache_read: u64,
    pub count: u64,
}

/// Token fields for DbOp::WriteEvent.
#[derive(Debug, Clone)]
pub struct TokenFields {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

/// Result from TSDB queries.
pub type SummaryMap = HashMap<String, ModelUsageSummary>;
pub type GroupedSummaryMap = HashMap<String, HashMap<String, ModelUsageSummary>>;

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

/// Optional extension for parsers that can extract timestamps.
pub trait LogParserWithTs: Send + Sync {
    fn parse_line_with_ts(&self, line: &str, source_file: &str) -> Option<UsageEventWithTs>;
}

/// Toki error types.
#[derive(Debug)]
pub enum TokiError {
    Db(fjall::Error),
    Io(std::io::Error),
    Watcher(notify::Error),
}

impl std::fmt::Display for TokiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokiError::Db(e) => write!(f, "database error: {}", e),
            TokiError::Io(e) => write!(f, "io error: {}", e),
            TokiError::Watcher(e) => write!(f, "watcher error: {}", e),
        }
    }
}

impl std::error::Error for TokiError {}

impl From<fjall::Error> for TokiError {
    fn from(e: fjall::Error) -> Self {
        TokiError::Db(e)
    }
}

impl From<std::io::Error> for TokiError {
    fn from(e: std::io::Error) -> Self {
        TokiError::Io(e)
    }
}

impl From<notify::Error> for TokiError {
    fn from(e: notify::Error) -> Self {
        TokiError::Watcher(e)
    }
}
