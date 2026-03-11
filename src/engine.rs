use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use crate::checkpoint::{find_resume_offset, process_lines_streaming};
use crate::common::types::{FileCheckpoint, LogParser, LogParserWithTs, ModelUsageSummary, UsageEvent, UsageEventWithTs};
use chrono::{DateTime, Datelike, NaiveDate};
use crate::db::Database;

/// Debug level:
///   0 = off
///   1 = normal debug logging (state transitions, events, timing)
///   2 = level 1 + force cold start (clear DB)
///   3 = level 1 + verbose (size-unchanged, no-new-lines skips)
///   4 = level 2 + verbose (force cold start + all skip logs)
pub fn debug_level() -> u8 {
    std::env::var("CLITRACE_DEBUG").map_or(0, |v| {
        match v.as_str() {
            "true" | "1" => 1,
            "2" => 2,
            "3" => 3,
            "4" => 4,
            _ => 0,
        }
    })
}

macro_rules! debug_log {
    ($($arg:tt)*) => {
        if debug_level() >= 1 {
            eprintln!("[clitrace:debug] {}", format!($($arg)*));
        }
    };
}

/// Verbose debug log — only emitted at level 3+.
/// Used for high-frequency skip events (size unchanged, no new lines).
macro_rules! debug_log_verbose {
    ($($arg:tt)*) => {
        if debug_level() >= 3 {
            eprintln!("[clitrace:debug] {}", format!($($arg)*));
        }
    };
}

/// Cooldown for active files (recently produced new lines).
const ACTIVE_COOLDOWN: Duration = Duration::from_millis(150);
/// Cooldown for idle files (no new lines for a while).
const IDLE_COOLDOWN: Duration = Duration::from_millis(500);
/// Time without new lines before a file transitions Active → Idle.
const IDLE_TRANSITION: Duration = Duration::from_secs(15);

/// Flush interval for dirty checkpoints.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq)]
enum FileState {
    Active,
    Idle,
}

struct FileActivity {
    state: FileState,
    last_active: Instant,
    last_checked: Instant,
}

pub struct TrackerEngine {
    db: Database,
    checkpoints: HashMap<String, FileCheckpoint>,
    /// Cached file sizes for fast skip when size unchanged.
    file_sizes: HashMap<String, u64>,
    /// Per-file activity tracking (state + cooldowns).
    activity: HashMap<String, FileActivity>,
    /// Paths with checkpoints updated since last flush.
    dirty: HashSet<String>,
}

struct ColdStartResult {
    summaries: HashMap<String, ModelUsageSummary>,
    checkpoints: Vec<FileCheckpoint>,
    session_count: usize,
    total_files: usize,
}

fn cold_start_collect(
    parser: &(dyn LogParser + Sync),
    root_dir: &str,
    checkpoints: &HashMap<String, FileCheckpoint>,
) -> Result<ColdStartResult, Box<dyn std::error::Error>> {
    let sessions = parser.discover_sessions(root_dir);

    if sessions.is_empty() {
        return Ok(ColdStartResult {
            summaries: HashMap::new(),
            checkpoints: Vec::new(),
            session_count: 0,
            total_files: 0,
        });
    }

    // Count total files (parent + subagents)
    let total_files: usize = sessions.iter()
        .map(|s| 1 + s.subagent_jsonls.len())
        .sum();

    let max_threads = num_cpus::get();
    let semaphore = Arc::new(Mutex::new(max_threads));

    // Collect all (path, events, last_line_len, last_line_hash) results from parallel processing
    let all_results: Arc<Mutex<Vec<(String, Vec<UsageEvent>, u64, u64)>>> =
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
                let checkpoints = checkpoints;

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

                    if let Ok(Some((events, last_line_len, last_line_hash))) = result {
                        let mut r = results.lock().unwrap();
                        r.push((path_str, events, last_line_len, last_line_hash));
                    }
                });
            }
        }
    });

    // Merge results into summaries and checkpoints
    let mut summaries: HashMap<String, ModelUsageSummary> = HashMap::new();
    let results = Arc::try_unwrap(all_results).unwrap().into_inner().unwrap();

    let mut batch: Vec<FileCheckpoint> = Vec::with_capacity(results.len());
    for (path, events, last_line_len, last_line_hash) in results {
        for event in &events {
            let summary = summaries
                .entry(event.model.clone())
                .or_insert_with(|| ModelUsageSummary {
                    model: event.model.clone(),
                    ..Default::default()
                });
            summary.accumulate(event);
        }

        let cp = FileCheckpoint {
            file_path: path.clone(),
            last_line_len,
            last_line_hash,
        };
        batch.push(cp);
    }

    Ok(ColdStartResult {
        summaries,
        checkpoints: batch,
        session_count: sessions.len(),
        total_files,
    })
}

