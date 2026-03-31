use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crossbeam_channel::Receiver;

use crate::common::types::{FileCheckpoint, RollupValue, StoredEvent, TokenFields};
use crate::db::Database;
use crate::retention::{RetentionPolicy, run_retention};

/// Event data for cold start bulk write.
/// Uses Arc<str> for session_id/source_file to avoid per-event String clones.
pub struct ColdStartEvent {
    pub ts_ms: i64,
    pub message_id: String,
    pub model: String,
    pub session_id: std::sync::Arc<str>,
    pub source_file: std::sync::Arc<str>,
    pub project_name: Option<std::sync::Arc<str>>,
    pub tokens: TokenFields,
}

/// Data for a single watch-mode event write. Boxed inside DbOp to keep the
/// enum small (~32 bytes instead of ~160), improving channel buffer cache locality.
pub struct WriteEventData {
    pub ts_ms: i64,
    pub message_id: String,
    pub model: String,
    pub session_id: String,
    pub source_file: String,
    pub project_name: Option<String>,
    pub tokens: TokenFields,
}

/// Operations sent to the writer thread via bounded channel.
pub enum DbOp {
    WriteEvent(Box<WriteEventData>),
    /// Bulk write chunk for cold start -- events from one file.
    /// Rollups are accumulated in memory until FlushBulkRollups.
    BulkWrite(Vec<ColdStartEvent>),
    /// Flush accumulated rollups from cold start bulk writes. Signals done.
    FlushBulkRollups(crossbeam_channel::Sender<()>),
    WriteCheckpoint(FileCheckpoint),
    FlushCheckpoints(Vec<FileCheckpoint>),
    Shutdown,
}

/// Batch size: commit after this many events accumulate.
const BATCH_SIZE: usize = 64;

pub struct DbWriter {
    db: Arc<Database>,
    op_rx: Receiver<DbOp>,
    dict_cache: HashMap<String, u32>,
    next_dict_id: u32,
    pending_events: Vec<PendingEvent>,
    retention: RetentionPolicy,
    /// Accumulated rollups during cold start bulk writes.
    bulk_rollups: HashMap<(i64, String), RollupValue>,
    /// Accumulated events during cold start bulk writes (flushed at BULK_BATCH_SIZE).
    bulk_pending: Vec<ColdStartEvent>,
    /// Sync notification: set dirty=true + notify after each flush so the sync thread wakes.
    /// None when sync is not configured.
    pub flush_notify: Option<Arc<(Mutex<bool>, Condvar)>>,
}

struct PendingEvent {
    ts_ms: i64,
    message_id: String,
    model: String,
    session_id: String,
    source_file: String,
    project_name: Option<String>,
    tokens: TokenFields,
}

impl DbWriter {
    pub fn new(db: Arc<Database>, op_rx: Receiver<DbOp>, retention: RetentionPolicy) -> Self {
        // Load existing dictionary into cache
        let dict_cache = db.load_dict_forward().unwrap_or_default();
        let next_dict_id = dict_cache.values().max().map_or(0, |&v| v + 1);

        DbWriter {
            db,
            op_rx,
            dict_cache,
            next_dict_id,
            pending_events: Vec::with_capacity(BATCH_SIZE),
            retention,
            bulk_rollups: HashMap::new(),
            bulk_pending: Vec::new(),
            flush_notify: None,
        }
    }

