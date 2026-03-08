use crate::common::types::{LogParser, SessionGroup, UsageEvent};
use serde_json::Value;
use std::path::PathBuf;

pub struct ClaudeCodeParser;

impl ClaudeCodeParser {
    /// Check if a filename matches UUID format (8-4-4-4-12 hex).
    fn is_uuid_filename(name: &str) -> bool {
        let parts: Vec<&str> = name.split('-').collect();
        if parts.len() != 5 {
            return false;
        }
        let expected_lens = [8, 4, 4, 4, 12];
        parts.iter().zip(expected_lens.iter()).all(|(part, &len)| {
            part.len() == len && part.chars().all(|c| c.is_ascii_hexdigit())
        })
    }
}

impl LogParser for ClaudeCodeParser {
    fn parse_line(&self, line: &str, source_file: &str) -> Option<UsageEvent> {
        let v: Value = serde_json::from_str(line).ok()?;

        // Only process "assistant" type lines
        if v.get("type")?.as_str()? != "assistant" {
            return None;
        }

        let msg = v.get("message")?;
        let usage = msg.get("usage")?;

        let input_tokens = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let cache_creation_input_tokens = usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_read_input_tokens = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output_tokens = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);

        let model = msg
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let message_id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let timestamp = v.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
        let event_key = format!("{}:{}", message_id, timestamp);

        Some(UsageEvent {
            event_key,
            source_file: source_file.to_string(),
            model,
            input_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            output_tokens,
        })
    }

    fn file_patterns(&self, root_dir: &str) -> Vec<String> {
        vec![
            format!("{}/projects/**/*.jsonl", root_dir),
        ]
    }

    fn discover_sessions(&self, root_dir: &str) -> Vec<SessionGroup> {
        let pattern = format!("{}/projects/**/*.jsonl", root_dir);
        let mut sessions = Vec::new();

        let jsonl_files: Vec<PathBuf> = glob::glob(&pattern)
            .into_iter()
            .flatten()
            .filter_map(|p| p.ok())
            .collect();

        for path in &jsonl_files {
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };

            // Only process UUID-named JSONL files (not subagent files like agent-*.jsonl)
            if !Self::is_uuid_filename(stem) {
                continue;
            }

            let session_id = stem.to_string();
            let parent_dir = match path.parent() {
                Some(p) => p,
                None => continue,
            };

            // Check for subagent directory: <UUID>/subagents/agent-*.jsonl
            let subagent_dir = parent_dir.join(&session_id).join("subagents");
            let subagent_jsonls = if subagent_dir.is_dir() {
                let sub_pattern = subagent_dir.join("agent-*.jsonl");
                glob::glob(sub_pattern.to_str().unwrap_or(""))
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p.ok())
                    .collect()
            } else {
                Vec::new()
            };

            sessions.push(SessionGroup {
                session_id,
                parent_jsonl: path.clone(),
                subagent_jsonls,
            });
        }

        sessions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_assistant_line() {
        let line = r#"{"type":"assistant","message":{"id":"msg_123","model":"claude-opus-4-6","usage":{"input_tokens":3,"cache_creation_input_tokens":5139,"cache_read_input_tokens":9631,"output_tokens":14}},"timestamp":"2026-03-08T12:00:00Z"}"#;
        let parser = ClaudeCodeParser;
        let event = parser.parse_line(line, "/test/file.jsonl").unwrap();

        assert_eq!(event.model, "claude-opus-4-6");
        assert_eq!(event.input_tokens, 3);
        assert_eq!(event.cache_creation_input_tokens, 5139);
        assert_eq!(event.cache_read_input_tokens, 9631);
        assert_eq!(event.output_tokens, 14);
        assert_eq!(event.event_key, "msg_123:2026-03-08T12:00:00Z");
    }

    #[test]
    fn test_skip_non_assistant_lines() {
        let parser = ClaudeCodeParser;

        // user type
        let line = r#"{"type":"user","message":{"text":"hello"}}"#;
        assert!(parser.parse_line(line, "/test.jsonl").is_none());

        // file-history-snapshot type
        let line = r#"{"type":"file-history-snapshot","messageId":"abc","snapshot":{}}"#;
        assert!(parser.parse_line(line, "/test.jsonl").is_none());
    }

    #[test]
    fn test_skip_invalid_json() {
        let parser = ClaudeCodeParser;
        assert!(parser.parse_line("not json", "/test.jsonl").is_none());
    }

    #[test]
    fn test_missing_usage_fields_default_zero() {
        let line = r#"{"type":"assistant","message":{"id":"msg_1","model":"claude-opus-4-6","usage":{"input_tokens":10,"output_tokens":5}},"timestamp":"2026-03-08T12:00:00Z"}"#;
        let parser = ClaudeCodeParser;
        let event = parser.parse_line(line, "/test.jsonl").unwrap();

        assert_eq!(event.input_tokens, 10);
        assert_eq!(event.output_tokens, 5);
        assert_eq!(event.cache_creation_input_tokens, 0);
        assert_eq!(event.cache_read_input_tokens, 0);
    }

    #[test]
    fn test_is_uuid_filename() {
        assert!(ClaudeCodeParser::is_uuid_filename(
            "4de9291e-061e-414a-85cb-de615826aded"
        ));
        assert!(!ClaudeCodeParser::is_uuid_filename("agent-aed1da92cc2e4e9e7"));
        assert!(!ClaudeCodeParser::is_uuid_filename("not-a-uuid"));
        assert!(!ClaudeCodeParser::is_uuid_filename(""));
    }

    #[test]
    fn test_discover_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let projects_dir = dir.path().join("projects").join("test-project");
        std::fs::create_dir_all(&projects_dir).unwrap();

        // Create a UUID.jsonl
        let uuid = "4de9291e-061e-414a-85cb-de615826aded";
        let jsonl_path = projects_dir.join(format!("{}.jsonl", uuid));
        std::fs::write(&jsonl_path, "").unwrap();

        // Create subagent directory + file
        let sub_dir = projects_dir.join(uuid).join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        std::fs::write(sub_dir.join("agent-abc123.jsonl"), "").unwrap();

        let parser = ClaudeCodeParser;
        let sessions = parser.discover_sessions(dir.path().to_str().unwrap());

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, uuid);
        assert_eq!(sessions[0].subagent_jsonls.len(), 1);
    }

    #[test]
    fn test_discover_sessions_no_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let projects_dir = dir.path().join("projects").join("test-project");
        std::fs::create_dir_all(&projects_dir).unwrap();

        let uuid = "db7cd31e-fdb1-4767-a6a2-f2f3dc68a74b";
        std::fs::write(projects_dir.join(format!("{}.jsonl", uuid)), "").unwrap();

        let parser = ClaudeCodeParser;
        let sessions = parser.discover_sessions(dir.path().to_str().unwrap());

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].subagent_jsonls.len(), 0);
    }
}
