use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use serde::Deserialize;
use xxhash_rust::xxh3::xxh3_64;

use crate::common::types::{LogParser, LogParserWithTs, SessionGroup, UsageEvent, UsageEventWithTs};
use crate::providers::{ColdStartParsed, FileParser};

/// Build a Codex `event_key` whose first `:`-segment is unique per token_count
/// event.
///
/// The dedup layer (`Database::bare_msg_id`) treats the substring before the
/// first `:` as the logical message id and collapses events that share it. That
/// contract holds for Claude Code (`{msg_id}:{ts}`, unique id first) but Codex
/// previously led with a constant literal (`codex`, watch) or a per-session UUID
/// (cold start), so every Codex event collapsed onto one shared id and all but
/// one were deleted. See issue #11.
///
/// Here we lead with a colon-free xxh3 hash of `identity + ts + token counts`:
/// - distinct events differ in `ts` (or token counts within the same ms), so
///   they get distinct keys and are all retained;
/// - re-reading the exact same physical line hashes identically, so dedup
///   collapses only true duplicates (idempotent re-scan after `daemon reset`).
///
/// `identity` is a per-source stable string: the source file path in watch
/// mode, the session id in cold start. The readable `ts` tail is kept after the
/// hash purely for debuggability.
fn codex_event_key(
    identity: &str,
    ts: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
) -> String {
    let mut buf = String::with_capacity(identity.len() + ts.len() + 48);
    buf.push_str(identity);
    buf.push('\0');
    buf.push_str(ts);
    let _ = write!(
        buf,
        "\0{}:{}:{}:{}",
        input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens
    );
    let h = xxh3_64(buf.as_bytes());
    format!("{:016x}:codex:{}", h, ts)
}

/// Read the `cwd` from a Codex rollout file's `session_meta` line on demand.
/// session_meta is the first line; we scan a small prefix to stay robust to any
/// leading lines, and stop early. Returns None if no session_meta/cwd is found.
fn read_first_session_meta_cwd(path: &str) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().take(50).map_while(Result::ok) {
        if !line.contains("\"session_meta\"") {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<CodexSessionMetaLine>(&line) {
            if let Some(cwd) = parsed.payload.and_then(|p| p.cwd) {
                return Some(cwd.to_string());
            }
        }
        // session_meta seen but no cwd → no point scanning further.
        break;
    }
    None
}

/// Path-independent identity for a session file, used as the `codex_event_key`
/// seed in watch mode: the session UUID from the filename, falling back to the
/// bare filename.
///
/// We deliberately avoid seeding with the full `source_file` path: the same file
/// can reach the watch path under different equivalent spellings — the FSEvents
/// watcher reports the canonical path (e.g. `/private/tmp/...`) while the poller
/// globs the configured dir (e.g. `/tmp/...`, a symlink). Seeding with the path
/// would hash those to different keys and count every event twice. The filename
/// is identical regardless of spelling, and the UUID matches session_meta.id so
/// watch and cold-start keys share one namespace.
fn watch_identity(source_file: &str) -> String {
    let name = source_file.rsplit('/').next().unwrap_or(source_file);
    let stem = name.strip_suffix(".jsonl").unwrap_or(name);
    super::extract_uuid_from_filename(stem).unwrap_or_else(|| stem.to_string())
}

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

                // Build a per-event-unique event key (see codex_event_key).
                let session_part = self
                    .session_id
                    .as_deref()
                    .unwrap_or("unknown");
                let event_key = codex_event_key(
                    session_part,
                    ts,
                    input_tokens,
                    output_tokens,
                    reasoning_output_tokens,
                    cached_input_tokens,
                );

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
    /// Per-file cwd tracking: file_path -> session_meta.cwd.
    /// Populated from the session_meta line so live (watch) events can be
    /// attributed to a project instead of bucketing as `unknown` (issue #11,
    /// Bug 2). Cold start reads cwd directly via CodexFileParser.
    file_cwds: Mutex<HashMap<String, String>>,
}

