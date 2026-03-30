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
pub mod sync;
pub mod update;
pub mod writer;

pub use common::types::{UsageEvent, UsageEventWithTs, ModelUsageSummary, SessionGroup, TokiError};
pub use config::Config;

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use db::Database;
use engine::TrackerEngine;
use providers::Provider;
use retention::RetentionPolicy;
use sink::Sink;
use writer::{DbOp, DbWriter};

/// Per-provider runtime state.
struct ProviderRuntime {
    provider: Box<dyn Provider>,
    db: Arc<Database>,
    db_tx: crossbeam_channel::Sender<DbOp>,
    writer_handle: JoinHandle<()>,
}

/// Running toki instance handle.
/// Drop triggers automatic stop().
pub struct Handle {
    stop_tx: Option<crossbeam_channel::Sender<()>>,
    worker_handle: Option<JoinHandle<()>>,
    // Keep watchers alive -- dropping them stops file watching
    _watchers: Vec<notify::RecommendedWatcher>,
    /// Provider runtimes (for shutdown ordering and DB access).
    runtimes: Vec<ProviderRuntimeHandle>,
    /// Primary DB handle for report queries (read-only from listener thread).
    /// Points to the first provider's DB for backward compat.
    db: Arc<Database>,
    /// All provider DBs for multi-provider report queries, with provider names.
    provider_dbs: Vec<(String, Arc<Database>)>,
    /// Sync thread stop channels + join handles (one per provider).
    sync_stops: Vec<crossbeam_channel::Sender<()>>,
    sync_threads: Vec<Option<JoinHandle<()>>>,
    /// Per-provider sync toggles for hot-reload.
    sync_toggles: Vec<(String, sync::SyncToggle)>,
    /// Per-provider flush notifiers — wake sync threads on shutdown.
    flush_notifies: Vec<sync::FlushNotify>,
    /// Settings watcher thread join handle.
    settings_watcher_handle: Option<JoinHandle<()>>,
    /// Settings watcher stop channel.
    settings_watcher_stop: Option<crossbeam_channel::Sender<()>>,
}

struct ProviderRuntimeHandle {
    db_tx: crossbeam_channel::Sender<DbOp>,
    writer_handle: Option<JoinHandle<()>>,
}

impl Handle {
    /// Gracefully stop toki: flush dirty checkpoints and join threads.
    pub fn stop(mut self) {
        self.shutdown();
    }

    /// Shared DB handle for report queries (first provider's DB for backward compat).
    pub fn db(&self) -> &Arc<Database> {
        &self.db
    }

    /// All provider DBs for report queries that need to merge across providers.
    /// Returns (provider_name, db) pairs.
    pub fn dbs(&self) -> Vec<(&str, &Arc<Database>)> {
        self.provider_dbs.iter().map(|(name, db)| (name.as_str(), db)).collect()
    }

    fn shutdown(&mut self) {
        // Stop settings watcher thread
        if let Some(tx) = self.settings_watcher_stop.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.settings_watcher_handle.take() {
            let _ = handle.join();
        }

        // Signal sync threads to stop
        for tx in &self.sync_stops {
            let _ = tx.send(());
        }
        // Wake sync threads that may be waiting on toggle or flush_notify
        for (_, toggle) in &self.sync_toggles {
            toggle.1.notify_one();
        }
        for flush in &self.flush_notifies {
            *flush.0.lock().unwrap() = true;
            flush.1.notify_one();
        }
        // Join sync threads before shutting down DB writers — they hold DB read handles
        for handle_opt in &mut self.sync_threads {
            if let Some(handle) = handle_opt.take() {
                let _ = handle.join();
            }
        }

        // Stop the worker thread (sends remaining ops to writers)
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }

