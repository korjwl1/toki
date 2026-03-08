pub mod common;
pub mod config;
pub mod db;
pub mod engine;
pub mod checkpoint;
pub mod platform;
pub mod providers;

pub use common::types::{UsageEvent, ModelUsageSummary, SessionGroup, WebtraceError};
pub use config::Config;

use std::thread::JoinHandle;
use std::time::Duration;

use db::Database;
use engine::TrackerEngine;
use providers::claude_code::ClaudeCodeParser;

/// Running webtrace instance handle.
/// Drop triggers automatic stop().
pub struct Handle {
    stop_tx: Option<crossbeam_channel::Sender<()>>,
    worker_handle: Option<JoinHandle<()>>,
    // Keep watcher alive — dropping it stops file watching
    _watcher: notify::RecommendedWatcher,
}

impl Handle {
    /// Gracefully stop webtrace: flush dirty checkpoints and join worker thread.
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

/// Start webtrace: cold start scan, then enter watch mode.
/// Returns a Handle to control the running instance.
pub fn start(config: Config) -> Result<Handle, WebtraceError> {
    let db = Database::open(&config.db_path).map_err(|e| WebtraceError::Db(e.into()))?;

    let mut engine = TrackerEngine::new(db);
    engine.load_checkpoints().map_err(|e| WebtraceError::Db(e.into()))?;

    let parser = ClaudeCodeParser;
    let root_dir = config.claude_code_root.clone();

    // Cold start
    println!("[webtrace] Running initial scan...");
    if let Err(e) = engine.cold_start(&parser, &root_dir) {
        eprintln!("[webtrace] Cold start error: {}", e);
    }

    // Set up file watcher
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<String>();
    let mut watcher = platform::create_watcher(event_tx)?;

    // Watch the projects directory under claude root
    let projects_dir = format!("{}/projects", root_dir);
    if std::path::Path::new(&projects_dir).exists() {
        platform::watch_directory(&mut watcher, &projects_dir)?;
        println!("[webtrace] Watching: {}", projects_dir);
    } else {
        platform::watch_directory(&mut watcher, &root_dir)?;
        println!("[webtrace] Watching: {}", root_dir);
    }

    // Stop channel
    let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);

    let poll_interval = Duration::from_secs(config.poll_interval_secs);

    // Spawn worker thread
    let worker_handle = std::thread::Builder::new()
        .name("webtrace-worker".to_string())
        .spawn(move || {
            engine.watch_loop(
                event_rx,
                stop_rx,
                &parser,
                &root_dir,
                poll_interval,
            );
        })
        .map_err(|e| WebtraceError::Io(e))?;

    Ok(Handle {
        stop_tx: Some(stop_tx),
        worker_handle: Some(worker_handle),
        _watcher: watcher,
    })
}
