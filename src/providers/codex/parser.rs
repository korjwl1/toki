use std::collections::HashMap;
use std::sync::Mutex;

use serde::Deserialize;

use crate::common::types::{LogParser, LogParserWithTs, SessionGroup, UsageEvent, UsageEventWithTs};
use crate::providers::{ColdStartParsed, FileParser};

/// Stateful per-file parser for Codex CLI cold start.
/// Tracks model name across turn_context -> token_count events.
pub struct CodexFileParser {
    last_model: String,
    session_id: Option<String>,
    cwd: Option<String>,
}

impl CodexFileParser {
    pub fn new() -> Self {
        CodexFileParser {
            last_model: "unknown".to_string(),
            session_id: None,
            cwd: None,
        }
    }

    /// Get the project name (cwd) discovered from session_meta.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }
}

impl FileParser for CodexFileParser {
    fn parse_line(&mut self, line: &str) -> Option<ColdStartParsed> {
        // Pre-filter: only process lines that contain relevant keywords
        if !line.contains("\"token_count\"")
            && !line.contains("\"turn_context\"")
            && !line.contains("\"session_meta\"")
        {
            return None;
        }

        // First pass: extract only type and timestamp (zero-copy, no heap alloc for payload)
        let header: CodexLineHeader = serde_json::from_str(line).ok()?;

        match header.line_type {
            "session_meta" => {
                // Second pass: deserialize with targeted session_meta struct
                let parsed: CodexSessionMetaLine = serde_json::from_str(line).ok()?;
                if let Some(payload) = &parsed.payload {
                    if let Some(id) = payload.id {
                        self.session_id = Some(id.to_string());
                    }
                    if let Some(cwd) = payload.cwd {
                        self.cwd = Some(cwd.to_string());
                    }
                }
                None
            }
            "turn_context" => {
                // Second pass: deserialize with targeted turn_context struct
                let parsed: CodexTurnContextLine = serde_json::from_str(line).ok()?;
                if let Some(payload) = &parsed.payload {
                    if let Some(model) = payload.model {
                        self.last_model = model.to_string();
                    }
                }
                None
            }
            "event_msg" => {
                // Second pass: deserialize with targeted event_msg struct
                let parsed: CodexEventMsgLine = serde_json::from_str(line).ok()?;
                let payload = parsed.payload?;
                if payload.payload_type.as_deref() != Some("token_count") {
                    return None;
                }

                // "info" can be null for the first token_count event
                let info = payload.info?;
                let last_usage = info.last_token_usage?;

                let input_tokens = last_usage.input_tokens?;
                let output_tokens = last_usage.output_tokens.unwrap_or(0);
                let cached_input_tokens = last_usage.cached_input_tokens.unwrap_or(0);
                let reasoning_output_tokens = last_usage.reasoning_output_tokens.unwrap_or(0);

                let ts = header.timestamp.unwrap_or("");
                let ts_ms = crate::common::time::parse_ts_to_ms(ts)?;

                // Build a unique event key from session_id (or timestamp) and timestamp
                let session_part = self
                    .session_id
                    .as_deref()
                    .unwrap_or("unknown");
                let event_key = format!("{}:{}", session_part, ts);

                Some(ColdStartParsed {
                    event_key,
                    model: self.last_model.clone(),
                    ts_ms,
                    // Map Codex fields to common TokenFields:
                    // slot 3 (cache_creation_input_tokens) = reasoning_output_tokens
                    // slot 4 (cache_read_input_tokens) = cached_input_tokens
                    tokens: crate::common::types::TokenFields {
                        input_tokens,
                        output_tokens,
                        cache_creation_input_tokens: reasoning_output_tokens,
                        cache_read_input_tokens: cached_input_tokens,
                    },
                    project_name: self.cwd.clone(),
                })
            }
            _ => None,
        }
    }
}