        // Send shutdown to all writer threads first (parallel)
        for rt in &self.runtimes {
            let _ = rt.db_tx.send(DbOp::Shutdown);
        }
        // Then join all (they're already shutting down concurrently)
        for rt in &mut self.runtimes {
            if let Some(handle) = rt.writer_handle.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Start toki: cold start scan per provider, then enter watch mode.
/// Returns a Handle to control the running instance.
pub fn start(config: Config, sink: Box<dyn Sink>) -> Result<Handle, TokiError> {
    let retention = RetentionPolicy {
        event_retention_days: config.retention_days,
        rollup_retention_days: config.rollup_retention_days,
    };

    // Migrate legacy toki.fjall → claude_code.fjall if needed
    {
        let legacy_path = config.db_base_dir.join("toki.fjall");
        let new_path = config.db_base_dir.join("claude_code.fjall");
        if legacy_path.exists() && !new_path.exists() {
            match std::fs::rename(&legacy_path, &new_path) {
                Ok(()) => eprintln!("[toki] Migrated {} → {}", legacy_path.display(), new_path.display()),
                Err(e) => eprintln!("[toki] Migration failed ({} → {}): {}", legacy_path.display(), new_path.display(), e),
            }
        }
    }

    // Create providers from config
    let provider_list = providers::create_providers(&config.providers, &config);

    if provider_list.is_empty() {
        eprintln!("[toki] No providers configured.");
        eprintln!("[toki] Add a provider first:");
        eprintln!("[toki]   toki provider add claude_code");
        eprintln!("[toki]   toki provider add codex");
        return Err(TokiError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "No providers configured",
        )));
    }

    // Set up per-provider DB + writer
    let mut runtimes: Vec<ProviderRuntime> = Vec::new();
    let mut channel_map: HashMap<String, crossbeam_channel::Sender<DbOp>> = HashMap::new();
    let mut all_checkpoints: HashMap<String, common::types::FileCheckpoint> = HashMap::new();
    // (flush_notify, db, provider_name) — used to start sync threads after cold start
    let mut provider_sync_infos: Vec<(sync::FlushNotify, Arc<Database>, String)> = Vec::new();

    for provider in provider_list {
        // Skip providers whose root directory doesn't exist (e.g., Codex not installed)
        if provider.root_dir().is_none() {
            eprintln!("[toki] Skipping {}: data directory not found", provider.display_name());
            continue;
        }

        let db_path = config.db_base_dir.join(provider.db_dir_name());
        let db = Arc::new(Database::open(&db_path).map_err(TokiError::Db)?);

        // Load checkpoints from this provider's DB
        let provider_checkpoints = db.load_all_checkpoints()
            .map_err(TokiError::Db)?;
        for cp in provider_checkpoints {
            all_checkpoints.insert(cp.file_path.clone(), cp);
        }

        let (db_tx, db_rx) = crossbeam_channel::bounded::<DbOp>(1024);
        let flush_notify: sync::FlushNotify = Arc::new((Mutex::new(false), Condvar::new()));
        let mut writer = DbWriter::new(db.clone(), db_rx, retention.clone());
        writer.flush_notify = Some(flush_notify.clone());
        let provider_name = provider.name().to_string();
        let writer_handle = std::thread::Builder::new()
            .name(format!("toki-writer-{}", provider_name))
            .spawn(move || {
                writer.run();
            })
            .map_err(TokiError::Io)?;

        channel_map.insert(provider.name().to_string(), db_tx.clone());
        provider_sync_infos.push((flush_notify, db.clone(), provider_name.clone()));

        runtimes.push(ProviderRuntime {
            provider,
            db,
            db_tx,
            writer_handle,
        });
    }

    // Load pricing for real-time cost calculation
    let pricing = {
        let cache_path = pricing::default_cache_path();
        let p = pricing::fetch_pricing(&cache_path);
        if p.is_empty() { None } else { Some(p) }
    };

    // Create engine with all channels
    let mut engine = TrackerEngine::new(channel_map, all_checkpoints, sink, pricing);

    // Sequential cold start per provider
    println!("[toki] Running initial scan...");
    for rt in &runtimes {
        if rt.provider.root_dir().is_some() {
            eprintln!("[toki] Scanning {} ({})", rt.provider.display_name(),
                rt.provider.root_dir().unwrap_or_default());
            if let Err(e) = engine.cold_start_provider(rt.provider.as_ref(), &rt.db_tx) {
                eprintln!("[toki] Cold start error for {}: {}", rt.provider.name(), e);
            }
        }
    }

    // Set up file watchers — one per provider to avoid FSEvents stream
    // restart issues in notify where adding a second directory to an
    // existing FsEventsWatcher can silently fail to deliver events.
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<String>();
    let mut watchers: Vec<notify::RecommendedWatcher> = Vec::new();

    for rt in &runtimes {
        for dir in rt.provider.watch_dirs() {
            if std::path::Path::new(&dir).exists() {
                let mut watcher = platform::create_watcher(event_tx.clone())?;
                platform::watch_directory(&mut watcher, &dir)?;
                println!("[toki] Watching: {} ({})", dir, rt.provider.display_name());
                watchers.push(watcher);
            }
        }
    }

