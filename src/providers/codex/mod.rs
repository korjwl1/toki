pub mod parser;

pub use parser::CodexParser;

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
