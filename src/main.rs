use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use clap::{Parser, Subcommand};
use clitrace::Config;
use fs2::FileExt;

// Global flag for signal handler
static RUNNING: std::sync::atomic::AtomicBool = AtomicBool::new(true);

#[derive(Parser)]
#[command(name = "clitrace", version, about = "AI CLI tool token usage tracker")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Watch mode: live tracking with checkpoints (single-instance only).
    Trace {
        /// Claude Code root directory (default: ~/.claude)
        #[arg(long)]
        claude_root: Option<String>,
        /// DB path for checkpoints (default: ~/.config/clitrace/clitrace.db)
        #[arg(long)]
        db_path: Option<PathBuf>,
        /// Clear checkpoints and perform full rescan on startup
        #[arg(long)]
        full_rescan: bool,
    },
    /// Report mode: one-shot summary without writing checkpoints.
    Report {
        /// Claude Code root directory (default: ~/.claude)
        #[arg(long)]
        claude_root: Option<String>,
        /// Group summary by: hour, date, month, year
        #[arg(long)]
        group_by: Option<String>,
    },
}

fn build_config(claude_root: Option<String>, db_path: Option<PathBuf>) -> Config {
    let mut config = Config::new();

    if let Ok(root) = std::env::var("CLITRACE_CLAUDE_ROOT") {
        config = config.with_claude_root(root);
    }
    if let Ok(path) = std::env::var("CLITRACE_DB_PATH") {
        config = config.with_db_path(path.into());
    }

    if let Some(root) = claude_root {
        config = config.with_claude_root(root);
    }
    if let Some(path) = db_path {
        config = config.with_db_path(path);
    }

    config
}

fn acquire_trace_lock(db_path: &PathBuf) -> std::io::Result<std::fs::File> {
    let lock_path = db_path.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    file.try_lock_exclusive()?;
    Ok(file)
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Trace { claude_root, db_path, full_rescan } => {
            let mut config = build_config(claude_root, db_path);
            if full_rescan {
                config = config.with_full_rescan(true);
            }

            let _lock = match acquire_trace_lock(&config.db_path) {
                Ok(f) => f,
                Err(_) => {
                    eprintln!("[clitrace] Another trace instance is already running.");
                    std::process::exit(1);
                }
            };

            println!("[clitrace] Starting trace...");
            println!("[clitrace] Claude Code root: {}", config.claude_code_root);
            println!("[clitrace] Database: {}", config.db_path.display());

            let handle = match clitrace::start(config) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("[clitrace] Failed to start: {}", e);
                    std::process::exit(1);
                }
            };

            println!("[clitrace] Listening for file changes... (Ctrl+C to stop)");

            // Register SIGINT handler
            unsafe {
                libc::signal(libc::SIGINT, sigint_handler as libc::sighandler_t);
            }

            // Wait until SIGINT
            while RUNNING.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }

            println!("\n[clitrace] Shutting down...");
            handle.stop();
            println!("[clitrace] Done.");
        }
        Commands::Report { claude_root, group_by } => {
            let config = build_config(claude_root, None);
            println!("[clitrace] Running report...");
            println!("[clitrace] Claude Code root: {}", config.claude_code_root);

            let parser = clitrace::providers::claude_code::ClaudeCodeParser;
            let group_by = match group_by.as_deref() {
                Some("hour") => Some(clitrace::engine::ReportGroupBy::Hour),
                Some("date") => Some(clitrace::engine::ReportGroupBy::Date),
                Some("month") => Some(clitrace::engine::ReportGroupBy::Month),
                Some("year") => Some(clitrace::engine::ReportGroupBy::Year),
                Some(v) => {
                    eprintln!("[clitrace] Invalid --group-by: {} (use hour|date|month|year)", v);
                    std::process::exit(1);
                }
                None => None,
            };

            if let Some(group_by) = group_by {
                if matches!(group_by, clitrace::engine::ReportGroupBy::Hour) {
                    let db = match clitrace::db::Database::open(&config.db_path) {
                        Ok(db) => db,
                        Err(e) => {
                            eprintln!("[clitrace] Failed to open DB: {}", e);
                            std::process::exit(1);
                        }
                    };
                    let mut checkpoints = HashMap::new();
                    if let Ok(cps) = db.load_all_checkpoints() {
                        for cp in cps {
                            checkpoints.insert(cp.file_path.clone(), cp);
                        }
                    }
                    if checkpoints.is_empty() {
                        eprintln!("[clitrace] Hour report requires existing checkpoints. Run trace first.");
                        std::process::exit(1);
                    }
                    if let Err(e) = clitrace::engine::cold_start_report_grouped_incremental(
                        &parser,
                        &config.claude_code_root,
                        group_by,
                        &checkpoints,
                    ) {
                        eprintln!("[clitrace] Report failed: {}", e);
                        std::process::exit(1);
                    }
                } else if let Err(e) = clitrace::engine::cold_start_report_grouped(
                    &parser,
                    &config.claude_code_root,
                    group_by,
                ) {
                    eprintln!("[clitrace] Report failed: {}", e);
                    std::process::exit(1);
                }
            } else if let Err(e) = clitrace::engine::cold_start_report(&parser, &config.claude_code_root) {
                eprintln!("[clitrace] Report failed: {}", e);
                std::process::exit(1);
            }
        }
    }
}

extern "C" fn sigint_handler(_: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}
