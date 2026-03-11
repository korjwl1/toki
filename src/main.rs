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
    },
    /// Report mode: one-shot summary without writing checkpoints.
    Report {
        /// Claude Code root directory (default: ~/.claude)
        #[arg(long)]
        claude_root: Option<String>,
        #[command(subcommand)]
        command: Option<ReportCommands>,
    },
}

#[derive(Subcommand)]
enum ReportCommands {
    /// Group summary by day.
    Daily,
    /// Group summary by week.
    Weekly {
        /// Start of week: mon, tue, wed, thu, fri, sat
        #[arg(long = "start-of-week", short = 'w')]
        start_of_week: Option<String>,
    },
    /// Group summary by year.
    Yearly,
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
        Commands::Trace { claude_root, db_path } => {
            let config = build_config(claude_root, db_path);

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
        Commands::Report { claude_root, command } => {
            let config = build_config(claude_root, None);
            println!("[clitrace] Running report...");
            println!("[clitrace] Claude Code root: {}", config.claude_code_root);

            let parser = clitrace::providers::claude_code::ClaudeCodeParser;
            let group_by = match command {
                Some(ReportCommands::Daily) => Some(clitrace::engine::ReportGroupBy::Day),
                Some(ReportCommands::Weekly { start_of_week }) => {
                    let start = match start_of_week.as_deref().unwrap_or("mon") {
                        "mon" => chrono::Weekday::Mon,
                        "tue" => chrono::Weekday::Tue,
                        "wed" => chrono::Weekday::Wed,
                        "thu" => chrono::Weekday::Thu,
                        "fri" => chrono::Weekday::Fri,
                        "sat" => chrono::Weekday::Sat,
                        _ => {
                            eprintln!("[clitrace] Invalid start-of-week (use mon|tue|wed|thu|fri|sat)");
                            std::process::exit(1);
                        }
                    };
                    Some(clitrace::engine::ReportGroupBy::Week { start_of_week: start })
                }
                Some(ReportCommands::Yearly) => Some(clitrace::engine::ReportGroupBy::Year),
                None => None,
            };

            if let Some(group_by) = group_by {
                if let Err(e) = clitrace::engine::cold_start_report_grouped(&parser, &config.claude_code_root, group_by) {
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
