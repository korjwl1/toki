use std::collections::HashMap;
use std::sync::Mutex;

use serde::Deserialize;

use crate::common::types::{LogParser, LogParserWithTs, SessionGroup, UsageEvent, UsageEventWithTs};
use crate::providers::{CodexTokenFields, ColdStartParsed, FileParser, ProviderTokenData};

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

        let parsed: CodexLine = serde_json::from_str(line).ok()?;

        match parsed.line_type {
            "session_meta" => {
                if let Some(payload) = &parsed.payload {
                    if let Some(id) = payload.get("id").and_then(|v| v.as_str()) {
                        self.session_id = Some(id.to_string());
                    }
                    if let Some(cwd) = payload.get("cwd").and_then(|v| v.as_str()) {
                        self.cwd = Some(cwd.to_string());
                    }
                }
                None
            }
            "turn_context" => {
                if let Some(payload) = &parsed.payload {
                    if let Some(model) = payload.get("model").and_then(|v| v.as_str()) {
                        self.last_model = model.to_string();
                    }
                }
                None
            }
            "event_msg" => {
                let payload = parsed.payload.as_ref()?;
                let payload_type = payload.get("type")?.as_str()?;
                if payload_type != "token_count" {
                    return None;
                }

                // "info" can be null for the first token_count event
                let info = match payload.get("info") {
                    Some(serde_json::Value::Object(obj)) => obj,
                    _ => return None,
                };

                let last_usage = info.get("last_token_usage")?;
                let input_tokens = last_usage.get("input_tokens")?.as_u64()?;
                let output_tokens = last_usage.get("output_tokens")?.as_u64().unwrap_or(0);
                let cached_input_tokens = last_usage
                    .get("cached_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let reasoning_output_tokens = last_usage
                    .get("reasoning_output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                let ts = parsed.timestamp.unwrap_or("");
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
                    total_input: input_tokens + cached_input_tokens,
                    total_output: output_tokens + reasoning_output_tokens,
                    provider_data: ProviderTokenData::Codex(CodexTokenFields {
                        input_tokens,
                        output_tokens,
                        cached_input_tokens,
                        reasoning_output_tokens,
                    }),
                    project_name: self.cwd.clone(),
                })
            }
            _ => None,
        }
    }
}

/// Minimal deserialization struct for Codex JSONL lines.
/// Uses lifetime parameter with `&'a str` for zero-copy deserialization
/// of `line_type` and `timestamp`, similar to Claude Code's `JsonlLine<'a>`.
#[derive(Deserialize)]
struct CodexLine<'a> {
    #[serde(rename = "type")]
    line_type: &'a str,
    timestamp: Option<&'a str>,
    #[serde(default)]
    payload: Option<serde_json::Value>,
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

        let parsed: CodexLine = serde_json::from_str(line).ok()?;

        match parsed.line_type {
            "turn_context" => {
                if let Some(payload) = &parsed.payload {
                    if let Some(model) = payload.get("model").and_then(|v| v.as_str()) {
                        self.set_model(source_file, model);
                    }
                }
                None
            }
            "event_msg" => {
                let payload = parsed.payload.as_ref()?;
                if payload.get("type")?.as_str()? != "token_count" {
                    return None;
                }

                let info = match payload.get("info") {
                    Some(serde_json::Value::Object(obj)) => obj,
                    _ => return None,
                };

                let last_usage = info.get("last_token_usage")?;
                let input_tokens = last_usage.get("input_tokens")?.as_u64()?;
                let output_tokens = last_usage.get("output_tokens")?.as_u64().unwrap_or(0);
                let cached_input_tokens = last_usage
                    .get("cached_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                let ts = parsed.timestamp.unwrap_or("");
                let model = self.get_model(source_file);
                let event_key = format!("codex:{}:{}", source_file, ts);

                Some(UsageEvent {
                    event_key,
                    source_file: source_file.to_string(),
                    model,
                    input_tokens,
                    output_tokens,
                    // Map Codex fields to Claude Code schema
                    cache_creation_input_tokens: 0,
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

        let parsed: CodexLine = serde_json::from_str(line).ok()?;

        match parsed.line_type {
            "turn_context" => {
                if let Some(payload) = &parsed.payload {
                    if let Some(model) = payload.get("model").and_then(|v| v.as_str()) {
                        self.set_model(source_file, model);
                    }
                }
                None
            }
            "event_msg" => {
                let payload = parsed.payload.as_ref()?;
                if payload.get("type")?.as_str()? != "token_count" {
                    return None;
                }

                let info = match payload.get("info") {
                    Some(serde_json::Value::Object(obj)) => obj,
                    _ => return None,
                };

                let last_usage = info.get("last_token_usage")?;
                let input_tokens = last_usage.get("input_tokens")?.as_u64()?;
                let output_tokens = last_usage.get("output_tokens")?.as_u64().unwrap_or(0);
                let cached_input_tokens = last_usage
                    .get("cached_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                let ts = parsed.timestamp.unwrap_or_default().to_string();
                let model = self.get_model(source_file);
                let event_key = format!("codex:{}:{}", source_file, &ts);

                Some(UsageEventWithTs {
                    event_key,
                    source_file: source_file.to_string(),
                    model,
                    input_tokens,
                    output_tokens,
                    cache_creation_input_tokens: 0,
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

        match &result.provider_data {
            ProviderTokenData::Codex(f) => {
                assert_eq!(f.input_tokens, 15262);
                assert_eq!(f.output_tokens, 82);
                assert_eq!(f.cached_input_tokens, 15104);
                assert_eq!(f.reasoning_output_tokens, 0);
            }
            _ => panic!("Expected Codex token data"),
        }
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