    /// Main run loop for the writer thread.
    pub fn run(mut self) {
        // Run retention on startup
        match run_retention(&self.db, &self.retention) {
            Ok(stats) => {
                if stats.events_deleted > 0 || stats.rollups_deleted > 0 {
                    eprintln!("[toki:writer] retention cleanup: {} events, {} rollups deleted ({}ms)",
                        stats.events_deleted, stats.rollups_deleted, stats.elapsed.as_millis());
                }
            }
            Err(e) => eprintln!("[toki:writer] retention error: {}", e),
        }

        // Periodic flush: commit pending events even if batch isn't full (1s interval)
        let flush_tick = crossbeam_channel::tick(std::time::Duration::from_secs(1));
        // Daily retention tick
        let retention_tick = crossbeam_channel::tick(std::time::Duration::from_secs(86400));

        loop {
            crossbeam_channel::select! {
                recv(self.op_rx) -> msg => {
                    match msg {
                        Ok(op) => {
                            if !self.handle_op(op) {
                                // Shutdown requested
                                self.flush_pending();
                                return;
                            }
                        }
                        Err(_) => {
                            // Channel closed
                            self.flush_pending();
                            return;
                        }
                    }
                }
                recv(flush_tick) -> _ => {
                    self.flush_pending();
                }
                recv(retention_tick) -> _ => {
                    match run_retention(&self.db, &self.retention) {
                        Ok(stats) => {
                            if stats.events_deleted > 0 || stats.rollups_deleted > 0 {
                                eprintln!("[toki:writer] daily retention: {} events, {} rollups deleted",
                                    stats.events_deleted, stats.rollups_deleted);
                            }
                        }
                        Err(e) => eprintln!("[toki:writer] retention error: {}", e),
                    }
                }
            }
        }
    }

    /// Handle a single operation. Returns false on Shutdown.
    fn handle_op(&mut self, op: DbOp) -> bool {
        match op {
            DbOp::WriteEvent(data) => {
                self.pending_events.push(PendingEvent {
                    ts_ms: data.ts_ms,
                    message_id: data.message_id,
                    model: data.model,
                    session_id: data.session_id,
                    source_file: data.source_file,
                    project_name: data.project_name,
                    tokens: data.tokens,
                });
                if self.pending_events.len() >= BATCH_SIZE {
                    self.flush_pending();
                }
                true
            }
            DbOp::BulkWrite(events) => {
                self.bulk_write_chunk(events);
                true
            }
            DbOp::FlushBulkRollups(done_tx) => {
                self.flush_bulk_rollups();
                let _ = done_tx.send(());
                true
            }
            DbOp::WriteCheckpoint(cp) => {
                if let Err(e) = self.db.upsert_checkpoint(&cp) {
                    eprintln!("[toki:writer] checkpoint error: {}", e);
                }
                true
            }
            DbOp::FlushCheckpoints(cps) => {
                if let Err(e) = self.db.flush_checkpoints(&cps) {
                    eprintln!("[toki:writer] flush checkpoints error: {}", e);
                }
                true
            }
            DbOp::Shutdown => false,
        }
    }

