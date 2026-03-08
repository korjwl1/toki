use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::checkpoint::{hash_line, read_checkpoint_bytes, read_from_offset, verify_checkpoint, recover_by_hash};
use crate::common::types::{FileCheckpoint, LogParser, ModelUsageSummary, UsageEvent};
use crate::db::Database;

pub struct TrackerEngine {
    db: Database,
    checkpoints: HashMap<String, FileCheckpoint>,
    dirty_checkpoints: Vec<String>,
}

impl TrackerEngine {
    pub fn new(db: Database) -> Self {
        TrackerEngine {
            db,
            checkpoints: HashMap::new(),
            dirty_checkpoints: Vec::new(),
        }
    }

    /// Load existing checkpoints from DB into memory.
    pub fn load_checkpoints(&mut self) -> Result<(), redb::Error> {
        let cps = self.db.load_all_checkpoints()?;
        for cp in cps {
            self.checkpoints.insert(cp.file_path.clone(), cp);
        }
        Ok(())
    }

    /// Cold start: discover all sessions, process them in parallel,
    /// aggregate by model, print summary, and flush checkpoints.
    pub fn cold_start(
        &mut self,
        parser: &(dyn LogParser + Sync),
        root_dir: &str,
    ) -> Result<HashMap<String, ModelUsageSummary>, Box<dyn std::error::Error>> {
        let sessions = parser.discover_sessions(root_dir);

        if sessions.is_empty() {
            return Ok(HashMap::new());
        }

        let max_threads = num_cpus::get();
        let semaphore = Arc::new(Mutex::new(max_threads));

        // Collect all (path, events) results from parallel processing
        let all_results: Arc<Mutex<Vec<(String, Vec<UsageEvent>, u64, String, Vec<u8>)>>> =
            Arc::new(Mutex::new(Vec::new()));

        std::thread::scope(|s| {
            for session in &sessions {
                // Collect all files in this session (parent + subagents)
                let mut files: Vec<&std::path::Path> = vec![session.parent_jsonl.as_path()];
                for sub in &session.subagent_jsonls {
                    files.push(sub.as_path());
                }

                for file_path in files {
                    let sem = Arc::clone(&semaphore);
                    let results = Arc::clone(&all_results);
                    let checkpoints = &self.checkpoints;

                    s.spawn(move || {
                        // Acquire semaphore slot
                        {
                            let mut count = sem.lock().unwrap();
                            while *count == 0 {
                                drop(count);
                                std::thread::yield_now();
                                count = sem.lock().unwrap();
                            }
                            *count -= 1;
                        }

                        let path_str = file_path.to_string_lossy().to_string();
                        let result = process_file_cold(&path_str, parser, checkpoints);

                        // Release semaphore slot
                        {
                            let mut count = sem.lock().unwrap();
                            *count += 1;
                        }

                        if let Ok(Some((events, new_offset, last_hash, cp_bytes))) = result {
                            let mut r = results.lock().unwrap();
                            r.push((path_str, events, new_offset, last_hash, cp_bytes));
                        }
                    });
                }
            }
        });

        // Merge results into summaries and update checkpoints
        let mut summaries: HashMap<String, ModelUsageSummary> = HashMap::new();
        let results = Arc::try_unwrap(all_results).unwrap().into_inner().unwrap();

        for (path, events, new_offset, last_hash, cp_bytes) in results {
            for event in &events {
                let summary = summaries
                    .entry(event.model.clone())
                    .or_insert_with(|| ModelUsageSummary {
                        model: event.model.clone(),
                        ..Default::default()
                    });
                summary.accumulate(event);
            }

            // Update checkpoint
            let cp = FileCheckpoint {
                file_path: path.clone(),
                last_offset: new_offset,
                last_line_hash: last_hash,
                checkpoint_bytes: cp_bytes,
            };
            self.checkpoints.insert(path.clone(), cp);
            self.dirty_checkpoints.push(path);
        }

        // Print summary
        print_summary(&summaries);

        // Flush checkpoints
        self.flush_checkpoints()?;

        Ok(summaries)
    }