/// Minimal header struct for first-pass deserialization.
/// Only extracts `type` and `timestamp` — no heap allocation for payload.
#[derive(Deserialize)]
struct CodexLineHeader<'a> {
    #[serde(rename = "type")]
    line_type: &'a str,
    timestamp: Option<&'a str>,
}

/// Targeted deserialization for session_meta lines.
#[derive(Deserialize)]
struct CodexSessionMetaLine<'a> {
    #[allow(dead_code)]
    #[serde(rename = "type")]
    line_type: &'a str,
    #[allow(dead_code)]
    timestamp: Option<&'a str>,
    payload: Option<SessionMetaPayload<'a>>,
}

#[derive(Deserialize)]
struct SessionMetaPayload<'a> {
    id: Option<&'a str>,
    cwd: Option<&'a str>,
}

/// Targeted deserialization for turn_context lines.
#[derive(Deserialize)]
struct CodexTurnContextLine<'a> {
    #[allow(dead_code)]
    #[serde(rename = "type")]
    line_type: &'a str,
    #[allow(dead_code)]
    timestamp: Option<&'a str>,
    payload: Option<TurnContextPayload<'a>>,
}

#[derive(Deserialize)]
struct TurnContextPayload<'a> {
    model: Option<&'a str>,
}

/// Targeted deserialization for event_msg (token_count) lines.
/// Note: timestamp is read from the CodexLineHeader first pass, not here.
#[derive(Deserialize)]
struct CodexEventMsgLine {
    payload: Option<EventMsgPayload>,
}

#[derive(Deserialize)]
struct EventMsgPayload {
    #[serde(rename = "type")]
    payload_type: Option<String>,
    #[serde(default)]
    info: Option<TokenCountInfo>,
}

#[derive(Deserialize)]
struct TokenCountInfo {
    last_token_usage: Option<LastTokenUsage>,
}

#[derive(Deserialize)]
struct LastTokenUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
}

/// Shared (stateful) parser for Codex watch mode.
/// Uses a Mutex to track per-file model state.
///
/// Note: session_meta lines are intentionally skipped in watch mode.
/// The session ID is derived from the filename (UUID extraction), not from
/// session_meta payload, because the engine's process_and_print_provider
/// calls provider.extract_session_id(path) for session identification.
pub struct CodexParser {
    /// Per-file model tracking: file_path -> last_model
    file_models: Mutex<HashMap<String, String>>,
}

impl CodexParser {
    pub fn new() -> Self {
        CodexParser {
            file_models: Mutex::new(HashMap::new()),
        }
    }

    fn get_model(&self, source_file: &str) -> String {
        self.file_models
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(source_file)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string())
    }

    fn set_model(&self, source_file: &str, model: &str) {
        let mut map = self.file_models
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Cap size to prevent unbounded growth. Evict roughly half of entries
        // (arbitrary iteration order, but preserves some model tracking rather than
        // losing all of it via clear()).
        if map.len() > 500 {
            let keys_to_remove: Vec<String> = map.keys().take(250).cloned().collect();
            for key in keys_to_remove {
                map.remove(&key);
            }
        }
        map.insert(source_file.to_string(), model.to_string());
    }
}