    // Stop channel
    let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);

    // Build provider+channel pairs for watch loop
    let mut provider_channels: Vec<(Box<dyn Provider>, crossbeam_channel::Sender<DbOp>)> = Vec::new();
    for rt in &runtimes {
        // We need to create new provider instances for the worker thread since we can't move
        // them out of runtimes (we still need runtimes for shutdown).
        // Instead, rebuild from config.
        match create_provider_instance(rt.provider.name(), rt.provider.root_dir()) {
            Ok(provider) => provider_channels.push((provider, rt.db_tx.clone())),
            Err(e) => {
                eprintln!("[toki] Skipping provider for watch loop: {}", e);
            }
        }
    }

    // Spawn worker thread. If this thread panics, events are silently lost,
    // so we treat a panic as fatal and exit the process (supervisor will restart).
    let worker_handle = std::thread::Builder::new()
        .name("toki-worker".to_string())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                engine.watch_loop_providers(
                    event_rx,
                    stop_rx,
                    &provider_channels,
                );
            }));

            if let Err(e) = result {
                let msg = if let Some(s) = e.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = e.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                eprintln!("[toki] FATAL: worker thread panicked: {}", msg);
                std::process::exit(1);
            }
        })
        .map_err(TokiError::Io)?;

    // Use first provider's DB as primary for report queries.
    // runtimes is guaranteed non-empty here because provider_list.is_empty() returns early above.
    let primary_db = runtimes.first()
        .expect("runtimes guaranteed non-empty: provider_list emptiness checked above")
        .db.clone();

    // Collect all provider DBs for multi-provider queries (with names)
    let provider_dbs: Vec<(String, Arc<Database>)> = runtimes.iter()
        .map(|rt| (rt.provider.name().to_string(), rt.db.clone()))
        .collect();

    let runtime_handles: Vec<ProviderRuntimeHandle> = runtimes
        .into_iter()
        .map(|rt| ProviderRuntimeHandle {
            db_tx: rt.db_tx,
            writer_handle: Some(rt.writer_handle),
        })
        .collect();

    // Check credentials file permissions before starting sync
    crate::sync::credentials::check_file_permissions();

    // Determine if sync is initially enabled
    let sync_initially_enabled = config::get_setting("sync_enabled")
        .map(|v| v == "true")
        .unwrap_or(false);

    // Always spawn sync threads — one per provider. They wait on SyncToggle when disabled.
    let mut sync_stops: Vec<crossbeam_channel::Sender<()>> = Vec::new();
    let mut sync_threads: Vec<Option<JoinHandle<()>>> = Vec::new();
    let mut sync_toggles: Vec<(String, sync::SyncToggle)> = Vec::new();
    let mut flush_notifies: Vec<sync::FlushNotify> = Vec::new();
    for (flush_notify, db, provider_name) in provider_sync_infos {
        flush_notifies.push(flush_notify.clone());
        let (sync_stop_tx, sync_stop_rx) = crossbeam_channel::bounded::<()>(1);
        let sync_toggle: sync::SyncToggle = Arc::new((
            Mutex::new(sync_initially_enabled),
            Condvar::new(),
        ));
        let handle = sync::start_sync_thread(
            db, flush_notify, sync_stop_rx, provider_name.clone(), sync_toggle.clone(),
        );
        sync_stops.push(sync_stop_tx);
        sync_threads.push(Some(handle));
        sync_toggles.push((provider_name, sync_toggle));
    }

    // Start settings file watcher for hot-reload (auto-respawns on panic)
    let (settings_stop_tx, settings_stop_rx) = crossbeam_channel::bounded::<()>(1);
    let settings_toggles = sync_toggles.clone();
    let settings_watcher_handle = std::thread::Builder::new()
        .name("toki-settings-watcher".to_string())
        .spawn(move || {
            loop {
                if settings_stop_rx.try_recv().is_ok() {
                    return;
                }

                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_settings_watcher(settings_stop_rx.clone(), settings_toggles.clone());
                }));

                match result {
                    Ok(()) => return, // Normal exit (stop signal)
                    Err(e) => {
                        let msg = if let Some(s) = e.downcast_ref::<&str>() {
                            s.to_string()
                        } else if let Some(s) = e.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "unknown panic".to_string()
                        };
                        eprintln!("[toki:settings-watcher] thread panicked: {}, restarting in 5s...", msg);

                        if settings_stop_rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok() {
                            return;
                        }
                    }
                }
            }
        })
        .map_err(TokiError::Io)?;

    Ok(Handle {
        stop_tx: Some(stop_tx),
        worker_handle: Some(worker_handle),
        _watchers: watchers,
        runtimes: runtime_handles,
        db: primary_db,
        provider_dbs,
        sync_stops,
        sync_threads,
        sync_toggles,
        flush_notifies,
        settings_watcher_handle: Some(settings_watcher_handle),
        settings_watcher_stop: Some(settings_stop_tx),
    })
}