pub fn cold_start_report(
    parser: &(dyn LogParser + Sync),
    root_dir: &str,
) -> Result<HashMap<String, ModelUsageSummary>, Box<dyn std::error::Error>> {
    let empty = HashMap::new();
    let result = cold_start_collect(parser, root_dir, &empty)?;
    print_summary(&result.summaries);
    Ok(result.summaries)
}

#[derive(Debug, Clone, Copy)]
pub enum ReportGroupBy {
    Day,
    Week,
    Year,
}

pub fn cold_start_report_grouped<P>(
    parser: &P,
    root_dir: &str,
    group_by: ReportGroupBy,
) -> Result<(), Box<dyn std::error::Error>>
where
    P: LogParser + LogParserWithTs + Sync,
{
    let sessions = parser.discover_sessions(root_dir);
    if sessions.is_empty() {
        println!("[clitrace] No usage data found.");
        return Ok(());
    }

    let max_threads = num_cpus::get();
    let semaphore = Arc::new(Mutex::new(max_threads));

    let all_results: Arc<Mutex<Vec<Vec<UsageEventWithTs>>>> =
        Arc::new(Mutex::new(Vec::new()));

    std::thread::scope(|s| {
        for session in &sessions {
            let mut files: Vec<&std::path::Path> = vec![session.parent_jsonl.as_path()];
            for sub in &session.subagent_jsonls {
                files.push(sub.as_path());
            }

            for file_path in files {
                let sem = Arc::clone(&semaphore);
                let results = Arc::clone(&all_results);

                s.spawn(move || {
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
                    let mut events: Vec<UsageEventWithTs> = Vec::new();
                    if let Ok(Some((_bytes, _len, _hash))) = process_lines_streaming(&path_str, 0, |line| {
                        if let Some(event) = parser.parse_line_with_ts(line, &path_str) {
                            events.push(event);
                        }
                    }) {
                        let mut r = results.lock().unwrap();
                        r.push(events);
                    }

                    {
                        let mut count = sem.lock().unwrap();
                        *count += 1;
                    }
                });
            }
        }
    });

    let results = Arc::try_unwrap(all_results).unwrap().into_inner().unwrap();

    let mut grouped: HashMap<String, HashMap<String, ModelUsageSummary>> = HashMap::new();
    for batch in results {
        for event in batch {
            let bucket = match parse_bucket(&event.timestamp, group_by) {
                Some(b) => b,
                None => continue,
            };
            let by_model = grouped.entry(bucket).or_insert_with(HashMap::new);
            let summary = by_model.entry(event.model.clone()).or_insert_with(|| ModelUsageSummary {
                model: event.model.clone(),
                ..Default::default()
            });
            summary.input_tokens += event.input_tokens;
            summary.cache_creation_input_tokens += event.cache_creation_input_tokens;
            summary.cache_read_input_tokens += event.cache_read_input_tokens;
            summary.output_tokens += event.output_tokens;
            summary.event_count += 1;
        }
    }

    if grouped.is_empty() {
        println!("[clitrace] No usage data found.");
        return Ok(());
    }

    let mut buckets: Vec<_> = grouped.keys().cloned().collect();
    buckets.sort();

    for bucket in buckets {
        println!("[clitrace] ═══════════════════════════════════════════");
        println!("[clitrace] Token Usage Summary ({})", bucket);
        if let Some(models) = grouped.get(&bucket) {
            let mut sorted: Vec<_> = models.values().collect();
            sorted.sort_by(|a, b| b.event_count.cmp(&a.event_count));

            for s in &sorted {
                println!("[clitrace] ───────────────────────────────────────────");
                println!("[clitrace] Model: {}", s.model);
                println!(
                    "[clitrace]   Input: {:>12} | Cache Create: {:>12}",
                    format_number(s.input_tokens),
                    format_number(s.cache_creation_input_tokens),
                );
                println!(
                    "[clitrace]   Cache Read: {:>8} | Output: {:>12}",
                    format_number(s.cache_read_input_tokens),
                    format_number(s.output_tokens),
                );
                println!("[clitrace]   Events: {}", s.event_count);
            }
        }
    }

    println!("[clitrace] ═══════════════════════════════════════════");
    Ok(())
}