impl LogParser for CodexParser {
    fn parse_line(&self, line: &str, source_file: &str) -> Option<UsageEvent> {
        // Pre-filter
        if !line.contains("\"token_count\"") && !line.contains("\"turn_context\"") {
            return None;
        }

        // First pass: extract type and timestamp only (no payload heap alloc)
        let header: CodexLineHeader = serde_json::from_str(line).ok()?;

        match header.line_type {
            "turn_context" => {
                // Second pass: targeted turn_context deserialization
                let parsed: CodexTurnContextLine = serde_json::from_str(line).ok()?;
                if let Some(payload) = &parsed.payload {
                    if let Some(model) = payload.model {
                        self.set_model(source_file, model);
                    }
                }
                None
            }
            "event_msg" => {
                // Second pass: targeted event_msg deserialization
                let parsed: CodexEventMsgLine = serde_json::from_str(line).ok()?;
                let payload = parsed.payload?;
                if payload.payload_type.as_deref() != Some("token_count") {
                    return None;
                }

                let info = payload.info?;
                let last_usage = info.last_token_usage?;

                let input_tokens = last_usage.input_tokens?;
                let output_tokens = last_usage.output_tokens.unwrap_or(0);
                let cached_input_tokens = last_usage.cached_input_tokens.unwrap_or(0);
                let reasoning_output_tokens = last_usage.reasoning_output_tokens.unwrap_or(0);

                let ts = header.timestamp.unwrap_or("");
                let model = self.get_model(source_file);
                let event_key = format!("codex:{}:{}", source_file, ts);

                Some(UsageEvent {
                    event_key,
                    source_file: source_file.to_string(),
                    model,
                    input_tokens,
                    output_tokens,
                    // slot 3 = reasoning_output_tokens, slot 4 = cached_input_tokens
                    cache_creation_input_tokens: reasoning_output_tokens,
                    cache_read_input_tokens: cached_input_tokens,
                })
            }
            _ => None,
        }
    }

    fn file_patterns(&self, root_dir: &str) -> Vec<String> {
        vec![format!("{}/sessions/**/*.jsonl", root_dir)]
    }

    fn discover_sessions(&self, root_dir: &str) -> Vec<SessionGroup> {
        let pattern = format!("{}/sessions/**/*.jsonl", root_dir);
        let mut sessions = Vec::new();

        let jsonl_files: Vec<std::path::PathBuf> = glob::glob(&pattern)
            .into_iter()
            .flatten()
            .filter_map(|p| p.ok())
            .collect();

        for path in jsonl_files {
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };

            let session_id = super::extract_uuid_from_filename(stem)
                .unwrap_or_else(|| stem.to_string());

            sessions.push(SessionGroup {
                session_id,
                parent_jsonl: path,
                subagent_jsonls: vec![],
            });
        }

        sessions
    }
}

