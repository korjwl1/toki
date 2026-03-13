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
pub mod retention;
pub mod settings;
pub mod sink;
pub mod writer;

pub use common::types::{UsageEvent, UsageEventWithTs, ModelUsageSummary, SessionGroup, ClitraceError};
pub use config::Config;

use std::collections::HashMap;
use std::thread::JoinHandle;

use db::Database;
use engine::{ReportGroupBy, TrackerEngine};
use providers::claude_code::ClaudeCodeParser;
use retention::RetentionPolicy;
use sink::Sink;
use writer::{DbOp, DbWriter};

/// Running clitrace instance handle.
/// Drop triggers automatic stop().
pub struct Handle {
    stop_tx: Option<crossbeam_channel::Sender<()>>,
    db_tx: Option<crossbeam_channel::Sender<DbOp>>,
    worker_handle: Option<JoinHandle<()>>,
    writer_handle: Option<JoinHandle<()>>,
    // Keep watcher alive — dropping it stops file watching
    _watcher: notify::RecommendedWatcher,
}

impl Handle {
    /// Gracefully stop clitrace: flush dirty checkpoints and join threads.
    pub fn stop(mut self) {
        self.shutdown();
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

/// Start clitrace: cold start scan, then enter watch mode.
/// Returns a Handle to control the running instance.
pub fn start(config: Config, startup_group_by: Option<ReportGroupBy>, sink: Box<dyn Sink>) -> Result<Handle, ClitraceError> {
    // 1. Open DB and load checkpoints before spawning writer thread
    let db = Database::open(&config.db_path).map_err(|e| ClitraceError::Db(e.into()))?;

    // Full rescan: clear checkpoints.
    if config.full_rescan {
        eprintln!("[clitrace] Full rescan requested — clearing all checkpoints");
        db.clear_checkpoints().map_err(|e| ClitraceError::Db(e.into()))?;
    }

    // Fetch/load pricing
    let pricing_table = if config.no_cost {
        None
    } else {
        let p = pricing::fetch_pricing(&db);
        if p.is_empty() { None } else { Some(p) }
    };

    // Load checkpoints into memory
    let checkpoints: HashMap<String, common::types::FileCheckpoint> = db.load_all_checkpoints()
        .map_err(|e| ClitraceError::Db(e.into()))?
        .into_iter()
        .map(|cp| (cp.file_path.clone(), cp))
        .collect();

    // 2. Create bounded channel for writer thread
    let (db_tx, db_rx) = crossbeam_channel::bounded::<DbOp>(1024);

    // 3. Spawn writer thread (owns the Database)
    let retention = RetentionPolicy {
        event_retention_days: config.retention_days,
        rollup_retention_days: config.rollup_retention_days,
    };
    let writer = DbWriter::new(db, db_rx, retention);
    let writer_handle = std::thread::Builder::new()
        .name("clitrace-writer".to_string())
        .spawn(move || {
            writer.run();
        })
        .map_err(ClitraceError::Io)?;

    // 4. Create engine with db_tx and loaded checkpoints
    let mut engine = TrackerEngine::new(db_tx.clone(), checkpoints)
        .with_sink(sink)
        .with_session_filter(config.session_filter.clone())
        .with_project_filter(config.project_filter.clone())
        .with_tz(config.tz)
        .with_pricing(pricing_table);

    let parser = ClaudeCodeParser;
    let root_dir = config.claude_code_root.clone();

    // Cold start
    println!("[clitrace] Running initial scan...");
    if let Some(group_by) = startup_group_by {
        if let Err(e) = engine.cold_start_grouped(&parser, &root_dir, group_by) {
            eprintln!("[clitrace] Cold start error: {}", e);
        }
    } else if let Err(e) = engine.cold_start(&parser, &root_dir) {
        eprintln!("[clitrace] Cold start error: {}", e);
    }

    // Set up file watcher
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<String>();
    let mut watcher = platform::create_watcher(event_tx)?;

    // Watch the projects directory under claude root
    let projects_dir = format!("{}/projects", root_dir);
    if std::path::Path::new(&projects_dir).exists() {
        platform::watch_directory(&mut watcher, &projects_dir)?;
        println!("[clitrace] Watching: {}", projects_dir);
    } else {
        platform::watch_directory(&mut watcher, &root_dir)?;
        println!("[clitrace] Watching: {}", root_dir);
    }

    // Stop channel
    let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);

    // Spawn worker thread
    let worker_handle = std::thread::Builder::new()
        .name("clitrace-worker".to_string())
        .spawn(move || {
            engine.watch_loop(
                event_rx,
                stop_rx,
                &parser,
            );
        })
        .map_err(ClitraceError::Io)?;

    Ok(Handle {
        stop_tx: Some(stop_tx),
        db_tx: Some(db_tx),
        worker_handle: Some(worker_handle),
        writer_handle: Some(writer_handle),
        _watcher: watcher,
    })
}
