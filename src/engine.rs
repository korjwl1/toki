use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

use crate::checkpoint::{find_resume_offset, process_lines_streaming};
use crate::common::types::{FileCheckpoint, LogParser, LogParserWithTs, ModelUsageSummary, TokenFields};
use crate::providers::Provider;
use chrono::{DateTime, NaiveDateTime, Weekday};
use chrono_tz::Tz;
use crate::sink::Sink;
use crate::writer::{ColdStartEvent, DbOp, WriteEventData};

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

/// Verbose debug log -- only emitted at level 2.
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
/// Time without new lines before a file transitions Active -> Idle.
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
    /// provider_name -> db_tx channel
    channels: HashMap<String, Sender<DbOp>>,
    checkpoints: HashMap<String, FileCheckpoint>,
    /// Cached file sizes for fast skip when size unchanged.
    file_sizes: HashMap<String, u64>,
    /// Per-file activity tracking (state + cooldowns).
    activity: HashMap<String, FileActivity>,
    /// file_path -> provider_name for routing flushes.
    dirty: HashMap<String, String>,
    /// Output sink (print, UDS, HTTP, or multi).
    sink: Box<dyn Sink>,
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
    pub fn new(
        channels: HashMap<String, Sender<DbOp>>,
        checkpoints: HashMap<String, FileCheckpoint>,
        sink: Box<dyn Sink>,
    ) -> Self {
        TrackerEngine {
            channels,
            checkpoints,
            file_sizes: HashMap::new(),
            activity: HashMap::new(),
            dirty: HashMap::new(),
            sink,
        }
    }

    /// Backward-compatible constructor for single-provider use (tests).
    pub fn new_single(db_tx: Sender<DbOp>, checkpoints: HashMap<String, FileCheckpoint>, sink: Box<dyn Sink>) -> Self {
        let mut channels = HashMap::new();
        channels.insert("default".to_string(), db_tx);
        TrackerEngine {
            channels,
            checkpoints,
            file_sizes: HashMap::new(),
            activity: HashMap::new(),
            dirty: HashMap::new(),
            sink,
        }
    }

    /// Get a db_tx channel by provider name, falling back to "default" for backward compat.
    fn get_channel(&self, provider_name: &str) -> Option<&Sender<DbOp>> {
        self.channels.get(provider_name).or_else(|| self.channels.get("default"))
    }

    /// Cold start with Provider trait: discover all sessions, process in parallel,
    /// aggregate by model, print summary, flush checkpoints, and populate TSDB.
    pub fn cold_start_provider(
        &mut self,
        provider: &dyn Provider,
        db_tx: &Sender<DbOp>,
    ) -> Result<HashMap<String, ModelUsageSummary>, Box<dyn std::error::Error>> {
        let t_cold = Instant::now();
        let sessions = provider.discover_sessions();

        if sessions.is_empty() {
            debug_log!("cold_start[{}] -- 0 sessions, 0 files ({}us)", provider.name(), t_cold.elapsed().as_micros());
            return Ok(HashMap::new());
        }

        let total_files: usize = sessions.iter().map(|s| 1 + s.subagent_jsonls.len()).sum();
        let summaries: Mutex<HashMap<String, ModelUsageSummary>> = Mutex::new(HashMap::new());
        let cp_batch: Mutex<Vec<FileCheckpoint>> = Mutex::new(Vec::new());
        let event_count: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

        // Macro for the per-event accumulation logic (shared across provider branches).
        // This avoids code duplication while keeping the entire parse→emit chain generic/inlinable.
        macro_rules! handle_parsed {
            ($parsed:expr, $local:expr, $file_events:expr,
             $parser_project_name:expr, $path_project_name:expr,
             $session_id:expr, $source_file:expr) => {
                if let Some(ref pn) = $parsed.project_name {
                    $parser_project_name = Some(pn.as_str().into());
                }
                // Only clone model when inserting a new entry (0 clones for existing models)
                if !$local.contains_key(&$parsed.model) {
                    $local.insert($parsed.model.clone(), ModelUsageSummary {
                        model: $parsed.model.clone(),
                        ..Default::default()
                    });
                }
                let summary = $local.get_mut(&$parsed.model).unwrap();
                let effective_project = $parser_project_name.clone()
                    .or_else(|| $path_project_name.clone());
                $file_events.push($parsed.into_summary_and_event(
                    summary,
                    std::sync::Arc::clone(&$session_id),
                    std::sync::Arc::clone(&$source_file),
                    effective_project,
                ));
            };
        }

        let provider_name = provider.name().to_string();
        parallel_scan(&sessions, &self.checkpoints, |path, offset| {
            let mut local: HashMap<String, ModelUsageSummary> = HashMap::new();
            let mut file_events: Vec<ColdStartEvent> = Vec::new();
            let session_id: std::sync::Arc<str> = provider.extract_session_id(path).unwrap_or_default().into();
            let source_file: std::sync::Arc<str> = path.into();
            let path_project_name: Option<std::sync::Arc<str>> = provider
                .extract_project_name(path)
                .map(|s| s.into());
            let mut parser_project_name: Option<std::sync::Arc<str>> = None;

            // Dispatch by provider name so the entire parse→emit chain is
            // concrete/generic — no dyn dispatch anywhere in the hot path.
            // New providers: add a branch here.
            let result = match provider_name.as_str() {
                "claude_code" => {
                    let parser = crate::providers::claude_code::ClaudeCodeParser;
                    process_lines_streaming(path, offset, |line| {
                        if let Some(parsed) = parser.parse_for_cold_start(line) {
                            handle_parsed!(parsed, local, file_events,
                                parser_project_name, path_project_name,
                                session_id, source_file);
                        }
                    })
                }
                "codex" => {
                    let mut parser = crate::providers::codex::parser::CodexFileParser::new();
                    process_lines_streaming(path, offset, |line| {
                        if let Some(parsed) = <crate::providers::codex::parser::CodexFileParser
                            as crate::providers::FileParser>::parse_line(&mut parser, line) {
                            handle_parsed!(parsed, local, file_events,
                                parser_project_name, path_project_name,
                                session_id, source_file);
                        }
                    })
                }
                _ => {
                    // Fallback for unknown providers: dyn dispatch
                    provider.scan_file_cold_start(path, offset, &mut |parsed| {
                        handle_parsed!(parsed, local, file_events,
                            parser_project_name, path_project_name,
                            session_id, source_file);
                    })
                }
            };
            if !local.is_empty() {
                let mut s = summaries.lock().unwrap_or_else(|e| e.into_inner());
                merge_summaries(&mut s, local);
            }
            if !file_events.is_empty() {
                event_count.fetch_add(file_events.len(), std::sync::atomic::Ordering::Relaxed);
                let _ = db_tx.send(DbOp::BulkWrite(file_events));
            }
            if let Ok(Some((_bytes, last_line_len, last_line_hash))) = result {
                cp_batch.lock().unwrap_or_else(|e| e.into_inner()).push(FileCheckpoint {
                    file_path: path.to_string(),
                    last_line_len,
                    last_line_hash,
                });
            }
        });

        let t_parse = t_cold.elapsed();
        let result_summaries = summaries.into_inner().unwrap_or_else(|e| e.into_inner());
        let checkpoints_batch = cp_batch.into_inner().unwrap_or_else(|e| e.into_inner());
        let total_events = event_count.load(std::sync::atomic::Ordering::Relaxed);

        for cp in &checkpoints_batch {
            self.checkpoints.insert(cp.file_path.clone(), cp.clone());
        }

        // Emit summary with provider-appropriate schema
        let schema = crate::common::schema::schema_for_provider(provider.name());
        self.sink.emit_summary(&result_summaries, None, Some(schema));

        // Signal writer to flush accumulated rollups, then wait for completion
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        let _ = db_tx.send(DbOp::FlushBulkRollups(done_tx));
        let _ = done_rx.recv();

        // Flush checkpoints
        let cp_count = checkpoints_batch.len();
        if !checkpoints_batch.is_empty() {
            let _ = db_tx.send(DbOp::FlushCheckpoints(checkpoints_batch));
        }
        let t_total = t_cold.elapsed();
        let t_db = t_total - t_parse;
        debug_log!("cold_start[{}] -- {} sessions, {} files, {} events, {} checkpoints (parse: {}us, db_wait: {}us, total: {}us)",
            provider.name(), sessions.len(), total_files, total_events, cp_count,
            t_parse.as_micros(), t_db.as_micros(), t_total.as_micros());

        Ok(result_summaries)
    }

    /// Legacy cold start for backward compatibility (used by tests with LogParser trait).
    /// NOTE: Kept as a thin wrapper to avoid test disruption. New code should use
    /// `cold_start_provider()` with a proper Provider implementation.
    pub fn cold_start<P>(
        &mut self,
        parser: &P,
        root_dir: &str,
    ) -> Result<HashMap<String, ModelUsageSummary>, Box<dyn std::error::Error>>
    where
        P: LogParser + LogParserWithTs + Sync,
    {
        let t_cold = Instant::now();
        let sessions = parser.discover_sessions(root_dir);

        if sessions.is_empty() {
            debug_log!("cold_start -- 0 sessions, 0 files ({}us)", t_cold.elapsed().as_micros());
            return Ok(HashMap::new());
        }

        let total_files: usize = sessions.iter().map(|s| 1 + s.subagent_jsonls.len()).sum();
        let summaries: Mutex<HashMap<String, ModelUsageSummary>> = Mutex::new(HashMap::new());
        let cp_batch: Mutex<Vec<FileCheckpoint>> = Mutex::new(Vec::new());
        let event_count: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let db_tx = self.channels.values().next().expect("no db channel");

        let cs_parser = crate::providers::claude_code::ClaudeCodeParser;
        parallel_scan(&sessions, &self.checkpoints, |path, offset| {
            let mut local: HashMap<String, ModelUsageSummary> = HashMap::new();
            let mut file_events: Vec<ColdStartEvent> = Vec::new();
            let session_id: std::sync::Arc<str> = crate::providers::claude_code::extract_session_id(path).unwrap_or_default().into();
            let source_file: std::sync::Arc<str> = path.into();
            let project_name: Option<std::sync::Arc<str>> = crate::providers::claude_code::extract_project_name(path).map(|s| s.into());
            let result = process_lines_streaming(path, offset, |line| {
                if let Some(parsed) = cs_parser.parse_for_cold_start(line) {
                    // Only clone model when inserting a new entry (0 clones for existing models)
                    if !local.contains_key(&parsed.model) {
                        local.insert(parsed.model.clone(), ModelUsageSummary {
                            model: parsed.model.clone(),
                            ..Default::default()
                        });
                    }
                    let summary = local.get_mut(&parsed.model).unwrap();

                    file_events.push(parsed.into_summary_and_event(
                        summary,
                        std::sync::Arc::clone(&session_id),
                        std::sync::Arc::clone(&source_file),
                        project_name.clone(),
                    ));
                }
            });
            if !local.is_empty() {
                let mut s = summaries.lock().unwrap_or_else(|e| e.into_inner());
                merge_summaries(&mut s, local);
            }
            if !file_events.is_empty() {
                event_count.fetch_add(file_events.len(), std::sync::atomic::Ordering::Relaxed);
                let _ = db_tx.send(DbOp::BulkWrite(file_events));
            }
            if let Ok(Some((_bytes, last_line_len, last_line_hash))) = result {
                cp_batch.lock().unwrap_or_else(|e| e.into_inner()).push(FileCheckpoint {
                    file_path: path.to_string(),
                    last_line_len,
                    last_line_hash,
                });
            }
        });

        let t_parse = t_cold.elapsed();
        let result_summaries = summaries.into_inner().unwrap_or_else(|e| e.into_inner());
        let checkpoints_batch = cp_batch.into_inner().unwrap_or_else(|e| e.into_inner());
        let total_events = event_count.load(std::sync::atomic::Ordering::Relaxed);

        for cp in &checkpoints_batch {
            self.checkpoints.insert(cp.file_path.clone(), cp.clone());
        }

        self.sink.emit_summary(&result_summaries, None, None);

        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        let _ = db_tx.send(DbOp::FlushBulkRollups(done_tx));
        let _ = done_rx.recv();

        let cp_count = checkpoints_batch.len();
        if !checkpoints_batch.is_empty() {
            let _ = db_tx.send(DbOp::FlushCheckpoints(checkpoints_batch));
        }
        let t_total = t_cold.elapsed();
        let t_db = t_total - t_parse;
        debug_log!("cold_start -- {} sessions, {} files, {} events, {} checkpoints (parse: {}us, db_wait: {}us, total: {}us)",
            sessions.len(), total_files, total_events, cp_count,
            t_parse.as_micros(), t_db.as_micros(), t_total.as_micros());

        Ok(result_summaries)
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
                    debug_log!("demote {} -> Idle ({}s since last active)",
                        path, now.duration_since(act.last_active).as_secs());
                }
                let cd = if s == FileState::Active { ACTIVE_COOLDOWN } else { IDLE_COOLDOWN };
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
                    // Avoid path.to_string() allocation: use get_mut for existing entries
                    if let Some(act) = self.activity.get_mut(path) {
                        act.last_checked = now;
                        act.state = state;
                    } else {
                        self.activity.insert(path.to_string(), FileActivity {
                            state, last_active: now, last_checked: now,
                        });
                    }
                    debug_log_verbose!("process_file {} -- size unchanged ({}B), {} ({}us)",
                        path, current_size,
                        if state == FileState::Active { "Active" } else { "Idle" },
                        t_total.elapsed().as_micros());
                    return Ok(Vec::new());
                }
            }
        }

        let path_owned = path.to_string();

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
                    self.file_sizes.insert(path_owned.clone(), meta.len());
                }
                let act = self.activity.entry(path_owned).or_insert(FileActivity {
                    state, last_active: now, last_checked: now,
                });
                act.last_checked = now;
                act.state = state;
                debug_log_verbose!("process_file {} -- no new lines (find_resume: {}us, read: {}us)",
                    path, find_us, read_us);
                Ok(Vec::new())
            }
            Some((bytes_read, last_line_len, last_line_hash)) => {
                self.file_sizes.insert(path_owned.clone(), offset + bytes_read);
                let cp = FileCheckpoint {
                    file_path: path_owned.clone(),
                    last_line_len,
                    last_line_hash,
                };
                self.checkpoints.insert(path_owned.clone(), cp);
                // Use "default" provider name for backward compat dirty tracking
                self.dirty.insert(path_owned.clone(), "default".to_string());
                if state == FileState::Idle {
                    debug_log!("promote {} -> Active ({} new lines)", path, line_count);
                }
                self.activity.insert(path_owned, FileActivity {
                    state: FileState::Active, last_active: now, last_checked: now,
                });
                debug_log!("process_file {} -- {} lines, {} bytes, {} events, Active | find_resume: {}us, read: {}us, total: {}us",
                    path, line_count, bytes_read, events.len(),
                    find_us, read_us, t_total.elapsed().as_micros());
                Ok(events)
            }
        }
    }

    /// Process a single file change using dynamic dispatch (for multi-provider watch mode).
    fn process_file_with_ts_dyn(
        &mut self,
        path: &str,
        parser_ts: &dyn LogParserWithTs,
        provider_name: &str,
    ) -> Result<Vec<crate::common::types::UsageEventWithTs>, Box<dyn std::error::Error>> {
        let now = Instant::now();

        let state = match self.activity.get(path) {
            None => FileState::Active,
            Some(act) => {
                let mut s = act.state;
                if s == FileState::Active && now.duration_since(act.last_active) > IDLE_TRANSITION {
                    s = FileState::Idle;
                    debug_log!("demote {} -> Idle ({}s since last active)",
                        path, now.duration_since(act.last_active).as_secs());
                }
                let cd = if s == FileState::Active { ACTIVE_COOLDOWN } else { IDLE_COOLDOWN };
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
                    // Avoid path.to_string() allocation: use get_mut for existing entries
                    if let Some(act) = self.activity.get_mut(path) {
                        act.last_checked = now;
                        act.state = state;
                    } else {
                        self.activity.insert(path.to_string(), FileActivity {
                            state, last_active: now, last_checked: now,
                        });
                    }
                    return Ok(Vec::new());
                }
            }
        }

        let path_owned = path.to_string();

        let t0 = Instant::now();
        let offset = self.determine_offset(path)?;
        let find_us = t0.elapsed().as_micros();

        let t1 = Instant::now();
        let mut events = Vec::new();
        let mut line_count: u64 = 0;
        let result = process_lines_streaming(path, offset, |line| {
            if let Some(event) = parser_ts.parse_line_with_ts(line, path) {
                events.push(event);
            }
            line_count += 1;
        })?;
        let read_us = t1.elapsed().as_micros();

        match result {
            None => {
                if let Ok(meta) = std::fs::metadata(path) {
                    self.file_sizes.insert(path_owned.clone(), meta.len());
                }
                let act = self.activity.entry(path_owned).or_insert(FileActivity {
                    state, last_active: now, last_checked: now,
                });
                act.last_checked = now;
                act.state = state;
                Ok(Vec::new())
            }
            Some((bytes_read, last_line_len, last_line_hash)) => {
                self.file_sizes.insert(path_owned.clone(), offset + bytes_read);
                let cp = FileCheckpoint {
                    file_path: path_owned.clone(),
                    last_line_len,
                    last_line_hash,
                };
                self.checkpoints.insert(path_owned.clone(), cp);
                self.dirty.insert(path_owned.clone(), provider_name.to_string());
                if state == FileState::Idle {
                    debug_log!("promote {} -> Active ({} new lines)", path, line_count);
                }
                self.activity.insert(path_owned, FileActivity {
                    state: FileState::Active, last_active: now, last_checked: now,
                });
                debug_log!("process_file {} -- {} lines, {} bytes, {} events, Active | find_resume: {}us, read: {}us, total: {}us",
                    path, line_count, bytes_read, events.len(),
                    find_us, read_us, t_total.elapsed().as_micros());
                Ok(events)
            }
        }
    }

    fn process_and_print_provider(&mut self, path: &str, provider: &dyn Provider, db_tx: &Sender<DbOp>) {
        let event_schema = crate::common::schema::schema_for_provider(provider.name());
        match self.process_file_with_ts_dyn(path, provider.parser_with_ts(), provider.name()) {
            Ok(events) => {
                let session_id = provider.extract_session_id(path).unwrap_or_default();
                let project_name = provider.extract_project_name(path).map(|s| s.to_string());
                for event in events {
                    let ts_ms = crate::common::time::parse_ts_to_ms(&event.timestamp)
                        .unwrap_or_else(|| {
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as i64
                        });

                    let (usage, _ts) = event.into_usage_event();
                    self.sink.emit_event(&usage, None, Some(event_schema));

                    let op = DbOp::WriteEvent(Box::new(WriteEventData {
                        ts_ms,
                        message_id: usage.event_key,
                        model: usage.model,
                        session_id: session_id.clone(),
                        source_file: usage.source_file,
                        project_name: project_name.clone(),
                        tokens: TokenFields {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cache_creation_input_tokens: usage.cache_creation_input_tokens,
                            cache_read_input_tokens: usage.cache_read_input_tokens,
                        },
                    }));
                    // Use blocking send to apply backpressure instead of dropping events
                    if let Err(e) = db_tx.send(op) {
                        debug_log!("writer channel closed: {}", e);
                    }
                }
                // dirty is already marked inside process_file_with_ts_dyn with the correct provider name
            }
            Err(e) => {
                eprintln!("[toki] Error processing {}: {}", path, e);
            }
        }
    }

    /// Legacy process_and_print for backward compat (used in legacy watch_loop).
    /// NOTE: Kept for test backward compatibility. New code should use `process_and_print_provider()`.
    fn process_and_print<P>(&mut self, path: &str, parser: &P)
    where
        P: LogParser + LogParserWithTs,
    {
        match self.process_file_with_ts(path, parser) {
            Ok(events) => {
                let session_id = crate::providers::claude_code::extract_session_id(path).unwrap_or_default();
                let project_name = crate::providers::claude_code::extract_project_name(path).map(|s| s.to_string());
                for event in events {
                    // Use fast parse_ts_to_ms (~0.1us) instead of chrono parse (~3-5us)
                    let ts_ms = crate::common::time::parse_ts_to_ms(&event.timestamp)
                        .unwrap_or_else(|| {
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as i64
                        });

                    let (usage, _ts) = event.into_usage_event();
                    self.sink.emit_event(&usage, None, None);

                    let op = DbOp::WriteEvent(Box::new(WriteEventData {
                        ts_ms,
                        message_id: usage.event_key,
                        model: usage.model,
                        session_id: session_id.clone(),
                        source_file: usage.source_file,
                        project_name: project_name.clone(),
                        tokens: TokenFields {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cache_creation_input_tokens: usage.cache_creation_input_tokens,
                            cache_read_input_tokens: usage.cache_read_input_tokens,
                        },
                    }));
                    if let Some(tx) = self.channels.values().next() {
                        // Use blocking send to apply backpressure instead of dropping events
                        if let Err(e) = tx.send(op) {
                            debug_log!("writer channel closed: {}", e);
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("[toki] Error processing {}: {}", path, e);
            }
        }
    }

    /// Flush dirty checkpoints via writer thread, routing to correct provider channel.
    fn flush_dirty(&mut self) {
        if self.dirty.is_empty() {
            return;
        }
        // Group dirty paths by provider
        let mut by_provider: HashMap<String, Vec<FileCheckpoint>> = HashMap::new();
        for (path, provider_name) in &self.dirty {
            if let Some(cp) = self.checkpoints.get(path) {
                by_provider.entry(provider_name.clone())
                    .or_default()
                    .push(cp.clone());
            }
        }
        let total_count: usize = by_provider.values().map(|v| v.len()).sum();
        for (provider_name, batch) in by_provider {
            if let Some(tx) = self.get_channel(&provider_name) {
                let _ = tx.send(DbOp::FlushCheckpoints(batch));
            }
        }
        self.dirty.clear();
        debug_log!("flush_dirty -- {} checkpoints sent to writer(s)", total_count);
    }

    /// Remove entries for files that no longer exist on disk.
    /// Called periodically from the watch loop to prevent unbounded HashMap growth.
    fn prune_stale_entries(&mut self) {
        let stale_keys: Vec<String> = self.file_sizes.keys()
            .filter(|path| !std::path::Path::new(path.as_str()).exists())
            .cloned()
            .collect();
        let count = stale_keys.len();
        for key in &stale_keys {
            self.file_sizes.remove(key);
            self.activity.remove(key);
            self.checkpoints.remove(key);
        }
        if count > 0 {
            debug_log!("prune_stale_entries -- removed {} stale file entries", count);
        }
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

    /// Multi-provider watch loop: receive file change events, route to owning provider,
    /// flush dirty checkpoints periodically.
    pub fn watch_loop_providers(
        &mut self,
        event_rx: Receiver<String>,
        stop_rx: Receiver<()>,
        providers: &[(Box<dyn Provider>, Sender<DbOp>)],
    ) {
        let flush_tick = crossbeam_channel::tick(FLUSH_INTERVAL);
        let prune_tick = crossbeam_channel::tick(Duration::from_secs(60));

        loop {
            crossbeam_channel::select! {
                recv(stop_rx) -> _ => {
                    self.flush_dirty();
                    break;
                }
                recv(prune_tick) -> _ => {
                    self.prune_stale_entries();
                }
                recv(event_rx) -> msg => {
                    match msg {
                        Ok(path) => {
                            let mut paths = HashSet::new();
                            paths.insert(path);
                            while let Ok(more) = event_rx.try_recv() {
                                paths.insert(more);
                            }
                            debug_log!("watch event -- {} unique paths queued", paths.len());

                            for path in paths {
                                // Route to owning provider
                                let mut handled = false;
                                for (provider, db_tx) in providers {
                                    if provider.owns_path(&path) {
                                        self.process_and_print_provider(&path, provider.as_ref(), db_tx);
                                        handled = true;
                                        break;
                                    }
                                }
                                if !handled {
                                    debug_log_verbose!("watch: no provider owns path {}", path);
                                }
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

    /// Legacy watch loop for backward compatibility (single parser).
    /// NOTE: Kept for test backward compatibility. New code should use `watch_loop_providers()`.
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
                            let mut paths = HashSet::new();
                            paths.insert(path);
                            while let Ok(more) = event_rx.try_recv() {
                                paths.insert(more);
                            }
                            debug_log!("watch event -- {} unique paths queued", paths.len());

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

/// Common parallel file scan over all sessions.
/// Uses rayon's global thread pool (default: num_cpus threads).
/// A custom pool is not configured; the global default is sufficient for cold start scans.
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
            let mut files = vec![session.parent_jsonl.to_str().unwrap_or_default().to_string()];
            for sub in &session.subagent_jsonls {
                files.push(sub.to_str().unwrap_or_default().to_string());
            }
            files
        })
        .collect();

    all_files.par_iter().for_each(|path| {
        let offset = resolve_offset(path, checkpoints);
        on_file(path, offset);
    });
}

/// Extract the full session UUID from a file path (legacy, delegates to claude_code).
pub fn extract_session_id(path: &str) -> Option<String> {
    crate::providers::claude_code::extract_session_id(path)
}

/// Extract project directory name from a source file path.
/// Currently only works for Claude Code paths (which contain `/projects/<name>/`).
/// Codex events don't encode the project in the file path; instead, project_name
/// is extracted from session_meta.cwd during cold start and stored in the project index.
/// Event-level project filtering for Codex will fall through (return None) and
/// those events won't match project filters in bucketed queries — this is a known
/// limitation that can be resolved by adding project_name_id to StoredEvent.
pub fn extract_project_name(path: &str) -> Option<&str> {
    crate::providers::claude_code::extract_project_name(path)
}

#[allow(dead_code)]
fn parse_timestamp(ts: &str) -> Option<NaiveDateTime> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
        return Some(dt.naive_utc());
    }
    NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ").ok()
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

    /// Create a db_tx + drain thread that consumes all DbOps (handles FlushBulkRollups done signal).
    fn test_db_channel() -> (crossbeam_channel::Sender<DbOp>, std::thread::JoinHandle<()>) {
        let (db_tx, db_rx) = crossbeam_channel::bounded::<DbOp>(1024);
        let handle = std::thread::spawn(move || {
            while let Ok(op) = db_rx.recv() {
                match op {
                    DbOp::FlushBulkRollups(done_tx) => { let _ = done_tx.send(()); }
                    DbOp::Shutdown => break,
                    _ => {}
                }
            }
        });
        (db_tx, handle)
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
        let (db_tx, _drain) = test_db_channel();
        let mut engine = TrackerEngine::new_single(db_tx, checkpoints_loaded, Box::new(crate::sink::PrintSink::new(crate::sink::OutputFormat::Table)));

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
        let (db_tx, _drain) = test_db_channel();
        let mut engine = TrackerEngine::new_single(db_tx, checkpoints_loaded, Box::new(crate::sink::PrintSink::new(crate::sink::OutputFormat::Table)));

        let projects_dir = dir.path().join("projects").join("test");
        std::fs::create_dir_all(&projects_dir).unwrap();

        let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let jsonl_path = projects_dir.join(format!("{}.jsonl", session_id));

        let mut f = std::fs::File::create(&jsonl_path).unwrap();
        writeln!(f, "{}", make_assistant_line("msg1", "claude-opus-4-6", 10, 100, 200, 50)).unwrap();

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
        let (db_tx, _drain) = test_db_channel();
        let mut engine = TrackerEngine::new_single(db_tx, checkpoints_loaded, Box::new(crate::sink::PrintSink::new(crate::sink::OutputFormat::Table)));

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
        let (db_tx, _drain) = test_db_channel();
        let mut engine = TrackerEngine::new_single(db_tx, checkpoints_loaded, Box::new(crate::sink::PrintSink::new(crate::sink::OutputFormat::Table)));

        let projects_dir = dir.path().join("projects").join("test");
        std::fs::create_dir_all(&projects_dir).unwrap();

        let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let jsonl_path = projects_dir.join(format!("{}.jsonl", session_id));

        let mut f = std::fs::File::create(&jsonl_path).unwrap();
        writeln!(f, "{}", make_assistant_line("msg1", "claude-opus-4-6", 10, 100, 200, 50)).unwrap();

        let parser = TestParser;
        engine.cold_start(&parser, dir.path().to_str().unwrap()).unwrap();

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
        let (db_tx, _drain) = test_db_channel();
        let mut engine = TrackerEngine::new_single(db_tx, checkpoints_loaded, Box::new(crate::sink::PrintSink::new(crate::sink::OutputFormat::Table)));

        let projects_dir = dir.path().join("projects").join("test");
        std::fs::create_dir_all(&projects_dir).unwrap();

        let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let jsonl_path = projects_dir.join(format!("{}.jsonl", session_id));

        let mut f = std::fs::File::create(&jsonl_path).unwrap();
        writeln!(f, "{}", make_assistant_line("msg1", "claude-opus-4-6", 10, 100, 200, 50)).unwrap();

        let parser = TestParser;
        let s1 = engine.cold_start(&parser, dir.path().to_str().unwrap()).unwrap();
        assert_eq!(s1["claude-opus-4-6"].event_count, 1);

        let mut f = std::fs::OpenOptions::new().append(true).open(&jsonl_path).unwrap();
        writeln!(f, "{}", make_assistant_line("msg2", "claude-opus-4-6", 20, 200, 400, 100)).unwrap();

        let s2 = engine.cold_start(&parser, dir.path().to_str().unwrap()).unwrap();
        assert_eq!(s2["claude-opus-4-6"].event_count, 1);
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
        let (db_tx, _drain) = test_db_channel();
        let mut engine = TrackerEngine::new_single(db_tx, checkpoints_loaded, Box::new(crate::sink::PrintSink::new(crate::sink::OutputFormat::Table)));

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

        let content = std::fs::read_to_string(path_str).unwrap();
        let line_490: Vec<&str> = content.lines().collect();
        let target = line_490[490];
        let cp = FileCheckpoint {
            file_path: path_str.to_string(),
            last_line_len: target.len() as u64,
            last_line_hash: hash_line(target.as_bytes()),
        };

        let start = Instant::now();
        for _ in 0..iterations {
            let _ = find_resume_offset(path_str, &cp).unwrap();
        }
        let find_us = start.elapsed().as_micros() / iterations as u128;

        let offset = find_resume_offset(path_str, &cp).unwrap().unwrap();
        let start = Instant::now();
        for _ in 0..iterations {
            let _ = process_lines_streaming(path_str, offset, |_| {}).unwrap();
        }
        let read_us = start.elapsed().as_micros() / iterations as u128;

        let db_path = dir.path().join("bench.db");
        let db = crate::db::Database::open(&db_path).unwrap();
        let start = Instant::now();
        for _ in 0..iterations {
            db.upsert_checkpoint(&cp).unwrap();
        }
        let db_us = start.elapsed().as_micros() / iterations as u128;

        let _db2 = crate::db::Database::open(&dir.path().join("bench2.db")).unwrap();
        let (db_tx2, _drain2) = test_db_channel();
        let mut engine = TrackerEngine::new_single(db_tx2, HashMap::new(), Box::new(crate::sink::PrintSink::new(crate::sink::OutputFormat::Table)));
        let parser = TestParser;

        let start = Instant::now();
        engine.cold_start(&parser, dir.path().to_str().unwrap()).unwrap();
        let cold_us = start.elapsed().as_micros();

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
        println!("  find_resume_offset:        {:>6}us", find_us);
        println!("  process_lines_streaming:   {:>6}us", read_us);
        println!("  db.upsert_checkpoint:      {:>6}us", db_us);
        println!();
        println!("End-to-end:");
        println!("  cold_start (500 lines):    {:>6}us", cold_us);
        println!("  process_file (10 new):     {:>6}us  ({} events)", incr_us, events.len());
    }
}
