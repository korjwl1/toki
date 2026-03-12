use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use crate::checkpoint::{find_resume_offset, process_lines_streaming};
use crate::common::types::{FileCheckpoint, LogParser, LogParserWithTs, ModelUsageSummary, UsageEvent};
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeZone, Weekday};
use chrono_tz::Tz;
use crate::db::Database;
use crate::pricing::PricingTable;
use crate::sink::Sink;

/// Debug level:
///   0 = off
///   1 = normal debug logging (state transitions, events, timing)
///   2 = level 1 + verbose (size-unchanged, no-new-lines skips)
pub fn debug_level() -> u8 {
    static LEVEL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| {
        std::env::var("CLITRACE_DEBUG").map_or(0, |v| match v.as_str() {
            "true" | "1" => 1,
            "2" => 2,
            _ => 0,
        })
    })
}

macro_rules! debug_log {
    ($($arg:tt)*) => {
        if debug_level() >= 1 {
            eprintln!("[clitrace:debug] {}", format!($($arg)*));
        }
    };
}

/// Verbose debug log — only emitted at level 2.
/// Used for high-frequency skip events (size unchanged, no new lines).
macro_rules! debug_log_verbose {
    ($($arg:tt)*) => {
        if debug_level() >= 2 {
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
    session_filter: Option<&str>,
    project_filter: Option<&str>,
) -> Result<ColdStartResult, Box<dyn std::error::Error>> {
    let sessions = apply_filters(parser.discover_sessions(root_dir), session_filter, project_filter);

    if sessions.is_empty() {
        return Ok(ColdStartResult {
            summaries: HashMap::new(),
            checkpoints: Vec::new(),
            session_count: 0,
            total_files: 0,
        });
    }

    let total_files: usize = sessions.iter()
        .map(|s| 1 + s.subagent_jsonls.len())
        .sum();

    let summaries: Mutex<HashMap<String, ModelUsageSummary>> = Mutex::new(HashMap::new());
    let cp_batch: Mutex<Vec<FileCheckpoint>> = Mutex::new(Vec::new());

    parallel_scan(&sessions, checkpoints, |path, offset| {
        let mut local: HashMap<String, ModelUsageSummary> = HashMap::new();
        let result = process_lines_streaming(path, offset, |line| {
            if let Some(event) = parser.parse_line(line, path) {
                let summary = local.entry(event.model.clone()).or_insert_with(|| ModelUsageSummary {
                    model: event.model.clone(),
                    ..Default::default()
                });
                summary.accumulate(&event);
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

    Ok(ColdStartResult {
        summaries: summaries.into_inner().unwrap_or_else(|e| e.into_inner()),
        checkpoints: cp_batch.into_inner().unwrap_or_else(|e| e.into_inner()),
        session_count: sessions.len(),
        total_files,
    })
}

pub fn cold_start_report(
    parser: &(dyn LogParser + Sync),
    root_dir: &str,
    sink: &dyn Sink,
    session_filter: Option<&str>,
    project_filter: Option<&str>,
    pricing: Option<&PricingTable>,
) -> Result<HashMap<String, ModelUsageSummary>, Box<dyn std::error::Error>> {
    let empty = HashMap::new();
    let result = cold_start_collect(parser, root_dir, &empty, session_filter, project_filter)?;
    sink.emit_summary(&result.summaries, pricing);
    Ok(result.summaries)
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
pub struct ReportFilter {
    pub since: Option<NaiveDateTime>,
    pub until: Option<NaiveDateTime>,
    /// Timezone for bucketing and display. None = UTC.
    pub tz: Option<Tz>,
}

impl Default for ReportFilter {
    fn default() -> Self {
        ReportFilter { since: None, until: None, tz: None }
    }
}

pub fn cold_start_report_grouped<P>(
    parser: &P,
    root_dir: &str,
    group_by: ReportGroupBy,
    checkpoints: &HashMap<String, FileCheckpoint>,
    filter: ReportFilter,
    sink: &dyn Sink,
    session_filter: Option<&str>,
    project_filter: Option<&str>,
    pricing: Option<&PricingTable>,
) -> Result<(), Box<dyn std::error::Error>>
where
    P: LogParser + LogParserWithTs + Sync,
{
    let sessions = apply_filters(parser.discover_sessions(root_dir), session_filter, project_filter);
    if sessions.is_empty() {
        sink.emit_grouped(&HashMap::new(), group_by.type_name(), pricing);
        return Ok(());
    }

    let grouped: Mutex<HashMap<String, HashMap<String, ModelUsageSummary>>> =
        Mutex::new(HashMap::new());

    parallel_scan(&sessions, checkpoints, |path, offset| {
        let mut local: HashMap<String, HashMap<String, ModelUsageSummary>> = HashMap::new();
        let _ = process_lines_streaming(path, offset, |line| {
            if let Some(event) = parser.parse_line_with_ts(line, path) {
                if let Some(bucket) = bucket_from_timestamp_filtered(&event.timestamp, group_by, filter) {
                    let by_model = local.entry(bucket).or_insert_with(HashMap::new);
                    let summary = by_model.entry(event.model.clone()).or_insert_with(|| ModelUsageSummary {
                        model: event.model.clone(),
                        ..Default::default()
                    });
                    summary.accumulate_with_ts(&event);
                }
            }
        });
        if !local.is_empty() {
            let mut g = grouped.lock().unwrap_or_else(|e| e.into_inner());
            merge_grouped(&mut g, local);
        }
    });

    sink.emit_grouped(&grouped.into_inner().unwrap_or_else(|e| e.into_inner()), group_by.type_name(), pricing);
    Ok(())
}

pub fn cold_start_report_filtered<P>(
    parser: &P,
    root_dir: &str,
    checkpoints: &HashMap<String, FileCheckpoint>,
    filter: ReportFilter,
    session_filter: Option<&str>,
    project_filter: Option<&str>,
) -> Result<HashMap<String, ModelUsageSummary>, Box<dyn std::error::Error>>
where
    P: LogParser + LogParserWithTs + Sync,
{
    let sessions = apply_filters(parser.discover_sessions(root_dir), session_filter, project_filter);
    if sessions.is_empty() {
        return Ok(HashMap::new());
    }

    let summaries: Mutex<HashMap<String, ModelUsageSummary>> = Mutex::new(HashMap::new());

    parallel_scan(&sessions, checkpoints, |path, offset| {
        let mut local: HashMap<String, ModelUsageSummary> = HashMap::new();
        let _ = process_lines_streaming(path, offset, |line| {
            if let Some(event) = parser.parse_line_with_ts(line, path) {
                let ts = match parse_timestamp(&event.timestamp) {
                    Some(t) => t,
                    None => return,
                };
                if !filter_match(ts, filter) {
                    return;
                }
                let summary = local.entry(event.model.clone()).or_insert_with(|| ModelUsageSummary {
                    model: event.model.clone(),
                    ..Default::default()
                });
                summary.accumulate_with_ts(&event);
            }
        });
        if !local.is_empty() {
            let mut s = summaries.lock().unwrap_or_else(|e| e.into_inner());
            merge_summaries(&mut s, local);
        }
    });

    Ok(summaries.into_inner().unwrap_or_else(|e| e.into_inner()))
}

/// Report grouped by session ID (each session shows its per-model breakdown).
pub fn cold_start_report_by_session<P>(
    parser: &P,
    root_dir: &str,
    checkpoints: &HashMap<String, FileCheckpoint>,
    filter: ReportFilter,
    session_filter: Option<&str>,
    project_filter: Option<&str>,
    sink: &dyn Sink,
    pricing: Option<&PricingTable>,
) -> Result<(), Box<dyn std::error::Error>>
where
    P: LogParser + LogParserWithTs + Sync,
{
    let sessions = apply_filters(parser.discover_sessions(root_dir), session_filter, project_filter);
    if sessions.is_empty() {
        sink.emit_grouped(&HashMap::new(), "session", pricing);
        return Ok(());
    }

    let grouped: Mutex<HashMap<String, HashMap<String, ModelUsageSummary>>> =
        Mutex::new(HashMap::new());

    let use_ts_filter = filter_active(filter);

    parallel_scan(&sessions, checkpoints, |path, offset| {
        let session_id = match extract_session_id(path) {
            Some(id) => id,
            None => return,
        };
        let mut local: HashMap<String, ModelUsageSummary> = HashMap::new();
        let _ = process_lines_streaming(path, offset, |line| {
            if use_ts_filter {
                if let Some(event) = parser.parse_line_with_ts(line, path) {
                    let ts = match parse_timestamp(&event.timestamp) {
                        Some(t) => t,
                        None => return,
                    };
                    if !filter_match(ts, filter) {
                        return;
                    }
                    let summary = local.entry(event.model.clone()).or_insert_with(|| ModelUsageSummary {
                        model: event.model.clone(),
                        ..Default::default()
                    });
                    summary.accumulate_with_ts(&event);
                }
            } else if let Some(event) = parser.parse_line(line, path) {
                let summary = local.entry(event.model.clone()).or_insert_with(|| ModelUsageSummary {
                    model: event.model.clone(),
                    ..Default::default()
                });
                summary.accumulate(&event);
            }
        });
        if !local.is_empty() {
            let mut g = grouped.lock().unwrap_or_else(|e| e.into_inner());
            let session_models = g.entry(session_id).or_insert_with(HashMap::new);
            merge_summaries(session_models, local);
        }
    });

    sink.emit_grouped(&grouped.into_inner().unwrap_or_else(|e| e.into_inner()), "session", pricing);
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
        let result = cold_start_collect(parser, root_dir, &self.checkpoints, None, self.project_filter.as_deref())?;

        if result.session_count == 0 {
            debug_log!("cold_start — 0 sessions, 0 files ({}µs)", t_cold.elapsed().as_micros());
            return Ok(HashMap::new());
        }

        for cp in &result.checkpoints {
            self.checkpoints.insert(cp.file_path.clone(), cp.clone());
        }

        // Emit summary
        self.sink.emit_summary(&result.summaries, self.pricing.as_ref());

        // Batch flush for cold start (single transaction).
        let t_flush = Instant::now();
        self.db.flush_checkpoints(&result.checkpoints)?;
        debug_log!("cold_start — {} sessions, {} files, {} checkpoints flushed (flush: {}µs, total: {}µs)",
            result.session_count, result.total_files, result.checkpoints.len(), t_flush.elapsed().as_micros(), t_cold.elapsed().as_micros());

        Ok(result.summaries)
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

        parallel_scan(&sessions, &self.checkpoints, |path, offset| {
            let mut local: HashMap<String, HashMap<String, ModelUsageSummary>> = HashMap::new();
            let result = process_lines_streaming(path, offset, |line| {
                if let Some(event) = parser.parse_line_with_ts(line, path) {
                    if let Some(bucket) = bucket_from_timestamp_filtered(&event.timestamp, group_by, filter) {
                        let by_model = local.entry(bucket).or_insert_with(HashMap::new);
                        let summary = by_model.entry(event.model.clone()).or_insert_with(|| ModelUsageSummary {
                            model: event.model.clone(),
                            ..Default::default()
                        });
                        summary.accumulate_with_ts(&event);
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
            let t_flush = Instant::now();
            self.db.flush_checkpoints(&batch)?;
            debug_log!("cold_start_grouped — {} checkpoints flushed ({}µs)",
                batch.len(), t_flush.elapsed().as_micros());
        }

        self.sink.emit_grouped(&grouped.into_inner().unwrap_or_else(|e| e.into_inner()), group_by.type_name(), self.pricing.as_ref());
        debug_log!("cold_start_grouped — ({}µs)", t_cold.elapsed().as_micros());
        Ok(())
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

        // Update cached file size from offset + bytes consumed (avoid second metadata() call)
        match result {
            None => {
                // No lines read — update size cache from offset (file may have trailing incomplete data)
                if let Ok(meta) = std::fs::metadata(path) {
                    self.file_sizes.insert(path.to_string(), meta.len());
                }
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
                // Update cached file size: offset + bytes_read = last consumed position.
                // File may have more trailing data, so use metadata only if needed for accuracy.
                self.file_sizes.insert(path.to_string(), offset + bytes_read);

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
        if !self.matches_filters(path) {
            return;
        }
        match self.process_file(path, parser) {
            Ok(events) => {
                for event in &events {
                    self.sink.emit_event(event, self.pricing.as_ref());
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
        gs.input_tokens += ls.input_tokens;
        gs.cache_creation_input_tokens += ls.cache_creation_input_tokens;
        gs.cache_read_input_tokens += ls.cache_read_input_tokens;
        gs.output_tokens += ls.output_tokens;
        gs.event_count += ls.event_count;
    }
}

/// Merge thread-local grouped summaries into global grouped map.
fn merge_grouped(
    global: &mut HashMap<String, HashMap<String, ModelUsageSummary>>,
    local: HashMap<String, HashMap<String, ModelUsageSummary>>,
) {
    for (bucket, local_models) in local {
        let global_models = global.entry(bucket).or_insert_with(HashMap::new);
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
    let parts: Vec<&str> = path.rsplit('/').collect();
    // Subagent: parts = ["agent-xxx.jsonl", "subagents", "<UUID>", ...]
    if parts.len() >= 3 && parts[1] == "subagents" {
        return Some(parts[2].to_string());
    }
    // Parent: filename without .jsonl
    parts.first().map(|s| s.trim_end_matches(".jsonl").to_string())
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