    /// Flush pending events as a batch transaction.
    /// Note: does a DB read per unique (hour, model) to fetch current rollup values.
    /// This is acceptable for the current BATCH_SIZE (64) — at most ~64 reads per flush.
    fn flush_pending(&mut self) {
        if self.pending_events.is_empty() {
            return;
        }

        let t = Instant::now();
        // Drain events to avoid borrow conflicts
        let events: Vec<PendingEvent> = self.pending_events.drain(..).collect();
        let count = events.len();

        // Build batch transaction with dedup: same msg_id replaces previous event.
        let mut batch = self.db.batch();
        let mut rollup_updates: HashMap<(i64, &str), RollupValue> = HashMap::new();

        for event in &events {
            let model_id = self.resolve_dict_id(&mut batch, &event.model);
            let session_id = self.resolve_dict_id(&mut batch, &event.session_id);
            let source_file_id = self.resolve_dict_id(&mut batch, &event.source_file);
            let project_name_id = match event.project_name.as_deref() {
                Some(p) if !p.is_empty() => self.resolve_dict_id(&mut batch, p),
                _ => 0,
            };

            let stored = StoredEvent {
                model_id,
                session_id,
                source_file_id,
                project_name_id,
                input_tokens: event.tokens.input_tokens,
                output_tokens: event.tokens.output_tokens,
                cache_creation_input_tokens: event.tokens.cache_creation_input_tokens,
                cache_read_input_tokens: event.tokens.cache_read_input_tokens,
            };

            // Dedup insert: delete previous event with same msg_id, get old tokens
            let prev = self.db.insert_event_dedup(&mut batch, event.ts_ms, &event.message_id, &stored);

            // Rollup: add new tokens, subtract old if replaced
            let hour_ts = event.ts_ms - (event.ts_ms % 3_600_000);
            let key = (hour_ts, event.model.as_str());
            let rollup = rollup_updates.entry(key).or_insert_with(|| {
                self.db.get_rollup(hour_ts, &event.model).ok().flatten().unwrap_or_default()
            });
            rollup.input += event.tokens.input_tokens;
            rollup.output += event.tokens.output_tokens;
            rollup.cache_create += event.tokens.cache_creation_input_tokens;
            rollup.cache_read += event.tokens.cache_read_input_tokens;
            rollup.count += 1;

            // Subtract previous event's tokens if it was in the same hour bucket
            if let Some((prev_ts, prev_event)) = prev {
                let prev_hour = prev_ts - (prev_ts % 3_600_000);
                if prev_hour == hour_ts {
                    // Same hour bucket — subtract from this rollup
                    rollup.input = rollup.input.saturating_sub(prev_event.input_tokens);
                    rollup.output = rollup.output.saturating_sub(prev_event.output_tokens);
                    rollup.cache_create = rollup.cache_create.saturating_sub(prev_event.cache_creation_input_tokens);
                    rollup.cache_read = rollup.cache_read.saturating_sub(prev_event.cache_read_input_tokens);
                    rollup.count = rollup.count.saturating_sub(1);
                } else {
                    // Different hour bucket — need to fix that rollup too
                    let prev_key = (prev_hour, event.model.as_str());
                    let prev_rollup = rollup_updates.entry(prev_key).or_insert_with(|| {
                        self.db.get_rollup(prev_hour, &event.model).ok().flatten().unwrap_or_default()
                    });
                    prev_rollup.input = prev_rollup.input.saturating_sub(prev_event.input_tokens);
                    prev_rollup.output = prev_rollup.output.saturating_sub(prev_event.output_tokens);
                    prev_rollup.cache_create = prev_rollup.cache_create.saturating_sub(prev_event.cache_creation_input_tokens);
                    prev_rollup.cache_read = prev_rollup.cache_read.saturating_sub(prev_event.cache_read_input_tokens);
                    prev_rollup.count = prev_rollup.count.saturating_sub(1);
                }
            }

            // Session index
            self.db.insert_session_index(&mut batch, &event.session_id, event.ts_ms, &event.message_id);

            // Project index
            if let Some(ref project) = event.project_name {
                self.db.insert_project_index(&mut batch, project, event.ts_ms, &event.message_id);
            }
        }

        // Write rollups
        for (&(hour_ts, model), rollup) in &rollup_updates {
            self.db.upsert_rollup(&mut batch, hour_ts, model, rollup);
        }

        if let Err(e) = batch.commit() {
            eprintln!("[toki:writer] batch commit error: {}", e);
        }

        if crate::engine::debug_level() >= 1 {
            eprintln!("[toki:writer] flushed {} events, {} rollups in {}µs",
                count, rollup_updates.len(), t.elapsed().as_micros());
        }

        // Notify sync thread that new data is available
        if let Some(ref notify) = self.flush_notify {
            let (lock, cvar) = notify.as_ref();
            *lock.lock().unwrap() = true;
            cvar.notify_one();
        }
    }

    /// Buffer a chunk of cold start events. Flushes to DB when buffer reaches BULK_BATCH_SIZE.
    ///
    /// Rollup accumulation is deferred until flush time. This avoids double-counting
    /// when the same event_key appears multiple times in JSONL (e.g. streaming progress
    /// updates followed by the final result — same msg_id:timestamp).
    /// Events keyspace uses insert (last write wins for same key), so only the final
    /// value survives. Rollups must match: we deduplicate by event_key before accumulating.
    fn bulk_write_chunk(&mut self, mut events: Vec<ColdStartEvent>) {
        // Buffer events, flush when large enough
        self.bulk_pending.append(&mut events);
        if self.bulk_pending.len() >= 1024 {
            self.flush_bulk_events();
        }
    }

