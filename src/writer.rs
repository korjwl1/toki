use std::collections::HashMap;
use std::sync::Arc;
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

/// Operations sent to the writer thread via bounded channel.
pub enum DbOp {
    WriteEvent {
        ts_ms: i64,
        message_id: String,
        model: String,
        session_id: String,
        source_file: String,
        project_name: Option<String>,
        tokens: TokenFields,
    },
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
            DbOp::WriteEvent { ts_ms, message_id, model, session_id, source_file, project_name, tokens } => {
                self.pending_events.push(PendingEvent {
                    ts_ms, message_id, model, session_id, source_file, project_name, tokens,
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

        // First pass: resolve rollups (need to read current values before batch)
        let mut rollup_updates: HashMap<(i64, &str), RollupValue> = HashMap::new();
        for event in &events {
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
        }

        // Build batch transaction
        let mut batch = self.db.batch();

        // Resolve dict IDs and insert new entries
        for event in &events {
            let model_id = self.resolve_dict_id(&mut batch, &event.model);
            let session_id = self.resolve_dict_id(&mut batch, &event.session_id);
            let source_file_id = self.resolve_dict_id(&mut batch, &event.source_file);

            let stored = StoredEvent {
                model_id,
                session_id,
                source_file_id,
                input_tokens: event.tokens.input_tokens,
                output_tokens: event.tokens.output_tokens,
                cache_creation_input_tokens: event.tokens.cache_creation_input_tokens,
                cache_read_input_tokens: event.tokens.cache_read_input_tokens,
            };

            self.db.insert_event_batch(&mut batch, event.ts_ms, &event.message_id, &stored);

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
    }

    /// Buffer a chunk of cold start events. Flushes to DB when buffer reaches BULK_BATCH_SIZE.
    fn bulk_write_chunk(&mut self, mut events: Vec<ColdStartEvent>) {
        // Accumulate rollups in memory (no DB reads).
        // Note: model.clone() is acceptable here — this only runs during cold start (one-time).
        for event in &events {
            let hour_ts = event.ts_ms - (event.ts_ms % 3_600_000);
            let rollup = self.bulk_rollups.entry((hour_ts, event.model.clone())).or_default();
            rollup.input += event.tokens.input_tokens;
            rollup.output += event.tokens.output_tokens;
            rollup.cache_create += event.tokens.cache_creation_input_tokens;
            rollup.cache_read += event.tokens.cache_read_input_tokens;
            rollup.count += 1;
        }

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

        let events: Vec<ColdStartEvent> = self.bulk_pending.drain(..).collect();
        let mut batch = self.db.batch();
        for event in &events {
            let model_id = self.resolve_dict_id(&mut batch, &event.model);
            let session_id = self.resolve_dict_id(&mut batch, &event.session_id);
            let source_file_id = self.resolve_dict_id(&mut batch, &event.source_file);

            let stored = StoredEvent {
                model_id,
                session_id,
                source_file_id,
                input_tokens: event.tokens.input_tokens,
                output_tokens: event.tokens.output_tokens,
                cache_creation_input_tokens: event.tokens.cache_creation_input_tokens,
                cache_read_input_tokens: event.tokens.cache_read_input_tokens,
            };

            self.db.insert_event_batch(&mut batch, event.ts_ms, &event.message_id, &stored);
            self.db.insert_session_index(&mut batch, &event.session_id, event.ts_ms, &event.message_id);

            if let Some(ref project) = event.project_name {
                self.db.insert_project_index(&mut batch, project, event.ts_ms, &event.message_id);
            }
        }

        if let Err(e) = batch.commit() {
            eprintln!("[toki:writer] bulk batch commit error: {}", e);
        }
    }

    /// Flush remaining bulk events + accumulated rollups, then signal completion.
    fn flush_bulk_rollups(&mut self) {
        let t = Instant::now();

        // Flush remaining buffered events
        self.flush_bulk_events();

        // Write rollups in a single batch
        if !self.bulk_rollups.is_empty() {
            let count = self.bulk_rollups.len();
            let mut batch = self.db.batch();
            for ((hour_ts, model), rollup) in &self.bulk_rollups {
                self.db.upsert_rollup(&mut batch, *hour_ts, model, rollup);
            }
            if let Err(e) = batch.commit() {
                eprintln!("[toki:writer] bulk rollup commit error: {}", e);
            }
            self.bulk_rollups.clear();

            if crate::engine::debug_level() >= 1 {
                eprintln!("[toki:writer] bulk flush complete: {} rollups in {}µs", count, t.elapsed().as_micros());
            }
        }
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