    /// Process a single file change (watch mode).
    pub fn process_file(
        &mut self,
        path: &str,
        parser: &dyn LogParser,
    ) -> Result<Vec<UsageEvent>, Box<dyn std::error::Error>> {
        let metadata = std::fs::metadata(path)?;
        let file_size = metadata.len();
        let offset = self.determine_offset(path, file_size)?;

        let (lines, bytes_read) = read_from_offset(path, offset)?;
        if lines.is_empty() {
            return Ok(Vec::new());
        }

        let mut events = Vec::new();
        for line in &lines {
            if let Some(event) = parser.parse_line(line, path) {
                events.push(event);
            }
        }

        let new_offset = offset + bytes_read;
        let last_line = lines.last().unwrap();
        let last_hash = hash_line(last_line);
        let cp_bytes = read_checkpoint_bytes(path, new_offset)?;

        let cp = FileCheckpoint {
            file_path: path.to_string(),
            last_offset: new_offset,
            last_line_hash: last_hash,
            checkpoint_bytes: cp_bytes,
        };
        self.checkpoints.insert(path.to_string(), cp);
        self.dirty_checkpoints.push(path.to_string());

        Ok(events)
    }

    fn determine_offset(&self, path: &str, file_size: u64) -> Result<u64, Box<dyn std::error::Error>> {
        let cp = match self.checkpoints.get(path) {
            None => return Ok(0),
            Some(cp) => cp,
        };

        if file_size >= cp.last_offset && verify_checkpoint(path, cp)? {
            Ok(cp.last_offset)
        } else {
            match recover_by_hash(path, &cp.last_line_hash)? {
                Some(offset) => Ok(offset),
                None => Ok(0),
            }
        }
    }

    /// Flush dirty checkpoints to database.
    pub fn flush_checkpoints(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.dirty_checkpoints.is_empty() {
            return Ok(());
        }

        let to_flush: Vec<FileCheckpoint> = self
            .dirty_checkpoints
            .drain(..)
            .filter_map(|path| self.checkpoints.get(&path).cloned())
            .collect();

        self.db.flush_checkpoints(&to_flush)?;
        Ok(())
    }
}

/// Process a single file during cold start (called from scoped threads).
/// Returns (events, new_offset, last_line_hash, checkpoint_bytes) or None if no new data.
fn process_file_cold(
    path: &str,
    parser: &(dyn LogParser + Sync),
    checkpoints: &HashMap<String, FileCheckpoint>,
) -> Result<Option<(Vec<UsageEvent>, u64, String, Vec<u8>)>, Box<dyn std::error::Error>> {
    let metadata = std::fs::metadata(path)?;
    let file_size = metadata.len();

    // Determine offset
    let offset = match checkpoints.get(path) {
        None => 0,
        Some(cp) => {
            if file_size >= cp.last_offset && verify_checkpoint(path, cp).unwrap_or(false) {
                cp.last_offset
            } else {
                recover_by_hash(path, &cp.last_line_hash)
                    .unwrap_or(None)
                    .unwrap_or(0)
            }
        }
    };

    let (lines, bytes_read) = read_from_offset(path, offset)?;
    if lines.is_empty() {
        return Ok(None);
    }

    let mut events = Vec::new();
    for line in &lines {
        if let Some(event) = parser.parse_line(line, path) {
            events.push(event);
        }
    }

    let new_offset = offset + bytes_read;
    let last_line = lines.last().unwrap();
    let last_hash = hash_line(last_line);
    let cp_bytes = read_checkpoint_bytes(path, new_offset)?;

    Ok(Some((events, new_offset, last_hash, cp_bytes)))
}

/// Print cold start summary.
pub fn print_summary(summaries: &HashMap<String, ModelUsageSummary>) {
    if summaries.is_empty() {
        println!("[webtrace] No usage data found.");
        return;
    }

    println!("[webtrace] ═══════════════════════════════════════════");
    println!("[webtrace] Token Usage Summary");

    let mut sorted: Vec<_> = summaries.values().collect();
    sorted.sort_by(|a, b| b.event_count.cmp(&a.event_count));

    for s in &sorted {
        println!("[webtrace] ───────────────────────────────────────────");
        println!("[webtrace] Model: {}", s.model);
        println!(
            "[webtrace]   Input: {:>12} | Cache Create: {:>12}",
            format_number(s.input_tokens),
            format_number(s.cache_creation_input_tokens),
        );
        println!(
            "[webtrace]   Cache Read: {:>8} | Output: {:>12}",
            format_number(s.cache_read_input_tokens),
            format_number(s.output_tokens),
        );
        println!("[webtrace]   Events: {}", s.event_count);
    }
    println!("[webtrace] ═══════════════════════════════════════════");
}