impl LogParserWithTs for CodexParser {
    fn parse_line_with_ts(&self, line: &str, source_file: &str) -> Option<UsageEventWithTs> {
        // Pre-filter
        if !line.contains("\"token_count\"") && !line.contains("\"turn_context\"") {
            return None;
        }

        // First pass: extract type and timestamp only (no payload heap alloc)
        let header: CodexLineHeader = serde_json::from_str(line).ok()?;

        match header.line_type {
            "turn_context" => {
                // Second pass: targeted turn_context deserialization
                let parsed: CodexTurnContextLine = serde_json::from_str(line).ok()?;
                if let Some(payload) = &parsed.payload {
                    if let Some(model) = payload.model {
                        self.set_model(source_file, model);
                    }
                }
                None
            }
            "event_msg" => {
                // Second pass: targeted event_msg deserialization
                let parsed: CodexEventMsgLine = serde_json::from_str(line).ok()?;
                let payload = parsed.payload?;
                if payload.payload_type.as_deref() != Some("token_count") {
                    return None;
                }

                let info = payload.info?;
                let last_usage = info.last_token_usage?;

                let input_tokens = last_usage.input_tokens?;
                let output_tokens = last_usage.output_tokens.unwrap_or(0);
                let cached_input_tokens = last_usage.cached_input_tokens.unwrap_or(0);
                let reasoning_output_tokens = last_usage.reasoning_output_tokens.unwrap_or(0);

                let ts = header.timestamp.unwrap_or_default().to_string();
                let model = self.get_model(source_file);
                let event_key = format!("codex:{}:{}", source_file, &ts);

                Some(UsageEventWithTs {
                    event_key,
                    source_file: source_file.to_string(),
                    model,
                    input_tokens,
                    output_tokens,
                    // slot 3 = reasoning_output_tokens, slot 4 = cached_input_tokens
                    cache_creation_input_tokens: reasoning_output_tokens,
                    cache_read_input_tokens: cached_input_tokens,
                    timestamp: ts,
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_session_meta() {
        let mut parser = CodexFileParser::new();
        let line = r#"{"timestamp":"2026-03-11T15:35:35.678Z","type":"session_meta","payload":{"id":"019cdd89-9fd9-7f11-b555-459c0ec30834","cwd":"/Users/test/project"}}"#;
        let result = parser.parse_line(line);
        assert!(result.is_none());
        assert_eq!(parser.session_id.as_deref(), Some("019cdd89-9fd9-7f11-b555-459c0ec30834"));
        assert_eq!(parser.cwd.as_deref(), Some("/Users/test/project"));
    }

    #[test]
    fn test_parse_turn_context() {
        let mut parser = CodexFileParser::new();
        let line = r#"{"timestamp":"2026-03-11T15:35:35.680Z","type":"turn_context","payload":{"model":"gpt-5.4","turn_id":"xxx"}}"#;
        let result = parser.parse_line(line);
        assert!(result.is_none());
        assert_eq!(parser.last_model, "gpt-5.4");
    }

    #[test]
    fn test_parse_token_count() {
        let mut parser = CodexFileParser::new();
        parser.last_model = "gpt-5.4".to_string();
        parser.session_id = Some("test-session".to_string());

        let line = r#"{"timestamp":"2026-03-11T15:36:16.626Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":15262,"cached_input_tokens":15104,"output_tokens":82,"reasoning_output_tokens":0,"total_tokens":15344},"total_token_usage":{"input_tokens":30395,"cached_input_tokens":24192,"output_tokens":165,"reasoning_output_tokens":0,"total_tokens":30560},"model_context_window":258400}}}"#;
        let result = parser.parse_line(line).unwrap();
        assert_eq!(result.model, "gpt-5.4");
        assert!(result.ts_ms > 0);

        assert_eq!(result.tokens.input_tokens, 15262);
        assert_eq!(result.tokens.output_tokens, 82);
        assert_eq!(result.tokens.cache_read_input_tokens, 15104);
        assert_eq!(result.tokens.cache_creation_input_tokens, 0);
    }

    #[test]
    fn test_parse_token_count_null_info() {
        let mut parser = CodexFileParser::new();
        let line = r#"{"timestamp":"2026-03-11T15:35:36.000Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#;
        let result = parser.parse_line(line);
        assert!(result.is_none()); // null info should be handled gracefully
    }

    #[test]
    fn test_skip_irrelevant_lines() {
        let mut parser = CodexFileParser::new();
        let line = r#"{"timestamp":"2026-03-11T15:35:36.000Z","type":"input_text","payload":{"text":"hello"}}"#;
        assert!(parser.parse_line(line).is_none());
    }

    #[test]
    fn test_watch_mode_parser() {
        let parser = CodexParser::new();

        // First: turn_context sets model
        let ctx_line = r#"{"timestamp":"2026-03-11T15:35:35.680Z","type":"turn_context","payload":{"model":"gpt-5.4","turn_id":"xxx"}}"#;
        assert!(parser.parse_line(ctx_line, "/test/session.jsonl").is_none());

        // Then: token_count uses the model
        let tc_line = r#"{"timestamp":"2026-03-11T15:36:16.626Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":50,"output_tokens":20,"reasoning_output_tokens":0,"total_tokens":170},"total_token_usage":{"input_tokens":200,"cached_input_tokens":100,"output_tokens":40,"reasoning_output_tokens":0,"total_tokens":340},"model_context_window":258400}}}"#;
        let event = parser.parse_line(tc_line, "/test/session.jsonl").unwrap();
        assert_eq!(event.model, "gpt-5.4");
        assert_eq!(event.input_tokens, 100);
        assert_eq!(event.output_tokens, 20);
        assert_eq!(event.cache_read_input_tokens, 50);
    }
}