impl CodexParser {
    pub fn new() -> Self {
        CodexParser {
            file_models: Mutex::new(HashMap::new()),
            file_cwds: Mutex::new(HashMap::new()),
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

    /// Project name (cwd) discovered from session_meta for a given source file.
    pub fn cwd_for(&self, source_file: &str) -> Option<String> {
        self.file_cwds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(source_file)
            .cloned()
    }

    /// Resolve cwd for a source file, falling back to a one-time on-demand read
    /// of the file's session_meta when the watch parser never saw it.
    ///
    /// The watch parser only captures cwd from session_meta lines it actually
    /// streams. A file cold-started before the watcher attached has its
    /// session_meta consumed by the cold-start parser, so live-appended events
    /// would otherwise resolve to no project (`unknown`). Here we read the
    /// session_meta directly (it is the file's first line) and cache it.
    pub fn cwd_for_or_read(&self, source_file: &str) -> Option<String> {
        if let Some(cwd) = self.cwd_for(source_file) {
            return Some(cwd);
        }
        let cwd = read_first_session_meta_cwd(source_file)?;
        self.set_cwd(source_file, &cwd);
        Some(cwd)
    }

    fn set_cwd(&self, source_file: &str, cwd: &str) {
        let mut map = self.file_cwds
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Same bounded-growth eviction policy as set_model.
        if map.len() > 500 {
            let keys_to_remove: Vec<String> = map.keys().take(250).cloned().collect();
            for key in keys_to_remove {
                map.remove(&key);
            }
        }
        map.insert(source_file.to_string(), cwd.to_string());
    }
}

impl LogParser for CodexParser {
    fn parse_line(&self, line: &str, source_file: &str) -> Option<UsageEvent> {
        // Pre-filter (session_meta is cheap: one line per file, carries cwd)
        if !line.contains("\"token_count\"")
            && !line.contains("\"turn_context\"")
            && !line.contains("\"session_meta\"")
        {
            return None;
        }

        // First pass: extract type and timestamp only (no payload heap alloc)
        let header: CodexLineHeader = serde_json::from_str(line).ok()?;

        match header.line_type {
            "session_meta" => {
                // Track cwd per source file so live events get a project (Bug 2).
                let parsed: CodexSessionMetaLine = serde_json::from_str(line).ok()?;
                if let Some(cwd) = parsed.payload.and_then(|p| p.cwd) {
                    self.set_cwd(source_file, cwd);
                }
                None
            }
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
                let event_key = codex_event_key(
                    &watch_identity(source_file),
                    ts,
                    input_tokens,
                    output_tokens,
                    reasoning_output_tokens,
                    cached_input_tokens,
                );

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
        // Pre-filter (session_meta is cheap: one line per file, carries cwd)
        if !line.contains("\"token_count\"")
            && !line.contains("\"turn_context\"")
            && !line.contains("\"session_meta\"")
        {
            return None;
        }

        // First pass: extract type and timestamp only (no payload heap alloc)
        let header: CodexLineHeader = serde_json::from_str(line).ok()?;

        match header.line_type {
            "session_meta" => {
                // Track cwd per source file so live events get a project (Bug 2).
                let parsed: CodexSessionMetaLine = serde_json::from_str(line).ok()?;
                if let Some(cwd) = parsed.payload.and_then(|p| p.cwd) {
                    self.set_cwd(source_file, cwd);
                }
                None
            }
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
                let event_key = codex_event_key(
                    &watch_identity(source_file),
                    &ts,
                    input_tokens,
                    output_tokens,
                    reasoning_output_tokens,
                    cached_input_tokens,
                );

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

    /// First ':'-segment of the event_key — the id the dedup layer collapses on.
    fn bare(event_key: &str) -> &str {
        event_key.split(':').next().unwrap()
    }

    fn tc_line(ts: &str, input: u64, output: u64) -> String {
        format!(
            r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":{},"cached_input_tokens":0,"output_tokens":{},"reasoning_output_tokens":0,"total_tokens":0}}}}}}}}"#,
            ts, input, output
        )
    }

    // --- Issue #11, Bug 1: distinct Codex events must not collapse on a shared id ---

    #[test]
    fn test_watch_event_keys_do_not_collapse() {
        let parser = CodexParser::new();
        let sf = "/test/rollout-abc.jsonl";
        let e1 = parser.parse_line(&tc_line("2026-06-19T11:38:31.718Z", 100, 10), sf).unwrap();
        let e2 = parser.parse_line(&tc_line("2026-06-19T11:39:01.000Z", 200, 20), sf).unwrap();
        // Pre-fix both bare ids were the literal "codex" -> global collapse.
        assert_ne!(bare(&e1.event_key), bare(&e2.event_key));
        assert_ne!(e1.event_key, e2.event_key);
    }

    #[test]
    fn test_coldstart_event_keys_do_not_collapse() {
        let mut parser = CodexFileParser::new();
        parser.session_id = Some("019ed863-b315-76f1-891a-e8d55fe53f0d".to_string());
        let e1 = parser.parse_line(&tc_line("2026-06-18T10:41:13.000Z", 100, 10)).unwrap();
        let e2 = parser.parse_line(&tc_line("2026-06-18T10:41:59.000Z", 200, 20)).unwrap();
        // Pre-fix both bare ids were the session UUID -> per-session collapse.
        assert_ne!(bare(&e1.event_key), bare(&e2.event_key));
    }

    #[test]
    fn test_event_key_idempotent_on_reread() {
        // Re-reading the exact same physical line must yield the same key so the
        // dedup collapses it onto itself (safe rescan after `daemon reset`).
        let parser = CodexParser::new();
        let sf = "/test/rollout-abc.jsonl";
        let line = tc_line("2026-06-19T11:38:31.718Z", 100, 10);
        let a = parser.parse_line(&line, sf).unwrap();
        let b = parser.parse_line(&line, sf).unwrap();
        assert_eq!(a.event_key, b.event_key);
    }

    #[test]
    fn test_watch_key_is_path_independent() {
        // The same file can reach the watch path under equivalent spellings: the
        // FSEvents watcher reports the canonical path while the poller globs the
        // configured (possibly symlinked) dir. Both must yield the same key, or
        // the event is counted twice.
        let parser = CodexParser::new();
        let line = tc_line("2026-06-30T20:00:05.000Z", 1000, 100);
        let name = "rollout-2026-06-30T20-00-00-019f1abc-0000-7000-8000-live00000001.jsonl";
        let a = parser.parse_line(&line, &format!("/tmp/sessions/{name}")).unwrap();
        let b = parser.parse_line(&line, &format!("/private/tmp/sessions/{name}")).unwrap();
        assert_eq!(a.event_key, b.event_key);
    }

    // --- Issue #11, Bug 2: watch parser resolves cwd from session_meta ---

    #[test]
    fn test_cwd_for_or_read_falls_back_to_file() {
        // Simulates a file cold-started before the watcher attached: the watch
        // parser never streamed session_meta, so cwd_for is empty, but
        // cwd_for_or_read reads it from the file on demand and caches it.
        let dir = std::env::temp_dir().join(format!("toki-codex-cwd-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("rollout-x.jsonl");
        std::fs::write(&path, concat!(
            r#"{"timestamp":"2026-06-30T20:00:00.000Z","type":"session_meta","payload":{"id":"abc","cwd":"/proj/foo"}}"#, "\n",
            r#"{"timestamp":"2026-06-30T20:00:05.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"output_tokens":1}}}}"#, "\n",
        )).unwrap();
        let p = path.to_string_lossy().to_string();

        let parser = CodexParser::new();
        assert_eq!(parser.cwd_for(&p), None); // watch parser never saw session_meta
        assert_eq!(parser.cwd_for_or_read(&p).as_deref(), Some("/proj/foo")); // read on demand
        assert_eq!(parser.cwd_for(&p).as_deref(), Some("/proj/foo")); // now cached
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_watch_tracks_cwd_from_session_meta() {
        let parser = CodexParser::new();
        let sf = "/test/rollout-abc.jsonl";
        assert_eq!(parser.cwd_for(sf), None);
        let meta = r#"{"timestamp":"2026-06-19T11:38:00.000Z","type":"session_meta","payload":{"id":"019ed863-b315-76f1-891a-e8d55fe53f0d","cwd":"/Users/test/sveltos-infra-apps"}}"#;
        assert!(parser.parse_line(meta, sf).is_none());
        assert_eq!(parser.cwd_for(sf).as_deref(), Some("/Users/test/sveltos-infra-apps"));
        // Unrelated files remain unknown.
        assert_eq!(parser.cwd_for("/test/other.jsonl"), None);
    }
}
