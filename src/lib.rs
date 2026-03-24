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
pub mod update;
pub mod writer;

pub use common::types::{UsageEvent, UsageEventWithTs, ModelUsageSummary, SessionGroup, TokiError};
pub use config::Config;

use std::collections::HashMap;
use std::sync::Arc;
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
        // Stop the worker thread first (it sends remaining ops to writers)
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
        let writer = DbWriter::new(db.clone(), db_rx, retention.clone());
        let provider_name = provider.name().to_string();
        let writer_handle = std::thread::Builder::new()
            .name(format!("toki-writer-{}", provider_name))
            .spawn(move || {
                writer.run();
            })
            .map_err(TokiError::Io)?;

        channel_map.insert(provider.name().to_string(), db_tx.clone());

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

    // Spawn worker thread
    let worker_handle = std::thread::Builder::new()
        .name("toki-worker".to_string())
        .spawn(move || {
            engine.watch_loop_providers(
                event_rx,
                stop_rx,
                &provider_channels,
            );
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

    Ok(Handle {
        stop_tx: Some(stop_tx),
        worker_handle: Some(worker_handle),
        _watchers: watchers,
        runtimes: runtime_handles,
        db: primary_db,
        provider_dbs,
    })
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
