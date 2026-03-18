pub mod parser;

pub use parser::ClaudeCodeParser;

use crate::common::types::{LogParser, LogParserWithTs, SessionGroup};
use crate::providers::{
    ColdStartParsed, FileParser, Provider,
};

/// Claude Code provider implementation.
pub struct ClaudeCodeProvider {
    root: String,
    parser: ClaudeCodeParser,
}

impl ClaudeCodeProvider {
    pub fn new(root: String) -> Self {
        ClaudeCodeProvider {
            root,
            parser: ClaudeCodeParser,
        }
    }
}

impl Provider for ClaudeCodeProvider {
    fn name(&self) -> &str {
        "claude_code"
    }

    fn display_name(&self) -> &str {
        "Claude Code"
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
        let projects_dir = format!("{}/projects", self.root);
        if std::path::Path::new(&projects_dir).exists() {
            vec![projects_dir]
        } else if std::path::Path::new(&self.root).exists() {
            vec![self.root.clone()]
        } else {
            vec![]
        }
    }

    fn owns_path(&self, path: &str) -> bool {
        path.contains("/.claude/")
    }

    fn discover_sessions(&self) -> Vec<SessionGroup> {
        self.parser.discover_sessions(&self.root)
    }

    fn create_file_parser(&self) -> Box<dyn FileParser> {
        Box::new(ClaudeCodeFileParser)
    }

    fn parser(&self) -> &dyn LogParser {
        &self.parser
    }

    fn parser_with_ts(&self) -> &dyn LogParserWithTs {
        &self.parser
    }

    fn extract_session_id(&self, path: &str) -> Option<String> {
        extract_session_id(path)
    }

    fn extract_project_name<'a>(&self, path: &'a str) -> Option<&'a str> {
        extract_project_name(path)
    }

    fn db_dir_name(&self) -> &str {
        "claude_code.fjall"
    }

    /// Override: use concrete ClaudeCodeParser directly for inlining (no dyn dispatch).
    fn scan_file_cold_start(&self, path: &str, offset: u64, emit: &mut dyn FnMut(ColdStartParsed))
        -> std::io::Result<Option<(u64, u64, u64)>>
    {
        let parser = ClaudeCodeParser;
        crate::checkpoint::process_lines_streaming(path, offset, |line| {
            if let Some(parsed) = parser.parse_for_cold_start(line) {
                emit(parsed);
            }
        })
    }
}

/// Stateless per-file parser for Claude Code cold start.
struct ClaudeCodeFileParser;

impl FileParser for ClaudeCodeFileParser {
    fn parse_line(&mut self, line: &str) -> Option<ColdStartParsed> {
        let parser = ClaudeCodeParser;
        // parse_for_cold_start now returns providers::ColdStartParsed directly
        parser.parse_for_cold_start(line)
    }
}

/// Extract the full session UUID from a Claude Code file path.
///   Parent:   .../projects/<dir>/<UUID>.jsonl        -> "<UUID>"
///   Subagent: .../<UUID>/subagents/agent-<id>.jsonl  -> "<UUID>" (grandparent dir name)
pub fn extract_session_id(path: &str) -> Option<String> {
    let mut parts = path.rsplit('/');
    let filename = parts.next()?;
    // Subagent: .../<UUID>/subagents/agent-xxx.jsonl
    if let Some(dir) = parts.next() {
        if dir == "subagents" {
            return parts.next().map(|s| s.to_string());
        }
    }
    // Parent: filename without .jsonl
    Some(filename.trim_end_matches(".jsonl").to_string())
}

/// Extract project directory name from a Claude Code file path (zero-alloc, returns &str slice).
///   .../projects/<PROJECT_DIR>/<UUID>.jsonl -> "<PROJECT_DIR>"
///   .../projects/<PROJECT_DIR>/<UUID>/subagents/agent-<id>.jsonl -> "<PROJECT_DIR>"
pub fn extract_project_name(path: &str) -> Option<&str> {
    let marker = "/projects/";
    let start = path.find(marker)? + marker.len();
    let rest = &path[start..];
    let end = rest.find('/').unwrap_or(rest.len());
    Some(&rest[..end])
}
