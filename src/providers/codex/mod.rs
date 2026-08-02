pub mod parser;

pub use parser::CodexParser;
pub(crate) use parser::parse_rate_limits_line;

/// Resolve the Codex account scope from `<codex_root>/auth.json`, as a
/// privacy-safe hash string. Returns "unknown" when unreadable.
pub fn account_scope(codex_root: &str) -> String {
    let path = std::path::Path::new(codex_root).join("auth.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return "unknown".to_string(),
    };
    #[derive(serde::Deserialize)]
    struct Auth {
        tokens: Option<AuthTokens>,
    }
    #[derive(serde::Deserialize)]
    struct AuthTokens {
        account_id: Option<String>,
    }
    match serde_json::from_str::<Auth>(&raw) {
        Ok(a) => match a.tokens.and_then(|t| t.account_id) {
            Some(id) if !id.is_empty() => format!("{:016x}", crate::windows::hash_str(&id)),
            _ => "unknown".to_string(),
        },
        Err(_) => "unknown".to_string(),
    }
}

/// Classify Codex auth by reading auth.json on demand (always current — the
/// monitor's old inode watcher existed only to trigger its own HTTP re-polls).
pub fn auth_status(codex_root: &str) -> crate::claude_poll::AuthStatus {
    use crate::claude_poll::AuthStatus;
    let path = std::path::Path::new(codex_root).join("auth.json");
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) if v.get("tokens").map(|t| !t.is_null()).unwrap_or(false) => AuthStatus::Ok,
            Ok(_) => AuthStatus::Missing,
            Err(_) => AuthStatus::Unreadable,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => AuthStatus::Missing,
        Err(_) => AuthStatus::Unreadable,
    }
}

/// Fast-mode pricing multipliers for Codex models.
/// Empty placeholder: Codex JSONL carries no service_tier marker and the
/// OpenAI API response does not echo it back, so per-event Fast detection
/// is impossible. Populate this list only if Codex CLI starts recording
/// the tier inside its session JSONL.
/// Reference (ccusage's table, kept here for future activation):
///   gpt-5.5 -> 2.5x, gpt-5.4 -> 2.0x, gpt-5.3-codex -> 2.0x
pub const FAST_MULTIPLIER: &[(&str, f64)] = &[];

use std::path::PathBuf;

use crate::common::types::{LogParser, LogParserWithTs, SessionGroup};
use crate::providers::{FileParser, Provider};

/// Codex CLI provider implementation.
pub struct CodexProvider {
    root: String,
    parser: CodexParser,
}

impl CodexProvider {
    pub fn new(root: String) -> Self {
        CodexProvider {
            root,
            parser: CodexParser::new(),
        }
    }
}

impl Provider for CodexProvider {
    fn name(&self) -> &str {
        "codex"
    }

    fn display_name(&self) -> &str {
        "Codex CLI"
    }

    fn root_dir(&self) -> Option<String> {
        let path = std::path::Path::new(&self.root);
        if path.exists() {
            Some(self.root.clone())
        } else {
            None
        }
    }

    fn watch_dirs(&self) -> Vec<String> {
        let sessions_dir = format!("{}/sessions", self.root);
        if std::path::Path::new(&sessions_dir).exists() {
            vec![sessions_dir]
        } else {
            vec![]
        }
    }

    fn owns_path(&self, path: &str) -> bool {
        path.contains("/.codex/")
    }

    fn discover_sessions(&self) -> Vec<SessionGroup> {
        let sessions_dir = format!("{}/sessions", self.root);
        if !std::path::Path::new(&sessions_dir).exists() {
            return vec![];
        }

        let pattern = format!("{}/**/*.jsonl", sessions_dir);
        let mut sessions = Vec::new();

        let jsonl_files: Vec<PathBuf> = glob::glob(&pattern)
            .into_iter()
            .flatten()
            .filter_map(|p| p.ok())
            .collect();

        for path in jsonl_files {
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };

            // Extract UUID from filename: rollout-YYYY-MM-DDTHH-MM-SS-<UUID>.jsonl
            let session_id = extract_uuid_from_filename(stem)
                .unwrap_or_else(|| stem.to_string());

            sessions.push(SessionGroup {
                session_id,
                parent_jsonl: path,
                subagent_jsonls: vec![], // Codex has no subagents
            });
        }

