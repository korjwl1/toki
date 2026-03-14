use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::Receiver;

use crate::common::types::{FileCheckpoint, RollupValue, StoredEvent, TokenFields};
use crate::db::Database;
use crate::engine::extract_project_name;
use crate::retention::{RetentionPolicy, run_retention};

/// Operations sent to the writer thread via bounded channel.
pub enum DbOp {
    WriteEvent {
        ts_ms: i64,
        message_id: String,
        model: String,
        session_id: String,
        source_file: String,
        tokens: TokenFields,
    },
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
}

struct PendingEvent {
    ts_ms: i64,
    message_id: String,
    model: String,
    session_id: String,
    source_file: String,
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
            DbOp::WriteEvent { ts_ms, message_id, model, session_id, source_file, tokens } => {
                self.pending_events.push(PendingEvent {
                    ts_ms, message_id, model, session_id, source_file, tokens,
                });
                if self.pending_events.len() >= BATCH_SIZE {
                    self.flush_pending();
                }
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

            // Project index (extract from source_file path)
            if let Some(project) = extract_project_name(&event.source_file) {
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