/// Watch the settings sentinel file for changes and dispatch hot-reload updates.
/// Runs in its own thread. Polls via `notify` crate file watcher on the sentinel,
/// with a fallback poll every 10s.
fn run_settings_watcher(
    stop_rx: crossbeam_channel::Receiver<()>,
    sync_toggles: Vec<(String, sync::SyncToggle)>,
) {
    let sentinel_path = config::settings_sentinel_path();

    // Ensure sentinel file exists so the watcher has something to watch
    if let Some(parent) = sentinel_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if !sentinel_path.exists() {
        std::fs::write(&sentinel_path, "0").ok();
    }

    // Track last modification time to detect actual changes
    let mut last_mtime = std::fs::metadata(&sentinel_path)
        .and_then(|m| m.modified())
        .ok();

    // Set up file watcher on the sentinel file
    let (watch_tx, watch_rx) = crossbeam_channel::unbounded::<()>();
    let _watcher = {
        use notify::{RecursiveMode, Watcher, Event, EventKind};
        let tx = watch_tx.clone();
        let mut w = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                if matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                    let _ = tx.send(());
                }
            }
        });
        match w {
            Ok(ref mut watcher) => {
                // Watch the parent directory since the sentinel file may be recreated
                if let Some(parent) = sentinel_path.parent() {
                    let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
                }
            }
            Err(ref e) => {
                eprintln!("[toki:settings-watcher] failed to create watcher: {}, falling back to polling", e);
            }
        }
        w.ok()
    };

    // Poll interval as fallback
    let poll_tick = crossbeam_channel::tick(std::time::Duration::from_secs(10));

    loop {
        crossbeam_channel::select! {
            recv(stop_rx) -> _ => {
                return;
            }
            recv(watch_rx) -> _ => {
                // File watcher triggered — check if sentinel actually changed
            }
            recv(poll_tick) -> _ => {
                // Fallback poll
            }
        }

        // Check if the sentinel file was actually modified
        let current_mtime = std::fs::metadata(&sentinel_path)
            .and_then(|m| m.modified())
            .ok();
        if current_mtime == last_mtime {
            continue;
        }
        last_mtime = current_mtime;

        eprintln!("[toki:settings-watcher] settings change detected, reloading...");
        handle_settings_change(&sync_toggles);
    }
}

/// Handle a settings change by re-reading the settings DB and dispatching updates.
fn handle_settings_change(
    sync_toggles: &[(String, sync::SyncToggle)],
) {
    let sync_enabled = config::get_setting("sync_enabled")
        .map(|v| v == "true")
        .unwrap_or(false);

    // Update sync toggles for all providers
    for (provider_name, toggle) in sync_toggles {
        let mut enabled = toggle.0.lock().unwrap();
        let was_enabled = *enabled;
        *enabled = sync_enabled;
        if sync_enabled && !was_enabled {
            eprintln!("[toki:settings-watcher] sync enabled for {}", provider_name);
            toggle.1.notify_one();
        } else if !sync_enabled && was_enabled {
            eprintln!("[toki:settings-watcher] sync disabled for {}", provider_name);
            // Sync thread will notice on next toggle check
        }
    }
}

/// Create a provider instance by name (used to clone providers for worker thread).
/// Returns Err if the provider name is unknown.
fn create_provider_instance(name: &str, root_dir: Option<String>) -> Result<Box<dyn Provider>, String> {
    match name {
        "claude_code" => {
            let root = root_dir.unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join(".claude")
                    .to_string_lossy()
                    .to_string()
            });
            Ok(Box::new(providers::claude_code::ClaudeCodeProvider::new(root)))
        }
        "codex" => {
            let root = root_dir.unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join(".codex")
                    .to_string_lossy()
                    .to_string()
            });
            Ok(Box::new(providers::codex::CodexProvider::new(root)))
        }
        _ => Err(format!("unknown provider '{}'", name)),
    }
}
