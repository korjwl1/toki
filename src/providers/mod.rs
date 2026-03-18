pub mod claude_code;
pub mod codex;

use std::sync::Arc;

use crate::common::types::{
    LogParser, LogParserWithTs, ModelUsageSummary, SessionGroup, TokenFields,
};
use crate::writer::ColdStartEvent;

/// Known provider identifiers.
pub const KNOWN_PROVIDERS: &[&str] = &["claude_code", "codex"];

/// Trait that all providers must implement.
pub trait Provider: Send + Sync {
    /// Unique identifier (e.g., "claude_code", "codex").
    fn name(&self) -> &str;

    /// Human-readable display name (e.g., "Claude Code", "Codex CLI").
    fn display_name(&self) -> &str;

    /// Root directory for this provider's data.
    /// Returns None if the directory does not exist.
    fn root_dir(&self) -> Option<String>;

    /// Directories to register with the file watcher.
    fn watch_dirs(&self) -> Vec<String>;

    /// Whether this provider owns a given file path.
    fn owns_path(&self, path: &str) -> bool;

    /// Discover session groups for cold start.
    fn discover_sessions(&self) -> Vec<SessionGroup>;

    /// Create a per-file stateful parser for cold start.
    /// Each file gets its own instance (supports stateful parsing like Codex model tracking).
    fn create_file_parser(&self) -> Box<dyn FileParser>;

    /// The LogParser implementation for watch mode.
    fn parser(&self) -> &dyn LogParser;

    /// The LogParserWithTs implementation for watch mode.
    fn parser_with_ts(&self) -> &dyn LogParserWithTs;

    /// Extract session ID from a file path.
    fn extract_session_id(&self, path: &str) -> Option<String>;

    /// Extract project name from a file path (zero-alloc where possible).
    fn extract_project_name<'a>(&self, path: &'a str) -> Option<&'a str>;

    /// DB directory name for this provider (e.g., "claude_code.fjall").
    fn db_dir_name(&self) -> &str;
}

/// Per-file stateful parser for cold start.
/// Created fresh for each file via Provider::create_file_parser().
/// Lines within a file are processed sequentially, so &mut self is safe.
pub trait FileParser: Send {
    /// Parse a single line during cold start.
    /// Returns Some with parsed data, or None if the line is not a token usage event.
    fn parse_line(&mut self, line: &str) -> Option<ColdStartParsed>;
}

/// Parsed cold-start data common to all providers.
pub struct ColdStartParsed {
    pub event_key: String,
    pub model: String,
    pub ts_ms: i64,
    /// Provider-specific token data.
    pub provider_data: ProviderTokenData,
    /// For display-level summary accumulation.
    pub total_input: u64,
    pub total_output: u64,
    /// Project name discovered during parsing (e.g., from Codex session_meta cwd).
    /// None for providers that extract project_name from the file path instead.
    pub project_name: Option<String>,
}

/// Provider-specific token fields, stored as an enum to avoid trait objects.
pub enum ProviderTokenData {
    ClaudeCode(ClaudeTokenFields),
    Codex(CodexTokenFields),
}

/// Claude Code token fields.
#[derive(Debug, Clone)]
pub struct ClaudeTokenFields {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

/// Codex CLI token fields.
#[derive(Debug, Clone)]
pub struct CodexTokenFields {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_output_tokens: u64,
}

impl ColdStartParsed {
    /// Convert provider-specific token data to the common TokenFields used by the DB schema.
    /// For Codex: cached_input_tokens -> cache_read_input_tokens, reasoning tokens dropped.
    pub fn to_token_fields(&self) -> TokenFields {
        match &self.provider_data {
            ProviderTokenData::ClaudeCode(f) => TokenFields {
                input_tokens: f.input_tokens,
                output_tokens: f.output_tokens,
                cache_creation_input_tokens: f.cache_creation_input_tokens,
                cache_read_input_tokens: f.cache_read_input_tokens,
            },
            ProviderTokenData::Codex(f) => TokenFields {
                input_tokens: f.input_tokens,
                output_tokens: f.output_tokens,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: f.cached_input_tokens,
            },
        }
    }

    /// Accumulate into summary and consume self to build ColdStartEvent (zero clone).
    pub fn into_summary_and_event(
        self,
        summary: &mut ModelUsageSummary,
        session_id: Arc<str>,
        source_file: Arc<str>,
        project_name: Option<Arc<str>>,
    ) -> ColdStartEvent {
        let tokens = self.to_token_fields();
        summary.input_tokens += tokens.input_tokens;
        summary.output_tokens += tokens.output_tokens;
        summary.cache_creation_input_tokens += tokens.cache_creation_input_tokens;
        summary.cache_read_input_tokens += tokens.cache_read_input_tokens;
        summary.event_count += 1;

        ColdStartEvent {
            ts_ms: self.ts_ms,
            message_id: self.event_key,   // move, no clone
            model: self.model,            // move, no clone
            session_id,
            source_file,
            project_name,
            tokens,                       // reuse, no second to_token_fields()
        }
    }
}

/// Create provider instances from a list of names.
pub fn create_providers(names: &[String], config: &crate::Config) -> Vec<Box<dyn Provider>> {
    names
        .iter()
        .filter_map(|name| match name.as_str() {
            "claude_code" => Some(
                Box::new(claude_code::ClaudeCodeProvider::new(
                    config.claude_code_root.clone(),
                )) as Box<dyn Provider>,
            ),
            "codex" => Some(Box::new(codex::CodexProvider::new()) as Box<dyn Provider>),
            _ => {
                eprintln!("[toki] Unknown provider: {}", name);
                None
            }
        })
        .collect()
}