impl TrackerEngine {
    pub fn new(db: Database) -> Self {
        TrackerEngine {
            db,
            checkpoints: HashMap::new(),
            file_sizes: HashMap::new(),
            activity: HashMap::new(),
            dirty: HashSet::new(),
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
        let t_cold = Instant::now();
        let result = cold_start_collect(parser, root_dir, &self.checkpoints)?;

        if result.session_count == 0 {
            debug_log!("cold_start — 0 sessions, 0 files ({}µs)", t_cold.elapsed().as_micros());
            return Ok(HashMap::new());
        }

        for cp in &result.checkpoints {
            self.checkpoints.insert(cp.file_path.clone(), cp.clone());
        }

        // Print summary
        print_summary(&result.summaries);

        // Batch flush for cold start (single transaction).
        let t_flush = Instant::now();
        self.db.flush_checkpoints(&result.checkpoints)?;
        debug_log!("cold_start — {} sessions, {} files, {} checkpoints flushed (flush: {}µs, total: {}µs)",
            result.session_count, result.total_files, result.checkpoints.len(), t_flush.elapsed().as_micros(), t_cold.elapsed().as_micros());

        Ok(result.summaries)
    }

    /// Process a single file change (watch mode).
    /// Uses active/idle classification to minimize unnecessary work.
    pub fn process_file(
        &mut self,
        path: &str,
        parser: &dyn LogParser,
    ) -> Result<Vec<UsageEvent>, Box<dyn std::error::Error>> {
        let now = Instant::now();

        // Step 1: Determine state (new file = Active, lazy Idle transition)
        let (state, cooldown) = match self.activity.get(path) {
            None => (FileState::Active, ACTIVE_COOLDOWN), // new file
            Some(act) => {
                let mut s = act.state;
                if s == FileState::Active && now.duration_since(act.last_active) > IDLE_TRANSITION {
                    s = FileState::Idle;
                    debug_log!("demote {} → Idle ({}s since last active)",
                        path, now.duration_since(act.last_active).as_secs());
                }
                let cd = if s == FileState::Active { ACTIVE_COOLDOWN } else { IDLE_COOLDOWN };
                (s, cd)
            }
        };

        // Step 2: Cooldown check
        if let Some(act) = self.activity.get(path) {
            if now.duration_since(act.last_checked) < cooldown {
                return Ok(Vec::new());
            }
        }

        let t_total = Instant::now();

        // Step 3: stat() → size change check
        if let Ok(meta) = std::fs::metadata(path) {
            let current_size = meta.len();
            if let Some(&cached_size) = self.file_sizes.get(path) {
                if current_size == cached_size {
                    // Update last_checked, preserve state
                    let act = self.activity.entry(path.to_string()).or_insert(FileActivity {
                        state,
                        last_active: now,
                        last_checked: now,
                    });
                    act.last_checked = now;
                    act.state = state;
                    debug_log_verbose!("process_file {} — size unchanged ({}B), {} ({}µs)",
                        path, current_size,
                        if state == FileState::Active { "Active" } else { "Idle" },
                        t_total.elapsed().as_micros());
                    return Ok(Vec::new());
                }
            }
        }

        // Step 4: Size changed → find_resume + streaming read/parse
        let t0 = Instant::now();
        let offset = self.determine_offset(path)?;
        let find_us = t0.elapsed().as_micros();

        let t1 = Instant::now();
        let mut events = Vec::new();
        let mut line_count: u64 = 0;
        let result = process_lines_streaming(path, offset, |line| {
            if let Some(event) = parser.parse_line(line, path) {
                events.push(event);
            }
            line_count += 1;
        })?;
        let read_us = t1.elapsed().as_micros();

        // Update cached file size
        if let Ok(meta) = std::fs::metadata(path) {
            self.file_sizes.insert(path.to_string(), meta.len());
        }

        match result {
            None => {
                let act = self.activity.entry(path.to_string()).or_insert(FileActivity {
                    state,
                    last_active: now,
                    last_checked: now,
                });
                act.last_checked = now;
                act.state = state;
                debug_log_verbose!("process_file {} — no new lines (find_resume: {}µs, read: {}µs)",
                    path, find_us, read_us);
                return Ok(Vec::new());
            }
            Some((bytes_read, last_line_len, last_line_hash)) => {
                let cp = FileCheckpoint {
                    file_path: path.to_string(),
                    last_line_len,
                    last_line_hash,
                };
                self.checkpoints.insert(path.to_string(), cp);
                self.dirty.insert(path.to_string());

                // Promote to Active if was Idle
                if state == FileState::Idle {
                    debug_log!("promote {} → Active ({} new lines)", path, line_count);
                }
                self.activity.insert(path.to_string(), FileActivity {
                    state: FileState::Active,
                    last_active: now,
                    last_checked: now,
                });

                debug_log!("process_file {} — {} lines, {} bytes, {} events, Active | find_resume: {}µs, read: {}µs, total: {}µs",
                    path, line_count, bytes_read, events.len(),
                    find_us, read_us, t_total.elapsed().as_micros());

                Ok(events)
            }
        }
    }

    fn process_and_print(&mut self, path: &str, parser: &dyn LogParser) {
        match self.process_file(path, parser) {
            Ok(events) => {
                for event in &events {
                    print_event(event);
                }
            }
            Err(e) => {
                eprintln!("[clitrace] Error processing {}: {}", path, e);
            }
        }
    }

    /// Flush dirty checkpoints to DB in a single batch transaction.
    fn flush_dirty(&mut self) {
        if self.dirty.is_empty() {
            return;
        }
        let batch: Vec<FileCheckpoint> = self.dirty.iter()
            .filter_map(|path| self.checkpoints.get(path).cloned())
            .collect();
        let count = batch.len();
        let t = Instant::now();
        if let Err(e) = self.db.flush_checkpoints(&batch) {
            eprintln!("[clitrace] flush error: {}", e);
            return;
        }
        self.dirty.clear();
        debug_log!("flush_dirty — {} checkpoints in {}µs", count, t.elapsed().as_micros());
    }

    fn determine_offset(&self, path: &str) -> Result<u64, Box<dyn std::error::Error>> {
        let cp = match self.checkpoints.get(path) {
            None => return Ok(0),
            Some(cp) => cp,
        };

        match find_resume_offset(path, cp)? {
            Some(offset) => Ok(offset),
            None => Ok(0), // Line not found, full reprocess.
        }
    }

    /// Watch loop: receive file change events, process incrementally,
    /// Watch loop: receive file change events, flush dirty checkpoints periodically.
    /// Graceful shutdown: flushes remaining dirty checkpoints before exiting.
    pub fn watch_loop(
        &mut self,
        event_rx: Receiver<String>,
        stop_rx: Receiver<()>,
        parser: &dyn LogParser,
    ) {
        let flush_tick = crossbeam_channel::tick(FLUSH_INTERVAL);

        loop {
            crossbeam_channel::select! {
                recv(stop_rx) -> _ => {
                    self.flush_dirty();
                    break;
                }
                recv(event_rx) -> msg => {
                    match msg {
                        Ok(path) => {
                            // Drain any queued events (dedup by path).
                            let mut paths = HashSet::new();
                            paths.insert(path);
                            while let Ok(more) = event_rx.try_recv() {
                                paths.insert(more);
                            }
                            debug_log!("watch event — {} unique paths queued", paths.len());

                            for path in paths {
                                self.process_and_print(&path, parser);
                            }
                        }
                        Err(_) => {
                            self.flush_dirty();
                            break;
                        }
                    }
                }
                recv(flush_tick) -> _ => {
                    self.flush_dirty();
                }
            }
        }
    }
}

/// Process a single file during cold start (called from scoped threads).
/// Uses streaming line reader to avoid holding all lines in memory.
/// Returns (events, last_line_len, last_line_hash) or None if no new data.
fn process_file_cold(
    path: &str,
    parser: &(dyn LogParser + Sync),
    checkpoints: &HashMap<String, FileCheckpoint>,
) -> Result<Option<(Vec<UsageEvent>, u64, u64)>, Box<dyn std::error::Error>> {
    // Determine offset
    let offset = match checkpoints.get(path) {
        None => 0,
        Some(cp) => find_resume_offset(path, cp)
            .unwrap_or(None)
            .unwrap_or(0),
    };

    let mut events = Vec::new();
    let result = process_lines_streaming(path, offset, |line| {
        if let Some(event) = parser.parse_line(line, path) {
            events.push(event);
        }
    })?;

    match result {
        Some((_bytes, last_line_len, last_line_hash)) => {
            Ok(Some((events, last_line_len, last_line_hash)))
        }
        None => Ok(None),
    }
}

/// Print cold start summary.
pub fn print_summary(summaries: &HashMap<String, ModelUsageSummary>) {
    if summaries.is_empty() {
        println!("[clitrace] No usage data found.");
        return;
    }

    println!("[clitrace] ═══════════════════════════════════════════");
    println!("[clitrace] Token Usage Summary");

    let mut sorted: Vec<_> = summaries.values().collect();
    sorted.sort_by(|a, b| b.event_count.cmp(&a.event_count));

    for s in &sorted {
        println!("[clitrace] ───────────────────────────────────────────");
        println!("[clitrace] Model: {}", s.model);
        println!(
            "[clitrace]   Input: {:>12} | Cache Create: {:>12}",
            format_number(s.input_tokens),
            format_number(s.cache_creation_input_tokens),
        );
        println!(
            "[clitrace]   Cache Read: {:>8} | Output: {:>12}",
            format_number(s.cache_read_input_tokens),
            format_number(s.output_tokens),
        );
        println!("[clitrace]   Events: {}", s.event_count);
    }
    println!("[clitrace] ═══════════════════════════════════════════");
}

/// Print a single watch-mode event.
pub fn print_event(event: &UsageEvent) {
    let label = format_source_label(&event.source_file);
    println!(
        "[clitrace] {} | {} | in:{} cc:{} cr:{} out:{}",
        event.model,
        label,
        event.input_tokens,
        event.cache_creation_input_tokens,
        event.cache_read_input_tokens,
        event.output_tokens,
    );
}

/// Extract a human-readable label from a source file path.
///   Parent:   .../projects/<dir>/<UUID>.jsonl        → "<UUID short>"
///   Subagent: .../<UUID>/subagents/agent-<id>.jsonl  → "<UUID short>/agent-<id short>"
fn format_source_label(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').collect();
    // parts[0] = filename, parts[1] = parent dir, parts[2] = grandparent, ...

    let filename = parts.first().map_or("", |s| s.trim_end_matches(".jsonl"));

    // Subagent: parts = ["agent-xxx.jsonl", "subagents", "<UUID>", ...]
    if parts.len() >= 3 && parts[1] == "subagents" {
        let session_id = shorten_id(parts[2]);
        let agent_id = shorten_id(filename);
        return format!("{}/{}", session_id, agent_id);
    }

    // Parent session
    shorten_id(filename).to_string()
}

/// Shorten a UUID or agent ID to first 8 chars.
fn shorten_id(id: &str) -> &str {
    if id.len() > 8 { &id[..8] } else { id }
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

fn parse_bucket(ts: &str, group_by: ReportGroupBy) -> Option<String> {
    let date = if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
        dt.date_naive()
    } else {
        NaiveDate::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ").ok()?
    };
    match group_by {
        ReportGroupBy::Day => Some(date.format("%Y-%m-%d").to_string()),
        ReportGroupBy::Year => Some(format!("{:04}", date.year())),
        ReportGroupBy::Week => {
            let (iso_year, iso_week) = iso_week(date);
            Some(format!("{:04}-W{:02}", iso_year, iso_week))
        }
    }
}

fn iso_week(date: NaiveDate) -> (i32, u32) {
    let year = date.year();
    let jan4 = NaiveDate::from_ymd_opt(year, 1, 4).unwrap();
    let jan4_weekday = jan4.weekday().number_from_monday() as i32;
    let week1_start = jan4 - chrono::Duration::days((jan4_weekday - 1) as i64);

    let date_weekday = date.weekday().number_from_monday() as i32;
    let date_week_start = date - chrono::Duration::days((date_weekday - 1) as i64);

    if date_week_start < week1_start {
        return iso_week(NaiveDate::from_ymd_opt(year - 1, 12, 31).unwrap());
    }

    let days = date_week_start.signed_duration_since(week1_start).num_days();
    let mut week = (days / 7 + 1) as u32;
    let mut iso_year = year;

    if week == 53 {
        let jan4_next = NaiveDate::from_ymd_opt(year + 1, 1, 4).unwrap();
        let jan4_next_weekday = jan4_next.weekday().number_from_monday() as i32;
        let week1_next_start = jan4_next - chrono::Duration::days((jan4_next_weekday - 1) as i64);
        if date_week_start >= week1_next_start {
            iso_year = year + 1;
            week = 1;
        }
    }

    (iso_year, week)
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
        assert!(engine.checkpoints[path_str].last_line_len > 0);
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

    #[test]
    #[ignore] // Run with: cargo test bench_timing -- --ignored --nocapture
    fn bench_timing() {
        use crate::checkpoint::{find_resume_offset, hash_line, process_lines_streaming};
        use std::time::Instant;

        let dir = tempfile::tempdir().unwrap();
        let iterations = 200;

        // Create 500-line JSONL file (~realistic session)
        let projects_dir = dir.path().join("projects").join("test");
        std::fs::create_dir_all(&projects_dir).unwrap();
        let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let jsonl_path = projects_dir.join(format!("{}.jsonl", session_id));
        {
            let mut f = std::fs::File::create(&jsonl_path).unwrap();
            for i in 0..500 {
                writeln!(f, "{}", make_assistant_line(
                    &format!("msg_{}", i), "claude-opus-4-6",
                    i * 10, i * 100, i * 50, i * 5
                )).unwrap();
            }
        }

        let path_str = jsonl_path.to_str().unwrap();
        let file_size = std::fs::metadata(path_str).unwrap().len();

        // Build checkpoint on line 490
        let content = std::fs::read_to_string(path_str).unwrap();
        let line_490: Vec<&str> = content.lines().collect();
        let target = line_490[490];
        let cp = FileCheckpoint {
            file_path: path_str.to_string(),
            last_line_len: target.len() as u64,
            last_line_hash: hash_line(target.as_bytes()),
        };

        // Bench find_resume_offset
        let start = Instant::now();
        for _ in 0..iterations {
            let _ = find_resume_offset(path_str, &cp).unwrap();
        }
        let find_us = start.elapsed().as_micros() / iterations as u128;

        // Bench process_lines_streaming (last 10 lines)
        let offset = find_resume_offset(path_str, &cp).unwrap().unwrap();
        let start = Instant::now();
        for _ in 0..iterations {
            let _ = process_lines_streaming(path_str, offset, |_| {}).unwrap();
        }
        let read_us = start.elapsed().as_micros() / iterations as u128;

        // Bench DB upsert
        let db_path = dir.path().join("bench.db");
        let db = Database::open(&db_path).unwrap();
        let start = Instant::now();
        for _ in 0..iterations {
            db.upsert_checkpoint(&cp).unwrap();
        }
        let db_us = start.elapsed().as_micros() / iterations as u128;

        // Bench full process_file (cold start + incremental)
        let db2 = Database::open(&dir.path().join("bench2.db")).unwrap();
        let mut engine = TrackerEngine::new(db2);
        let parser = TestParser;

        let start = Instant::now();
        engine.cold_start(&parser, dir.path().to_str().unwrap()).unwrap();
        let cold_us = start.elapsed().as_micros();

        // Append 10 lines, measure incremental
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&jsonl_path).unwrap();
            for i in 500..510 {
                writeln!(f, "{}", make_assistant_line(
                    &format!("msg_{}", i), "claude-opus-4-6",
                    i * 10, 0, 0, i * 5
                )).unwrap();
            }
        }
        let start = Instant::now();
        let events = engine.process_file(path_str, &parser).unwrap();
        let incr_us = start.elapsed().as_micros();

        println!("\n=== clitrace benchmark ===");
        println!("File: {} lines, {} bytes ({} KB)", 500, file_size, file_size / 1024);
        println!();
        println!("Per-operation (avg of {} runs):", iterations);
        println!("  find_resume_offset:        {:>6}µs", find_us);
        println!("  process_lines_streaming:   {:>6}µs", read_us);
        println!("  db.upsert_checkpoint:      {:>6}µs", db_us);
        println!();
        println!("End-to-end:");
        println!("  cold_start (500 lines):    {:>6}µs", cold_us);
        println!("  process_file (10 new):     {:>6}µs  ({} events)", incr_us, events.len());
    }
}