/// Print a single watch-mode event.
pub fn print_event(event: &UsageEvent) {
    let filename = event
        .source_file
        .rsplit('/')
        .next()
        .unwrap_or(&event.source_file);
    println!(
        "[webtrace] {} | {} | in:{} cc:{} cr:{} out:{}",
        event.model,
        filename,
        event.input_tokens,
        event.cache_creation_input_tokens,
        event.cache_read_input_tokens,
        event.output_tokens,
    );
}

fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::types::SessionGroup;
    use std::io::Write;

    struct TestParser;

    impl LogParser for TestParser {
        fn parse_line(&self, line: &str, source_file: &str) -> Option<UsageEvent> {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            if v.get("type")?.as_str()? != "assistant" {
                return None;
            }
            let msg = v.get("message")?;
            let usage = msg.get("usage")?;
            Some(UsageEvent {
                event_key: format!("{}:{}", msg.get("id")?.as_str()?, "ts"),
                source_file: source_file.to_string(),
                model: msg.get("model")?.as_str()?.to_string(),
                input_tokens: usage.get("input_tokens")?.as_u64()?,
                cache_creation_input_tokens: usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                cache_read_input_tokens: usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                output_tokens: usage.get("output_tokens")?.as_u64()?,
            })
        }

        fn file_patterns(&self, root_dir: &str) -> Vec<String> {
            vec![format!("{}/projects/**/*.jsonl", root_dir)]
        }

        fn discover_sessions(&self, root_dir: &str) -> Vec<SessionGroup> {
            let pattern = format!("{}/projects/**/*.jsonl", root_dir);
            let mut sessions = Vec::new();

            for entry in glob::glob(&pattern).into_iter().flatten().flatten() {
                let stem = entry.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                // Simple: treat each jsonl as its own session
                if !stem.starts_with("agent-") {
                    let parent_dir = entry.parent().unwrap();
                    let sub_dir = parent_dir.join(stem).join("subagents");
                    let subs = if sub_dir.is_dir() {
                        glob::glob(sub_dir.join("agent-*.jsonl").to_str().unwrap())
                            .into_iter()
                            .flatten()
                            .flatten()
                            .collect()
                    } else {
                        vec![]
                    };
                    sessions.push(SessionGroup {
                        session_id: stem.to_string(),
                        parent_jsonl: entry,
                        subagent_jsonls: subs,
                    });
                }
            }
            sessions
        }
    }

    fn make_assistant_line(id: &str, model: &str, input: u64, cc: u64, cr: u64, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"id":"{}","model":"{}","usage":{{"input_tokens":{},"cache_creation_input_tokens":{},"cache_read_input_tokens":{},"output_tokens":{}}}}},"timestamp":"2026-03-08T12:00:00Z"}}"#,
            id, model, input, cc, cr, output
        )
    }

    #[test]
    fn test_cold_start_single_session() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        let mut engine = TrackerEngine::new(db);

        // Create test JSONL
        let projects_dir = dir.path().join("projects").join("test");
        std::fs::create_dir_all(&projects_dir).unwrap();

        let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let jsonl_path = projects_dir.join(format!("{}.jsonl", session_id));

        let mut f = std::fs::File::create(&jsonl_path).unwrap();
        writeln!(f, "{}", make_assistant_line("msg1", "claude-opus-4-6", 3, 100, 200, 10)).unwrap();
        writeln!(f, "{}", make_assistant_line("msg2", "claude-opus-4-6", 5, 150, 300, 20)).unwrap();

        let parser = TestParser;
        let summaries = engine.cold_start(&parser, dir.path().to_str().unwrap()).unwrap();

        assert_eq!(summaries.len(), 1);
        let s = &summaries["claude-opus-4-6"];
        assert_eq!(s.input_tokens, 8);
        assert_eq!(s.cache_creation_input_tokens, 250);
        assert_eq!(s.cache_read_input_tokens, 500);
        assert_eq!(s.output_tokens, 30);
        assert_eq!(s.event_count, 2);
    }

    #[test]
    fn test_cold_start_with_subagent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        let mut engine = TrackerEngine::new(db);

        let projects_dir = dir.path().join("projects").join("test");
        std::fs::create_dir_all(&projects_dir).unwrap();

        let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let jsonl_path = projects_dir.join(format!("{}.jsonl", session_id));

        let mut f = std::fs::File::create(&jsonl_path).unwrap();
        writeln!(f, "{}", make_assistant_line("msg1", "claude-opus-4-6", 10, 100, 200, 50)).unwrap();

        // Create subagent
        let sub_dir = projects_dir.join(session_id).join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let sub_path = sub_dir.join("agent-abc123.jsonl");
        let mut sf = std::fs::File::create(&sub_path).unwrap();
        writeln!(sf, "{}", make_assistant_line("msg2", "claude-haiku-4-5-20251001", 5, 50, 100, 20)).unwrap();

        let parser = TestParser;
        let summaries = engine.cold_start(&parser, dir.path().to_str().unwrap()).unwrap();

        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries["claude-opus-4-6"].input_tokens, 10);
        assert_eq!(summaries["claude-haiku-4-5-20251001"].input_tokens, 5);
    }

    #[test]
    fn test_cold_start_multi_model() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        let mut engine = TrackerEngine::new(db);

        let projects_dir = dir.path().join("projects").join("test");
        std::fs::create_dir_all(&projects_dir).unwrap();

        let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let jsonl_path = projects_dir.join(format!("{}.jsonl", session_id));

        let mut f = std::fs::File::create(&jsonl_path).unwrap();
        writeln!(f, "{}", make_assistant_line("msg1", "claude-opus-4-6", 10, 100, 200, 50)).unwrap();
        writeln!(f, "{}", make_assistant_line("msg2", "claude-haiku-4-5-20251001", 5, 50, 100, 20)).unwrap();
        writeln!(f, "{}", make_assistant_line("msg3", "claude-opus-4-6", 15, 200, 300, 60)).unwrap();

        let parser = TestParser;
        let summaries = engine.cold_start(&parser, dir.path().to_str().unwrap()).unwrap();

        assert_eq!(summaries.len(), 2);
        let opus = &summaries["claude-opus-4-6"];
        assert_eq!(opus.input_tokens, 25);
        assert_eq!(opus.event_count, 2);
        let haiku = &summaries["claude-haiku-4-5-20251001"];
        assert_eq!(haiku.input_tokens, 5);
        assert_eq!(haiku.event_count, 1);
    }

    #[test]
    fn test_cold_start_checkpoints_saved() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        let mut engine = TrackerEngine::new(db);

        let projects_dir = dir.path().join("projects").join("test");
        std::fs::create_dir_all(&projects_dir).unwrap();

        let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let jsonl_path = projects_dir.join(format!("{}.jsonl", session_id));

        let mut f = std::fs::File::create(&jsonl_path).unwrap();
        writeln!(f, "{}", make_assistant_line("msg1", "claude-opus-4-6", 10, 100, 200, 50)).unwrap();

        let parser = TestParser;
        engine.cold_start(&parser, dir.path().to_str().unwrap()).unwrap();

        // Verify checkpoint was saved
        let path_str = jsonl_path.to_str().unwrap();
        assert!(engine.checkpoints.contains_key(path_str));
        assert!(engine.checkpoints[path_str].last_offset > 0);
    }

    #[test]
    fn test_cold_start_incremental() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        let mut engine = TrackerEngine::new(db);

        let projects_dir = dir.path().join("projects").join("test");
        std::fs::create_dir_all(&projects_dir).unwrap();

        let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let jsonl_path = projects_dir.join(format!("{}.jsonl", session_id));

        // Write initial data
        let mut f = std::fs::File::create(&jsonl_path).unwrap();
        writeln!(f, "{}", make_assistant_line("msg1", "claude-opus-4-6", 10, 100, 200, 50)).unwrap();

        let parser = TestParser;
        let s1 = engine.cold_start(&parser, dir.path().to_str().unwrap()).unwrap();
        assert_eq!(s1["claude-opus-4-6"].event_count, 1);

        // Append more data
        let mut f = std::fs::OpenOptions::new().append(true).open(&jsonl_path).unwrap();
        writeln!(f, "{}", make_assistant_line("msg2", "claude-opus-4-6", 20, 200, 400, 100)).unwrap();

        // Second cold start should only process new data
        let s2 = engine.cold_start(&parser, dir.path().to_str().unwrap()).unwrap();
        assert_eq!(s2["claude-opus-4-6"].event_count, 1); // only the new line
        assert_eq!(s2["claude-opus-4-6"].input_tokens, 20);
    }

    #[test]
    fn test_cold_start_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        let mut engine = TrackerEngine::new(db);

        let parser = TestParser;
        let summaries = engine.cold_start(&parser, dir.path().to_str().unwrap()).unwrap();
        assert!(summaries.is_empty());
    }

    #[test]
    fn test_format_number() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(123), "123");
        assert_eq!(format_number(1234), "1,234");
        assert_eq!(format_number(1234567), "1,234,567");
    }
}
