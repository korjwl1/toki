pub mod common;
pub mod config;
pub mod daemon;
pub mod db;
pub mod engine;
pub mod checkpoint;
pub mod platform;
pub mod pricing;
pub mod providers;
pub mod query;
pub mod query_parser;
pub mod retention;
pub mod settings;
pub mod sink;
pub mod writer;

pub use common::types::{UsageEvent, UsageEventWithTs, ModelUsageSummary, SessionGroup, TokiError};
pub use config::Config;

use std::collections::HashMap;
use std::sync::Arc;
use std::thread::JoinHandle;

use db::Database;
use engine::TrackerEngine;
use providers::claude_code::ClaudeCodeParser;
use retention::RetentionPolicy;
use sink::Sink;
use writer::{DbOp, DbWriter};

/// Running toki instance handle.
/// Drop triggers automatic stop().
pub struct Handle {
    stop_tx: Option<crossbeam_channel::Sender<()>>,
    db_tx: Option<crossbeam_channel::Sender<DbOp>>,
    worker_handle: Option<JoinHandle<()>>,
    writer_handle: Option<JoinHandle<()>>,
    // Keep watcher alive — dropping it stops file watching
    _watcher: notify::RecommendedWatcher,
    /// Shared DB handle for report queries (read-only from listener thread).
    db: Arc<Database>,
}

impl Handle {
    /// Gracefully stop toki: flush dirty checkpoints and join threads.
    pub fn stop(mut self) {
        self.shutdown();
    }

    /// Shared DB handle for report queries.
    pub fn db(&self) -> &Arc<Database> {
        &self.db
    }

    fn shutdown(&mut self) {
        // Stop the worker thread first (it sends remaining ops to writer)
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }

        // Then shutdown the writer thread
        if let Some(tx) = self.db_tx.take() {
            let _ = tx.send(DbOp::Shutdown);
        }
        if let Some(handle) = self.writer_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Start toki: cold start scan, then enter watch mode.
/// Returns a Handle to control the running instance.
pub fn start(config: Config, sink: Box<dyn Sink>) -> Result<Handle, TokiError> {
    // 1. Open DB and load checkpoints before spawning writer thread
    let db = Arc::new(Database::open(&config.db_path).map_err(TokiError::Db)?);

    // Load checkpoints into memory
    let checkpoints: HashMap<String, common::types::FileCheckpoint> = db.load_all_checkpoints()
        .map_err(TokiError::Db)?
        .into_iter()
        .map(|cp| (cp.file_path.clone(), cp))
        .collect();

    // 2. Create bounded channel for writer thread
    let (db_tx, db_rx) = crossbeam_channel::bounded::<DbOp>(1024);

    // 3. Spawn writer thread (shares DB via Arc)
    let retention = RetentionPolicy {
        event_retention_days: config.retention_days,
        rollup_retention_days: config.rollup_retention_days,
    };
    let writer = DbWriter::new(db.clone(), db_rx, retention);
    let writer_handle = std::thread::Builder::new()
        .name("toki-writer".to_string())
        .spawn(move || {
            writer.run();
        })
        .map_err(TokiError::Io)?;

    // 4. Create engine with db_tx and loaded checkpoints
    let mut engine = TrackerEngine::new(db_tx.clone(), checkpoints, sink);

    let parser = ClaudeCodeParser;
    let root_dir = config.claude_code_root.clone();

    // Cold start — full scan, index everything into TSDB
    println!("[toki] Running initial scan...");
    if let Err(e) = engine.cold_start(&parser, &root_dir) {
        eprintln!("[toki] Cold start error: {}", e);
    }

    // Set up file watcher
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<String>();
    let mut watcher = platform::create_watcher(event_tx)?;

    // Watch the projects directory under claude root
    let projects_dir = format!("{}/projects", root_dir);
    if std::path::Path::new(&projects_dir).exists() {
        platform::watch_directory(&mut watcher, &projects_dir)?;
        println!("[toki] Watching: {}", projects_dir);
    } else {
        platform::watch_directory(&mut watcher, &root_dir)?;
        println!("[toki] Watching: {}", root_dir);
    }

    // Stop channel
    let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);

    // Spawn worker thread
    let worker_handle = std::thread::Builder::new()
        .name("toki-worker".to_string())
        .spawn(move || {
            engine.watch_loop(
                event_rx,
                stop_rx,
                &parser,
            );
        })
        .map_err(TokiError::Io)?;

    Ok(Handle {
        stop_tx: Some(stop_tx),
        db_tx: Some(db_tx),
        worker_handle: Some(worker_handle),
        writer_handle: Some(writer_handle),
        _watcher: watcher,
        db,
    })
}
