pub mod common;
pub mod config;
pub mod db;
pub mod engine;
pub mod checkpoint;
pub mod platform;
pub mod providers;

pub use common::types::{UsageEvent, UsageEventWithTs, ModelUsageSummary, SessionGroup, ClitraceError};
pub use config::Config;

use std::thread::JoinHandle;

use db::Database;
use engine::{OutputFormat, ReportGroupBy, TrackerEngine};
use providers::claude_code::ClaudeCodeParser;

/// Running clitrace instance handle.
/// Drop triggers automatic stop().
pub struct Handle {
    stop_tx: Option<crossbeam_channel::Sender<()>>,
    worker_handle: Option<JoinHandle<()>>,
    // Keep watcher alive — dropping it stops file watching
    _watcher: notify::RecommendedWatcher,
}

impl Handle {
    /// Gracefully stop clitrace: flush dirty checkpoints and join worker thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.worker_handle.take() {
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
pub fn start(config: Config, startup_group_by: Option<ReportGroupBy>, output_format: OutputFormat) -> Result<Handle, ClitraceError> {
    let db = Database::open(&config.db_path).map_err(|e| ClitraceError::Db(e.into()))?;

    // Full rescan: clear checkpoints.
    if config.full_rescan {
        eprintln!("[clitrace] Full rescan requested — clearing all checkpoints");
        db.clear_checkpoints().map_err(|e| ClitraceError::Db(e.into()))?;
    }

    let mut engine = TrackerEngine::new(db)
        .with_output_format(output_format)
        .with_session_filter(config.session_filter.clone());
    engine.load_checkpoints().map_err(|e| ClitraceError::Db(e.into()))?;

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
        .map_err(|e| ClitraceError::Io(e))?;

    Ok(Handle {
        stop_tx: Some(stop_tx),
        worker_handle: Some(worker_handle),
        _watcher: watcher,
    })
}
