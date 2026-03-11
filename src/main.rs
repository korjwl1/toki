use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use clap::{Parser, Subcommand};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
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
        /// Filter start time (inclusive): YYYYMMDD or YYYYMMDDhhmmss
        #[arg(long)]
        since: Option<String>,
        /// Filter end time (inclusive): YYYYMMDD or YYYYMMDDhhmmss
        #[arg(long)]
        until: Option<String>,
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
    /// Group summary by hour (requires --since or --from-beginning).
    Hourly {
        /// Allow full scan without --since
        #[arg(long = "from-beginning")]
        from_beginning: bool,
    },
    /// Group summary by year.
    Yearly,
}

fn parse_range_arg(value: &str, is_until: bool) -> Result<NaiveDateTime, String> {
    if value.len() == 8 && value.chars().all(|c| c.is_ascii_digit()) {
        let year: i32 = value[0..4].parse().map_err(|_| "invalid year")?;
        let month: u32 = value[4..6].parse().map_err(|_| "invalid month")?;
        let day: u32 = value[6..8].parse().map_err(|_| "invalid day")?;
        let date = NaiveDate::from_ymd_opt(year, month, day).ok_or("invalid date")?;
        let time = if is_until {
            NaiveTime::from_hms_opt(23, 59, 59).unwrap()
        } else {
            NaiveTime::from_hms_opt(0, 0, 0).unwrap()
        };
        return Ok(NaiveDateTime::new(date, time));
    }
    if value.len() == 14 && value.chars().all(|c| c.is_ascii_digit()) {
        let year: i32 = value[0..4].parse().map_err(|_| "invalid year")?;
        let month: u32 = value[4..6].parse().map_err(|_| "invalid month")?;
        let day: u32 = value[6..8].parse().map_err(|_| "invalid day")?;
        let hour: u32 = value[8..10].parse().map_err(|_| "invalid hour")?;
        let min: u32 = value[10..12].parse().map_err(|_| "invalid minute")?;
        let sec: u32 = value[12..14].parse().map_err(|_| "invalid second")?;
        let date = NaiveDate::from_ymd_opt(year, month, day).ok_or("invalid date")?;
        let time = NaiveTime::from_hms_opt(hour, min, sec).ok_or("invalid time")?;
        return Ok(NaiveDateTime::new(date, time));
    }
    Err("invalid format (use YYYYMMDD or YYYYMMDDhhmmss)".to_string())
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
        Commands::Report { claude_root, since, until, command } => {
            let config = build_config(claude_root, None);
            println!("[clitrace] Running report...");
            println!("[clitrace] Claude Code root: {}", config.claude_code_root);

            let parser = clitrace::providers::claude_code::ClaudeCodeParser;
            let since_dt = match since.as_deref() {
                Some(v) => match parse_range_arg(v, false) {
                    Ok(dt) => Some(dt),
                    Err(e) => {
                        eprintln!("[clitrace] Invalid --since: {} ({})", v, e);
                        std::process::exit(1);
                    }
                },
                None => None,
            };
            let until_dt = match until.as_deref() {
                Some(v) => match parse_range_arg(v, true) {
                    Ok(dt) => Some(dt),
                    Err(e) => {
                        eprintln!("[clitrace] Invalid --until: {} ({})", v, e);
                        std::process::exit(1);
                    }
                },
                None => None,
            };
            if let (Some(s), Some(u)) = (since_dt, until_dt) {
                if u < s {
                    eprintln!("[clitrace] Invalid range: --until is earlier than --since");
                    std::process::exit(1);
                }
            }
            let filter = clitrace::engine::ReportFilter { since: since_dt, until: until_dt };
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
                Some(ReportCommands::Hourly { from_beginning }) => {
                    if since_dt.is_none() && !from_beginning {
                        eprintln!("[clitrace] Hourly report requires --since or --from-beginning");
                        std::process::exit(1);
                    }
                    Some(clitrace::engine::ReportGroupBy::Hour)
                }
                Some(ReportCommands::Yearly) => Some(clitrace::engine::ReportGroupBy::Year),
                None => None,
            };

            if let Some(group_by) = group_by {
                if let Err(e) = clitrace::engine::cold_start_report_grouped(
                    &parser,
                    &config.claude_code_root,
                    group_by,
                    filter,
                ) {
                    eprintln!("[clitrace] Report failed: {}", e);
                    std::process::exit(1);
                }
            } else if filter.since.is_some() || filter.until.is_some() {
                match clitrace::engine::cold_start_report_filtered(&parser, &config.claude_code_root, filter) {
                    Ok(summaries) => clitrace::engine::print_summary(&summaries),
                    Err(e) => {
                        eprintln!("[clitrace] Report failed: {}", e);
                        std::process::exit(1);
                    }
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