    /// Flush buffered bulk events to DB.
    fn flush_bulk_events(&mut self) {
        if self.bulk_pending.is_empty() {
            return;
        }

        let events = std::mem::take(&mut self.bulk_pending);

        let mut batch = self.db.batch();
        for event in &events {
            let model_id = self.resolve_dict_id(&mut batch, &event.model);
            let session_id = self.resolve_dict_id(&mut batch, &event.session_id);
            let source_file_id = self.resolve_dict_id(&mut batch, &event.source_file);
            let project_name_id = match event.project_name.as_deref() {
                Some(p) if !p.is_empty() => self.resolve_dict_id(&mut batch, p),
                _ => 0,
            };

            let stored = StoredEvent {
                model_id,
                session_id,
                source_file_id,
                project_name_id,
                input_tokens: event.tokens.input_tokens,
                output_tokens: event.tokens.output_tokens,
                cache_creation_input_tokens: event.tokens.cache_creation_input_tokens,
                cache_read_input_tokens: event.tokens.cache_read_input_tokens,
            };

            // Use dedup insert: engine already deduped by msg_id, but idx_msg
            // still needs to be populated for watch-mode dedup after cold start.
            self.db.insert_event_dedup(&mut batch, event.ts_ms, &event.message_id, &stored);
            self.db.insert_session_index(&mut batch, &event.session_id, event.ts_ms, &event.message_id);

            if let Some(ref project) = event.project_name {
                self.db.insert_project_index(&mut batch, project, event.ts_ms, &event.message_id);
            }
        }

        if let Err(e) = batch.commit() {
            eprintln!("[toki:writer] bulk batch commit error: {}", e);
        }
    }

    /// Flush remaining bulk events + build rollups from events keyspace.
    ///
    /// Events keyspace uses insert (last-write-wins for same key), so duplicate
    /// event_keys (e.g. streaming progress + final result in JSONL) are naturally
    /// deduplicated. We build rollups by scanning events keyspace after all inserts,
    /// guaranteeing rollup counts match event counts exactly.
    fn flush_bulk_rollups(&mut self) {
        let t = Instant::now();

        // Flush remaining buffered events
        self.flush_bulk_events();

        // Build rollups from events keyspace (source of truth after dedup)
        let dict = match self.db.load_dict_reverse() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[toki:writer] dict load error: {}", e);
                return;
            }
        };
        let unknown = String::new();
        let mut rollups: std::collections::HashMap<(i64, String), RollupValue> = std::collections::HashMap::new();

        let _ = self.db.for_each_event(0, i64::MAX, |ts, event| {
            let model = dict.get(&event.model_id).unwrap_or(&unknown).clone();
            let hour_ts = ts - (ts % 3_600_000);
            let rollup = rollups.entry((hour_ts, model)).or_default();
            rollup.input += event.input_tokens;
            rollup.output += event.output_tokens;
            rollup.cache_create += event.cache_creation_input_tokens;
            rollup.cache_read += event.cache_read_input_tokens;
            rollup.count += 1;
        });

        if !rollups.is_empty() {
            let count = rollups.len();
            let mut batch = self.db.batch();
            for ((hour_ts, model), rollup) in &rollups {
                self.db.upsert_rollup(&mut batch, hour_ts.clone(), model, rollup);
            }
            if let Err(e) = batch.commit() {
                eprintln!("[toki:writer] bulk rollup commit error: {}", e);
            }

            if crate::engine::debug_level() >= 1 {
                eprintln!("[toki:writer] bulk flush complete: {} rollups in {}µs", count, t.elapsed().as_micros());
            }
        }

        self.bulk_rollups.clear();
    }

    /// Resolve a string to a dict ID, inserting if new.
    fn resolve_dict_id(&mut self, batch: &mut fjall::OwnedWriteBatch, key: &str) -> u32 {
        if let Some(&id) = self.dict_cache.get(key) {
            return id;
        }
        let id = self.next_dict_id;
        self.next_dict_id += 1;
        self.dict_cache.insert(key.to_string(), id);
        self.db.dict_put(batch, key, id);
        id
    }
}
