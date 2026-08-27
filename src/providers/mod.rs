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

    /// Directories to scan via periodic polling, in addition to the native file watcher.
    /// Returns Some for providers where FSEvents misses writes to long-lived open fds (e.g. Codex on macOS).
    /// Returns None (default) for providers whose native watcher already catches all writes.
    fn poll_dirs(&self) -> Option<Vec<String>> {
        None
    }

    /// Whether this provider owns a given file path.
    fn owns_path(&self, path: &str) -> bool;

    /// Discover session groups for cold start.
    fn discover_sessions(&self) -> Vec<SessionGroup>;

    /// Create a per-file stateful parser positioned at `offset`.
    ///
    /// The offset is part of the contract, not a convenience: a stateful
    /// parser resuming mid-file has missed whatever the earlier bytes
    /// established (Codex tracks the model that way), and a parser built
    /// without knowing where it starts cannot recover it. Providers whose
    /// parse carries no state may ignore both arguments.
    fn create_file_parser(&self, path: &str, offset: u64) -> Box<dyn FileParser>;

    /// Scan a single file for cold start, calling `emit` for each parsed event.
    /// Returns checkpoint data (bytes_consumed, last_line_len, last_line_hash) if lines were processed.
    /// Default implementation uses `create_file_parser()` (dyn dispatch).
    /// Providers can override this with a concrete type for inlining.
    fn scan_file_cold_start(
        &self,
        path: &str,
        offset: u64,
        emit: &mut dyn FnMut(ColdStartParsed),
    ) -> std::io::Result<Option<(u64, u64, u64)>> {
        let mut parser = self.create_file_parser(path, offset);
        crate::checkpoint::process_lines_streaming(path, offset, |line| {
            if let Some(parsed) = parser.parse_line(line) {
                emit(parsed);
            }
        })
    }

    /// The LogParser implementation for watch mode.
    fn parser(&self) -> &dyn LogParser;

    /// The LogParserWithTs implementation for watch mode.
    fn parser_with_ts(&self) -> &dyn LogParserWithTs;

    /// Extract session ID from a file path.
    fn extract_session_id(&self, path: &str) -> Option<String>;

    /// Extract project name from a file path (zero-alloc where possible).
    fn extract_project_name<'a>(&self, path: &'a str) -> Option<&'a str>;

    /// Resolve the project name for a file, allowing providers to use parser
    /// state (e.g. a cwd discovered from session_meta inside the file) rather
    /// than only the file path. Used by the live/watch write path.
    ///
    /// Default: the path-based `extract_project_name`. Codex overrides this
    /// because its cwd lives in the file content, not the path (issue #11, Bug 2).
    fn resolve_project_name(&self, path: &str) -> Option<String> {
        self.extract_project_name(path).map(|s| s.to_string())
    }

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
/// Uses TokenFields directly — no provider-specific enum overhead.
/// Size: ~88 bytes (same as v1.0.0 ColdStartParsed + project_name).
pub struct ColdStartParsed {
    pub event_key: String,
    pub model: String,
    pub ts_ms: i64,
    pub tokens: TokenFields,
    /// Project name discovered during parsing (e.g., from Codex session_meta cwd).
    /// None for providers that extract project_name from the file path instead.
    pub project_name: Option<String>,
}

impl ColdStartParsed {
    /// Accumulate into summary and consume self to build ColdStartEvent (zero clone).
    pub fn into_summary_and_event(
        self,
        summary: &mut ModelUsageSummary,
        session_id: Arc<str>,
        source_file: Arc<str>,
        project_name: Option<Arc<str>>,
    ) -> ColdStartEvent {
        summary.input_tokens = summary
            .input_tokens
            .saturating_add(self.tokens.input_tokens);
        summary.output_tokens = summary
            .output_tokens
            .saturating_add(self.tokens.output_tokens);
        summary.cache_creation_input_tokens = summary
            .cache_creation_input_tokens
            .saturating_add(self.tokens.cache_creation_input_tokens);
        summary.cache_read_input_tokens = summary
            .cache_read_input_tokens
            .saturating_add(self.tokens.cache_read_input_tokens);
        summary.event_count = summary.event_count.saturating_add(1);

        ColdStartEvent {
            ts_ms: self.ts_ms,
            message_id: self.event_key,
            model: self.model,
            session_id,
            source_file,
            project_name,
            tokens: self.tokens,
        }
    }
}

/// Create provider instances from a list of names.
pub fn create_providers(names: &[String], config: &crate::Config) -> Vec<Box<dyn Provider>> {
    names
        .iter()
        .filter_map(|name| match name.as_str() {
            "claude_code" => Some(Box::new(claude_code::ClaudeCodeProvider::new(
                config.claude_code_root.clone(),
            )) as Box<dyn Provider>),
            "codex" => Some(
                Box::new(codex::CodexProvider::new(config.codex_root.clone())) as Box<dyn Provider>,
            ),
            _ => {
                eprintln!("[toki] Unknown provider: {}", name);
                None
            }
        })
        .collect()
}
