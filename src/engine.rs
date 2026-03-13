use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

use crate::checkpoint::{find_resume_offset, process_lines_streaming};
use crate::common::types::{FileCheckpoint, LogParser, LogParserWithTs, ModelUsageSummary, TokenFields};
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeZone, Weekday};
use chrono_tz::Tz;
use crate::pricing::PricingTable;
use crate::sink::Sink;
use crate::writer::DbOp;

/// Debug level:
///   0 = off
///   1 = normal debug logging (state transitions, events, timing)
///   2 = level 1 + verbose (size-unchanged, no-new-lines skips)
pub fn debug_level() -> u8 {
    static LEVEL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| {
        std::env::var("TOKI_DEBUG").map_or(0, |v| match v.as_str() {
            "true" | "1" => 1,
            "2" => 2,
            _ => 0,
        })
    })
}

macro_rules! debug_log {
    ($($arg:tt)*) => {
        if debug_level() >= 1 {
            eprintln!("[toki:debug] {}", format!($($arg)*));
        }
    };
}

/// Verbose debug log — only emitted at level 2.
/// Used for high-frequency skip events (size unchanged, no new lines).
macro_rules! debug_log_verbose {
    ($($arg:tt)*) => {
        if debug_level() >= 2 {
            eprintln!("[toki:debug] {}", format!($($arg)*));
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
    db_tx: Sender<DbOp>,
    checkpoints: HashMap<String, FileCheckpoint>,
    /// Cached file sizes for fast skip when size unchanged.
    file_sizes: HashMap<String, u64>,
    /// Per-file activity tracking (state + cooldowns).
    activity: HashMap<String, FileActivity>,
    /// Paths with checkpoints updated since last flush.
    dirty: HashSet<String>,
    /// Output sink (print, UDS, HTTP, or multi).
    sink: Box<dyn Sink>,
    /// Optional session ID prefix filter for trace mode.
    session_filter: Option<String>,
    /// Optional project name filter for trace mode.
    project_filter: Option<String>,
    /// Optional timezone for bucketing in startup grouping.
    tz: Option<Tz>,
    /// Pricing table for cost calculation.
    pricing: Option<PricingTable>,
}

#[derive(Debug, Clone, Copy)]
pub enum ReportGroupBy {
    Date,
    Week { start_of_week: Weekday },
    Month,
    Year,
    Hour,
}

impl ReportGroupBy {
    pub fn type_name(&self) -> &'static str {
        match self {
            ReportGroupBy::Date => "daily",
            ReportGroupBy::Week { .. } => "weekly",
            ReportGroupBy::Month => "monthly",
            ReportGroupBy::Year => "yearly",
            ReportGroupBy::Hour => "hourly",
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[derive(Default)]
pub struct ReportFilter {
    pub since: Option<NaiveDateTime>,
    pub until: Option<NaiveDateTime>,
    /// Timezone for bucketing and display. None = UTC.
    pub tz: Option<Tz>,
}


impl TrackerEngine {
    pub fn new(db_tx: Sender<DbOp>, checkpoints: HashMap<String, FileCheckpoint>) -> Self {
        TrackerEngine {
            db_tx,
            checkpoints,
            file_sizes: HashMap::new(),
            activity: HashMap::new(),
            dirty: HashSet::new(),
            sink: Box::new(crate::sink::PrintSink::new(crate::sink::OutputFormat::Table)),
            session_filter: None,
            project_filter: None,
            tz: None,
            pricing: None,
        }
    }

    pub fn with_sink(mut self, sink: Box<dyn Sink>) -> Self {
        self.sink = sink;
        self
    }

    pub fn with_session_filter(mut self, filter: Option<String>) -> Self {
        self.session_filter = filter;
        self
    }

    pub fn with_project_filter(mut self, filter: Option<String>) -> Self {
        self.project_filter = filter;
        self
    }

    pub fn with_tz(mut self, tz: Option<Tz>) -> Self {
        self.tz = tz;
        self
    }

    pub fn with_pricing(mut self, pricing: Option<PricingTable>) -> Self {
        self.pricing = pricing;
        self
    }

    /// Check if a file path matches the configured session and project filters.
    fn matches_filters(&self, path: &str) -> bool {
        if let Some(ref prefix) = self.session_filter {
            if let Some(sid) = extract_session_id(path) {
                if !sid.starts_with(prefix.as_str()) {
                    return false;
                }
            }
        }
        if let Some(ref project) = self.project_filter {
            match extract_project_name(path) {
                Some(name) if name.contains(project.as_str()) => {}
                _ => return false,
            }
        }
        true
    }

    /// Cold start: discover all sessions, process them in parallel,
    /// aggregate by model, print summary, flush checkpoints, and populate TSDB.
    pub fn cold_start<P>(
        &mut self,
        parser: &P,
        root_dir: &str,
    ) -> Result<HashMap<String, ModelUsageSummary>, Box<dyn std::error::Error>>
    where
        P: LogParser + LogParserWithTs + Sync,
    {
        let t_cold = Instant::now();
        let sessions = apply_filters(parser.discover_sessions(root_dir), None, self.project_filter.as_deref());

        if sessions.is_empty() {
            debug_log!("cold_start — 0 sessions, 0 files ({}µs)", t_cold.elapsed().as_micros());
            return Ok(HashMap::new());
        }

        let total_files: usize = sessions.iter().map(|s| 1 + s.subagent_jsonls.len()).sum();
        let summaries: Mutex<HashMap<String, ModelUsageSummary>> = Mutex::new(HashMap::new());
        let cp_batch: Mutex<Vec<FileCheckpoint>> = Mutex::new(Vec::new());
        let db_tx = &self.db_tx;

        parallel_scan(&sessions, &self.checkpoints, |path, offset| {
            let mut local: HashMap<String, ModelUsageSummary> = HashMap::new();
            let session_id = extract_session_id(path).unwrap_or_default();
            let result = process_lines_streaming(path, offset, |line| {
                if let Some(event) = parser.parse_line_with_ts(line, path) {
                    let summary = local.entry(event.model.clone()).or_insert_with(|| ModelUsageSummary {
                        model: event.model.clone(),
                        ..Default::default()
                    });
                    summary.accumulate_with_ts(&event);

                    // Send event to TSDB writer
                    let ts_ms = parse_timestamp(&event.timestamp)
                        .map(|dt| dt.and_utc().timestamp_millis())
                        .unwrap_or(0);
                    if ts_ms > 0 {
                        let op = DbOp::WriteEvent {
                            ts_ms,
                            message_id: event.event_key.clone(),
                            model: event.model.clone(),
                            session_id: session_id.clone(),
                            source_file: event.source_file.clone(),
                            tokens: TokenFields {
                                input_tokens: event.input_tokens,
                                output_tokens: event.output_tokens,
                                cache_creation_input_tokens: event.cache_creation_input_tokens,
                                cache_read_input_tokens: event.cache_read_input_tokens,
                            },
                        };
                        let _ = db_tx.send(op);
                    }
                }
            });
            if !local.is_empty() {
                let mut s = summaries.lock().unwrap_or_else(|e| e.into_inner());
                merge_summaries(&mut s, local);
            }
            if let Ok(Some((_bytes, last_line_len, last_line_hash))) = result {
                cp_batch.lock().unwrap_or_else(|e| e.into_inner()).push(FileCheckpoint {
                    file_path: path.to_string(),
                    last_line_len,
                    last_line_hash,
                });
            }
        });

        let result_summaries = summaries.into_inner().unwrap_or_else(|e| e.into_inner());
        let checkpoints_batch = cp_batch.into_inner().unwrap_or_else(|e| e.into_inner());

        for cp in &checkpoints_batch {
            self.checkpoints.insert(cp.file_path.clone(), cp.clone());
        }

        // Emit summary
        self.sink.emit_summary(&result_summaries, self.pricing.as_ref());

        // Batch flush for cold start via writer thread.
        let cp_count = checkpoints_batch.len();
        if !checkpoints_batch.is_empty() {
            let _ = self.db_tx.send(DbOp::FlushCheckpoints(checkpoints_batch));
        }
        debug_log!("cold_start — {} sessions, {} files, {} checkpoints flushed (total: {}µs)",
            sessions.len(), total_files, cp_count, t_cold.elapsed().as_micros());

        Ok(result_summaries)
    }

    /// Cold start with time-bucket grouping (e.g. hourly/daily).
    /// Streams events into buckets, saves checkpoints, and prints grouped summary.
    pub fn cold_start_grouped<P>(
        &mut self,
        parser: &P,
        root_dir: &str,
        group_by: ReportGroupBy,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        P: LogParser + LogParserWithTs + Sync,
    {
        let t_cold = Instant::now();
        let sessions = parser.discover_sessions(root_dir);
        if sessions.is_empty() {
            self.sink.emit_grouped(&HashMap::new(), group_by.type_name(), self.pricing.as_ref());
            debug_log!("cold_start_grouped — 0 sessions ({}µs)", t_cold.elapsed().as_micros());
            return Ok(());
        }

        let grouped: Mutex<HashMap<String, HashMap<String, ModelUsageSummary>>> =
            Mutex::new(HashMap::new());
        let cp_batch: Mutex<Vec<FileCheckpoint>> = Mutex::new(Vec::new());
        let filter = ReportFilter { tz: self.tz, ..Default::default() };

        let db_tx = &self.db_tx;

        parallel_scan(&sessions, &self.checkpoints, |path, offset| {
            let mut local: HashMap<String, HashMap<String, ModelUsageSummary>> = HashMap::new();
            let session_id = extract_session_id(path).unwrap_or_default();
            let result = process_lines_streaming(path, offset, |line| {
                if let Some(event) = parser.parse_line_with_ts(line, path) {
                    if let Some(bucket) = bucket_from_timestamp_filtered(&event.timestamp, group_by, filter) {
                        let by_model = local.entry(bucket).or_default();
                        let summary = by_model.entry(event.model.clone()).or_insert_with(|| ModelUsageSummary {
                            model: event.model.clone(),
                            ..Default::default()
                        });
                        summary.accumulate_with_ts(&event);
                    }

                    // Send event to TSDB writer
                    let ts_ms = parse_timestamp(&event.timestamp)
                        .map(|dt| dt.and_utc().timestamp_millis())
                        .unwrap_or(0);
                    if ts_ms > 0 {
                        let op = DbOp::WriteEvent {
                            ts_ms,
                            message_id: event.event_key.clone(),
                            model: event.model.clone(),
                            session_id: session_id.clone(),
                            source_file: event.source_file.clone(),
                            tokens: TokenFields {
                                input_tokens: event.input_tokens,
                                output_tokens: event.output_tokens,
                                cache_creation_input_tokens: event.cache_creation_input_tokens,
                                cache_read_input_tokens: event.cache_read_input_tokens,
                            },
                        };
                        let _ = db_tx.send(op);
                    }
                }
            });
            if !local.is_empty() {
                let mut g = grouped.lock().unwrap_or_else(|e| e.into_inner());
                merge_grouped(&mut g, local);
            }
            if let Ok(Some((_bytes, last_line_len, last_line_hash))) = result {
                cp_batch.lock().unwrap_or_else(|e| e.into_inner()).push(FileCheckpoint {
                    file_path: path.to_string(),
                    last_line_len,
                    last_line_hash,
                });
            }
        });

        // Save checkpoints
        let batch = cp_batch.into_inner().unwrap_or_else(|e| e.into_inner());
        if !batch.is_empty() {
            for cp in &batch {
                self.checkpoints.insert(cp.file_path.clone(), cp.clone());
            }
            let _ = self.db_tx.send(DbOp::FlushCheckpoints(batch));
        }

        self.sink.emit_grouped(&grouped.into_inner().unwrap_or_else(|e| e.into_inner()), group_by.type_name(), self.pricing.as_ref());
        debug_log!("cold_start_grouped — ({}µs)", t_cold.elapsed().as_micros());
        Ok(())
    }

    /// Process a single file change (watch mode).
    /// Uses active/idle classification to minimize unnecessary work.
    /// Returns events with timestamps for TSDB storage.
    pub fn process_file_with_ts<P>(
        &mut self,
        path: &str,
        parser: &P,
    ) -> Result<Vec<crate::common::types::UsageEventWithTs>, Box<dyn std::error::Error>>
    where
        P: LogParser + LogParserWithTs,
    {
        let now = Instant::now();

        let state = match self.activity.get(path) {
            None => FileState::Active,
            Some(act) => {
                let mut s = act.state;
                if s == FileState::Active && now.duration_since(act.last_active) > IDLE_TRANSITION {
                    s = FileState::Idle;
                    debug_log!("demote {} → Idle ({}s since last active)",
                        path, now.duration_since(act.last_active).as_secs());
                }
                let cd = if s == FileState::Active { ACTIVE_COOLDOWN } else { IDLE_COOLDOWN };
                // Cooldown check: skip if checked too recently
                if now.duration_since(act.last_checked) < cd {
                    return Ok(Vec::new());
                }
                s
            }
        };

        let t_total = Instant::now();

        if let Ok(meta) = std::fs::metadata(path) {
            let current_size = meta.len();
            if let Some(&cached_size) = self.file_sizes.get(path) {
                if current_size == cached_size {
                    let act = self.activity.entry(path.to_string()).or_insert(FileActivity {
                        state, last_active: now, last_checked: now,
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

        let t0 = Instant::now();
        let offset = self.determine_offset(path)?;
        let find_us = t0.elapsed().as_micros();

        let t1 = Instant::now();
        let mut events = Vec::new();
        let mut line_count: u64 = 0;
        let result = process_lines_streaming(path, offset, |line| {
            if let Some(event) = parser.parse_line_with_ts(line, path) {
                events.push(event);
            }
            line_count += 1;
        })?;
        let read_us = t1.elapsed().as_micros();

        match result {
            None => {
                if let Ok(meta) = std::fs::metadata(path) {
                    self.file_sizes.insert(path.to_string(), meta.len());
                }
                let act = self.activity.entry(path.to_string()).or_insert(FileActivity {
                    state, last_active: now, last_checked: now,
                });
                act.last_checked = now;
                act.state = state;
                debug_log_verbose!("process_file {} — no new lines (find_resume: {}µs, read: {}µs)",
                    path, find_us, read_us);
                Ok(Vec::new())
            }
            Some((bytes_read, last_line_len, last_line_hash)) => {
                self.file_sizes.insert(path.to_string(), offset + bytes_read);
                let cp = FileCheckpoint {
                    file_path: path.to_string(),
                    last_line_len,
                    last_line_hash,
                };
                self.checkpoints.insert(path.to_string(), cp);
                self.dirty.insert(path.to_string());
                if state == FileState::Idle {
                    debug_log!("promote {} → Active ({} new lines)", path, line_count);
                }
                self.activity.insert(path.to_string(), FileActivity {
                    state: FileState::Active, last_active: now, last_checked: now,
                });
                debug_log!("process_file {} — {} lines, {} bytes, {} events, Active | find_resume: {}µs, read: {}µs, total: {}µs",
                    path, line_count, bytes_read, events.len(),
                    find_us, read_us, t_total.elapsed().as_micros());
                Ok(events)
            }
        }
    }

    fn process_and_print<P>(&mut self, path: &str, parser: &P)
    where
        P: LogParser + LogParserWithTs,
    {
        if !self.matches_filters(path) {
            return;
        }
        match self.process_file_with_ts(path, parser) {
            Ok(events) => {
                let session_id = extract_session_id(path).unwrap_or_default();
                for event in events {
                    let ts_ms = parse_timestamp(&event.timestamp)
                        .map(|dt| dt.and_utc().timestamp_millis())
                        .unwrap_or_else(|| {
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH).unwrap()
                                .as_millis() as i64
                        });

                    let (usage, _ts) = event.into_usage_event();
                    self.sink.emit_event(&usage, self.pricing.as_ref());

                    let op = DbOp::WriteEvent {
                        ts_ms,
                        message_id: usage.event_key,
                        model: usage.model,
                        session_id: session_id.clone(),
                        source_file: usage.source_file,
                        tokens: TokenFields {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cache_creation_input_tokens: usage.cache_creation_input_tokens,
                            cache_read_input_tokens: usage.cache_read_input_tokens,
                        },
                    };
                    if self.db_tx.try_send(op).is_err() {
                        debug_log!("writer channel full, dropping event");
                    }
                }
            }
            Err(e) => {
                eprintln!("[toki] Error processing {}: {}", path, e);
            }
        }
    }

    /// Flush dirty checkpoints via writer thread.
    fn flush_dirty(&mut self) {
        if self.dirty.is_empty() {
            return;
        }
        let batch: Vec<FileCheckpoint> = self.dirty.iter()
            .filter_map(|path| self.checkpoints.get(path).cloned())
            .collect();
        let count = batch.len();
        let _ = self.db_tx.send(DbOp::FlushCheckpoints(batch));
        self.dirty.clear();
        debug_log!("flush_dirty — {} checkpoints sent to writer", count);
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
    /// flush dirty checkpoints periodically.
    /// Graceful shutdown: flushes remaining dirty checkpoints before exiting.
    pub fn watch_loop<P>(
        &mut self,
        event_rx: Receiver<String>,
        stop_rx: Receiver<()>,
        parser: &P,
    )
    where
        P: LogParser + LogParserWithTs,
    {
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

fn resolve_offset(path: &str, checkpoints: &HashMap<String, FileCheckpoint>) -> u64 {
    match checkpoints.get(path) {
        None => 0,
        Some(cp) => find_resume_offset(path, cp).ok().flatten().unwrap_or(0),
    }
}

/// Merge thread-local summaries into global map.
fn merge_summaries(
    global: &mut HashMap<String, ModelUsageSummary>,
    local: HashMap<String, ModelUsageSummary>,
) {
    for (model, ls) in local {
        let gs = global.entry(model.clone()).or_insert_with(|| ModelUsageSummary {
            model,
            ..Default::default()
        });
        gs.input_tokens = gs.input_tokens.saturating_add(ls.input_tokens);
        gs.cache_creation_input_tokens = gs.cache_creation_input_tokens.saturating_add(ls.cache_creation_input_tokens);
        gs.cache_read_input_tokens = gs.cache_read_input_tokens.saturating_add(ls.cache_read_input_tokens);
        gs.output_tokens = gs.output_tokens.saturating_add(ls.output_tokens);
        gs.event_count = gs.event_count.saturating_add(ls.event_count);
    }
}

/// Merge thread-local grouped summaries into global grouped map.
fn merge_grouped(
    global: &mut HashMap<String, HashMap<String, ModelUsageSummary>>,
    local: HashMap<String, HashMap<String, ModelUsageSummary>>,
) {
    for (bucket, local_models) in local {
        let global_models = global.entry(bucket).or_default();
        merge_summaries(global_models, local_models);
    }
}

/// Common parallel file scan over all sessions.
/// Resolves checkpoint offsets and runs `on_file(path, offset)` in parallel
/// using rayon's thread pool (bounded worker threads, work stealing).
fn parallel_scan<F>(
    sessions: &[crate::common::types::SessionGroup],
    checkpoints: &HashMap<String, FileCheckpoint>,
    on_file: F,
)
where
    F: Fn(&str, u64) + Sync,
{
    use rayon::prelude::*;

    let all_files: Vec<String> = sessions
        .iter()
        .flat_map(|session| {
            let mut files = vec![session.parent_jsonl.to_string_lossy().to_string()];
            for sub in &session.subagent_jsonls {
                files.push(sub.to_string_lossy().to_string());
            }
            files
        })
        .collect();

    all_files.par_iter().for_each(|path| {
        let offset = resolve_offset(path, checkpoints);
        on_file(path, offset);
    });
}

fn filter_active(filter: ReportFilter) -> bool {
    filter.since.is_some() || filter.until.is_some()
}

fn filter_match(ts: NaiveDateTime, filter: ReportFilter) -> bool {
    if let Some(since) = filter.since {
        if ts < since {
            return false;
        }
    }
    if let Some(until) = filter.until {
        if ts > until {
            return false;
        }
    }
    true
}

/// Extract the full session UUID from a file path.
///   Parent:   .../projects/<dir>/<UUID>.jsonl        → "<UUID>"
///   Subagent: .../<UUID>/subagents/agent-<id>.jsonl  → "<UUID>" (grandparent dir name)
pub fn extract_session_id(path: &str) -> Option<String> {
    let mut parts = path.rsplit('/');
    let filename = parts.next()?;
    // Subagent: .../\<UUID>/subagents/agent-xxx.jsonl
    if let Some(dir) = parts.next() {
        if dir == "subagents" {
            return parts.next().map(|s| s.to_string());
        }
    }
    // Parent: filename without .jsonl
    Some(filename.trim_end_matches(".jsonl").to_string())
}

/// Filter sessions by session_id prefix match.
pub fn filter_sessions_by_id(sessions: Vec<crate::common::types::SessionGroup>, prefix: &str) -> Vec<crate::common::types::SessionGroup> {
    sessions.into_iter()
        .filter(|s| s.session_id.starts_with(prefix))
        .collect()
}

/// Extract project directory name from a file path (zero-alloc, returns &str slice).
///   .../projects/<PROJECT_DIR>/<UUID>.jsonl → "<PROJECT_DIR>"
///   .../projects/<PROJECT_DIR>/<UUID>/subagents/agent-<id>.jsonl → "<PROJECT_DIR>"
pub fn extract_project_name(path: &str) -> Option<&str> {
    let marker = "/projects/";
    let start = path.find(marker)? + marker.len();
    let rest = &path[start..];
    let end = rest.find('/').unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Apply session and project filters to a list of sessions.
fn apply_filters(
    mut sessions: Vec<crate::common::types::SessionGroup>,
    session_filter: Option<&str>,
    project_filter: Option<&str>,
) -> Vec<crate::common::types::SessionGroup> {
    if let Some(prefix) = session_filter {
        sessions = filter_sessions_by_id(sessions, prefix);
    }
    if let Some(project) = project_filter {
        sessions.retain(|s| {
            let path_str = s.parent_jsonl.to_string_lossy();
            matches!(extract_project_name(&path_str), Some(name) if name.contains(project))
        });
    }
    sessions
}

fn bucket_from_timestamp(ts: &str, group_by: ReportGroupBy, tz: Option<Tz>) -> Option<String> {
    // Fast path: string slicing only valid for UTC (no timezone conversion needed)
    if tz.is_none() && ts.len() >= 4 {
        match group_by {
            ReportGroupBy::Year => return Some(ts[0..4].to_string()),
            ReportGroupBy::Month if ts.len() >= 7 => return Some(ts[0..7].to_string()),
            ReportGroupBy::Date if ts.len() >= 10 => return Some(ts[0..10].to_string()),
            ReportGroupBy::Hour if ts.len() >= 13 => {
                let hour = &ts[0..13];
                return Some(format!("{}:00", hour));
            }
            ReportGroupBy::Week { .. } => {}
            _ => {}
        }
    }

    let dt = parse_timestamp_with_tz(ts, tz)?;
    Some(bucket_from_datetime(dt, group_by))
}

fn bucket_from_timestamp_filtered(
    ts: &str,
    group_by: ReportGroupBy,
    filter: ReportFilter,
) -> Option<String> {
    if filter_active(filter) {
        let utc = parse_timestamp(ts)?;
        if !filter_match(utc, filter) {
            return None;
        }
        let local = apply_tz(utc, filter.tz);
        return Some(bucket_from_datetime(local, group_by));
    }
    bucket_from_timestamp(ts, group_by, filter.tz)
}

fn parse_timestamp(ts: &str) -> Option<NaiveDateTime> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
        return Some(dt.naive_utc());
    }
    NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ").ok()
}

/// Parse timestamp and convert to target timezone (or UTC if None).
fn parse_timestamp_with_tz(ts: &str, tz: Option<Tz>) -> Option<NaiveDateTime> {
    let utc = parse_timestamp(ts)?;
    Some(apply_tz(utc, tz))
}

/// Convert UTC NaiveDateTime to target timezone. Noop if tz is None.
fn apply_tz(utc: NaiveDateTime, tz: Option<Tz>) -> NaiveDateTime {
    match tz {
        Some(tz) => chrono::Utc.from_utc_datetime(&utc).with_timezone(&tz).naive_local(),
        None => utc,
    }
}

fn bucket_from_datetime(ts: NaiveDateTime, group_by: ReportGroupBy) -> String {
    let date = ts.date();
    match group_by {
        ReportGroupBy::Date => date.format("%Y-%m-%d").to_string(),
        ReportGroupBy::Week { start_of_week } => {
            let (week_year, week) = week_bucket(date, start_of_week);
            format!("{:04}-W{:02}", week_year, week)
        }
        ReportGroupBy::Month => date.format("%Y-%m").to_string(),
        ReportGroupBy::Year => format!("{:04}", date.year()),
        ReportGroupBy::Hour => ts.format("%Y-%m-%dT%H:00").to_string(),
    }
}

fn week_bucket(date: NaiveDate, start_of_week: Weekday) -> (i32, u32) {
    let date_week_start = week_start(date, start_of_week);
    let mut year = date_week_start.year();

    let first_start = first_week_start(year, start_of_week);
    if date_week_start < first_start {
        year -= 1;
    }
    let first_start = first_week_start(year, start_of_week);
    let days = date_week_start.signed_duration_since(first_start).num_days();
    let week = (days / 7 + 1) as u32;
    (year, week)
}

fn week_start(date: NaiveDate, start_of_week: Weekday) -> NaiveDate {
    let date_idx = weekday_index(date.weekday());
    let start_idx = weekday_index(start_of_week);
    let delta = (7 + date_idx - start_idx) % 7;
    date - chrono::Duration::days(delta as i64)
}

fn first_week_start(year: i32, start_of_week: Weekday) -> NaiveDate {
    let jan1 = NaiveDate::from_ymd_opt(year, 1, 1).unwrap();
    let delta = (weekday_index(start_of_week) - weekday_index(jan1.weekday()) + 7) % 7;
    jan1 + chrono::Duration::days(delta as i64)
}

fn weekday_index(day: Weekday) -> i32 {
    match day {
        Weekday::Mon => 0,
        Weekday::Tue => 1,
        Weekday::Wed => 2,
        Weekday::Thu => 3,
        Weekday::Fri => 4,
        Weekday::Sat => 5,
        Weekday::Sun => 6,
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::types::{SessionGroup, UsageEvent};
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
                    let Some(parent_dir) = entry.parent() else { continue };
                    let sub_dir = parent_dir.join(stem).join("subagents");
                    let subs = if sub_dir.is_dir() {
                        let Some(pattern) = sub_dir.join("agent-*.jsonl").to_str().map(|s| s.to_string()) else { continue };
                        glob::glob(&pattern)
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

    impl LogParserWithTs for TestParser {
        fn parse_line_with_ts(&self, line: &str, source_file: &str) -> Option<crate::common::types::UsageEventWithTs> {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            if v.get("type")?.as_str()? != "assistant" {
                return None;
            }
            let msg = v.get("message")?;
            let usage = msg.get("usage")?;
            let ts = v.get("timestamp")?.as_str()?.to_string();
            Some(crate::common::types::UsageEventWithTs {
                event_key: format!("{}:{}", msg.get("id")?.as_str()?, &ts),
                source_file: source_file.to_string(),
                model: msg.get("model")?.as_str()?.to_string(),
                input_tokens: usage.get("input_tokens")?.as_u64()?,
                cache_creation_input_tokens: usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                cache_read_input_tokens: usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                output_tokens: usage.get("output_tokens")?.as_u64()?,
                timestamp: ts,
            })
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
        let db = crate::db::Database::open(&db_path).unwrap();
        let checkpoints_loaded: HashMap<String, FileCheckpoint> = db.load_all_checkpoints()
            .unwrap_or_default()
            .into_iter()
            .map(|cp| (cp.file_path.clone(), cp))
            .collect();
        let (db_tx, _db_rx) = crossbeam_channel::bounded::<DbOp>(1024);
        let mut engine = TrackerEngine::new(db_tx, checkpoints_loaded);

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
        let db = crate::db::Database::open(&db_path).unwrap();
        let checkpoints_loaded: HashMap<String, FileCheckpoint> = db.load_all_checkpoints()
            .unwrap_or_default()
            .into_iter()
            .map(|cp| (cp.file_path.clone(), cp))
            .collect();
        let (db_tx, _db_rx) = crossbeam_channel::bounded::<DbOp>(1024);
        let mut engine = TrackerEngine::new(db_tx, checkpoints_loaded);

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
        let db = crate::db::Database::open(&db_path).unwrap();
        let checkpoints_loaded: HashMap<String, FileCheckpoint> = db.load_all_checkpoints()
            .unwrap_or_default()
            .into_iter()
            .map(|cp| (cp.file_path.clone(), cp))
            .collect();
        let (db_tx, _db_rx) = crossbeam_channel::bounded::<DbOp>(1024);
        let mut engine = TrackerEngine::new(db_tx, checkpoints_loaded);

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
        let db = crate::db::Database::open(&db_path).unwrap();
        let checkpoints_loaded: HashMap<String, FileCheckpoint> = db.load_all_checkpoints()
            .unwrap_or_default()
            .into_iter()
            .map(|cp| (cp.file_path.clone(), cp))
            .collect();
        let (db_tx, _db_rx) = crossbeam_channel::bounded::<DbOp>(1024);
        let mut engine = TrackerEngine::new(db_tx, checkpoints_loaded);

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
        let db = crate::db::Database::open(&db_path).unwrap();
        let checkpoints_loaded: HashMap<String, FileCheckpoint> = db.load_all_checkpoints()
            .unwrap_or_default()
            .into_iter()
            .map(|cp| (cp.file_path.clone(), cp))
            .collect();
        let (db_tx, _db_rx) = crossbeam_channel::bounded::<DbOp>(1024);
        let mut engine = TrackerEngine::new(db_tx, checkpoints_loaded);

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
        let db = crate::db::Database::open(&db_path).unwrap();
        let checkpoints_loaded: HashMap<String, FileCheckpoint> = db.load_all_checkpoints()
            .unwrap_or_default()
            .into_iter()
            .map(|cp| (cp.file_path.clone(), cp))
            .collect();
        let (db_tx, _db_rx) = crossbeam_channel::bounded::<DbOp>(1024);
        let mut engine = TrackerEngine::new(db_tx, checkpoints_loaded);

        let parser = TestParser;
        let summaries = engine.cold_start(&parser, dir.path().to_str().unwrap()).unwrap();
        assert!(summaries.is_empty());
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
        let db = crate::db::Database::open(&db_path).unwrap();
        let start = Instant::now();
        for _ in 0..iterations {
            db.upsert_checkpoint(&cp).unwrap();
        }
        let db_us = start.elapsed().as_micros() / iterations as u128;

        // Bench full process_file (cold start + incremental)
        let _db2 = crate::db::Database::open(&dir.path().join("bench2.db")).unwrap();
        let (db_tx2, _db_rx2) = crossbeam_channel::bounded::<DbOp>(1024);
        let mut engine = TrackerEngine::new(db_tx2, HashMap::new());
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
        let events = engine.process_file_with_ts(path_str, &parser).unwrap();
        let incr_us = start.elapsed().as_micros();

        println!("\n=== toki benchmark ===");
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