        sessions
    }

    fn create_file_parser(&self) -> Box<dyn FileParser> {
        Box::new(parser::CodexFileParser::new())
    }

    fn parser(&self) -> &dyn LogParser {
        &self.parser
    }

    fn parser_with_ts(&self) -> &dyn LogParserWithTs {
        &self.parser
    }

    fn extract_session_id(&self, path: &str) -> Option<String> {
        let filename = path.rsplit('/').next()?;
        let stem = filename.trim_end_matches(".jsonl");
        extract_uuid_from_filename(stem).or_else(|| Some(stem.to_string()))
    }

    fn extract_project_name<'a>(&self, _path: &'a str) -> Option<&'a str> {
        // Codex stores project info in session_meta inside the file content,
        // not in the file path. Returns None here; project_name is set during parsing.
        None
    }

    fn resolve_project_name(&self, path: &str) -> Option<String> {
        // cwd is discovered from session_meta and cached per source file by the
        // watch parser; `path` is the source file (issue #11, Bug 2). Falls back
        // to a one-time on-demand read of session_meta when the watcher never saw
        // it (file cold-started before the watcher attached), so live-appended
        // events don't land under `unknown`.
        self.parser.cwd_for_or_read(path)
    }

    fn poll_dirs(&self) -> Option<Vec<String>> {
        // macOS: FSEvents only fires FSE_CONTENT_MODIFIED on vn_close(). Codex holds a single
        // tokio::fs::File open for the entire session and only flushes — never closes — between
        // turns. Result: zero FSEvents during an active session; one event on session exit.
        // Poll the sessions directory every second to catch writes to open fds.
        //
        // Linux/Windows: inotify IN_MODIFY / ReadDirectoryChangesW fire per write regardless
        // of fd close, so the native watcher already handles this correctly.
        #[cfg(target_os = "macos")]
        {
            return Some(self.watch_dirs());
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    fn db_dir_name(&self) -> &str {
        "codex.fjall"
    }

    /// Override: use concrete CodexFileParser directly for inlining (no dyn dispatch).
    fn scan_file_cold_start(&self, path: &str, offset: u64, emit: &mut dyn FnMut(super::ColdStartParsed))
        -> std::io::Result<Option<(u64, u64, u64)>>
    {
        let mut parser = crate::providers::codex::parser::CodexFileParser::new();
        crate::checkpoint::process_lines_streaming(path, offset, |line| {
            if let Some(parsed) = <crate::providers::codex::parser::CodexFileParser as crate::providers::FileParser>::parse_line(&mut parser, line) {
                emit(parsed);
            }
        })
    }
}

/// Extract UUID from Codex filename format: rollout-YYYY-MM-DDTHH-MM-SS-<UUID>
/// The UUID is the last 36 characters (8-4-4-4-12 hex with dashes).
fn extract_uuid_from_filename(stem: &str) -> Option<String> {
    if stem.len() >= 36 {
        let candidate = &stem[stem.len() - 36..];
        // Validate UUID format: 8-4-4-4-12
        let parts: Vec<&str> = candidate.split('-').collect();
        if parts.len() == 5
            && parts[0].len() == 8
            && parts[1].len() == 4
            && parts[2].len() == 4
            && parts[3].len() == 4
            && parts[4].len() == 12
            && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Some(candidate.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_uuid_from_filename() {
        let stem = "rollout-2026-03-12T00-35-10-019cdd89-9fd9-7f11-b555-459c0ec30834";
        let uuid = extract_uuid_from_filename(stem);
        assert_eq!(uuid, Some("019cdd89-9fd9-7f11-b555-459c0ec30834".to_string()));
    }

    #[test]
    fn test_extract_uuid_invalid() {
        assert_eq!(extract_uuid_from_filename("not-a-uuid"), None);
        assert_eq!(extract_uuid_from_filename(""), None);
    }
}
