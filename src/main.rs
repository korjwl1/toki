use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use clap::{Args, Parser, Subcommand};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Weekday};
use chrono_tz::Tz;
use clitrace::Config;
use fs2::FileExt;

// Global flag for signal handler
static RUNNING: std::sync::atomic::AtomicBool = AtomicBool::new(true);

#[derive(Parser)]
#[command(name = "clitrace", version, about = "AI CLI tool token usage tracker")]
struct Cli {
    /// Output format: table (default) or json
    #[arg(long, default_value = "table", global = true)]
    output_format: String,
    /// Timezone for bucketing and --since/--until interpretation (e.g. Asia/Seoul, US/Eastern)
    #[arg(long, short = 'z', global = true)]
    timezone: Option<String>,
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
        /// Group startup summary by time period: hour, day, week, month, year
        #[arg(long = "startup-group-by")]
        startup_group_by: Option<String>,
        /// Filter by session ID prefix
        #[arg(long = "session-id")]
        session_id: Option<String>,
        /// Filter by project name (substring match on project directory)
        #[arg(long)]
        project: Option<String>,
    },
    /// Report mode: one-shot summary without writing checkpoints.
    Report {
        /// Claude Code root directory (default: ~/.claude)
        #[arg(long)]
        claude_root: Option<String>,
        /// Filter start time (inclusive, UTC): YYYYMMDD or YYYYMMDDhhmmss
        #[arg(long)]
        since: Option<String>,
        /// Filter end time (inclusive, UTC): YYYYMMDD or YYYYMMDDhhmmss
        #[arg(long)]
        until: Option<String>,
        /// Group results by session instead of time period
        #[arg(long = "group-by-session")]
        group_by_session: bool,
        /// Filter by session ID prefix
        #[arg(long = "session-id")]
        session_id: Option<String>,
        /// Filter by project name (substring match on project directory)
        #[arg(long)]
        project: Option<String>,
        #[command(subcommand)]
        command: Option<ReportCommands>,
    },
}

#[derive(Args, Clone)]
struct ReportFilterArgs {
    /// Filter start time (inclusive, UTC): YYYYMMDD or YYYYMMDDhhmmss
    #[arg(long)]
    since: Option<String>,
    /// Filter end time (inclusive, UTC): YYYYMMDD or YYYYMMDDhhmmss
    #[arg(long)]
    until: Option<String>,
    /// Allow full scan without --since (for hourly/daily/weekly)
    #[arg(long)]
    from_beginning: bool,
    /// Filter by session ID prefix
    #[arg(long = "session-id")]
    session_id: Option<String>,
    /// Filter by project name (substring match on project directory)
    #[arg(long)]
    project: Option<String>,
}

#[derive(Subcommand)]
enum ReportCommands {
    /// Group summary by day.
    Daily {
        #[command(flatten)]
        filter: ReportFilterArgs,
    },
    /// Group summary by week.
    Weekly {
        /// Start of week: mon, tue, wed, thu, fri, sat, sun
        #[arg(long = "start-of-week", short = 'w')]
        start_of_week: Option<String>,
        #[command(flatten)]
        filter: ReportFilterArgs,
    },
    /// Group summary by month.
    Monthly {
        #[command(flatten)]
        filter: ReportFilterArgs,
    },
    /// Group summary by year.
    Yearly {
        #[command(flatten)]
        filter: ReportFilterArgs,
    },
    /// Group summary by hour.
    Hourly {
        #[command(flatten)]
        filter: ReportFilterArgs,
    },
}

/// Parse a date/time string and return as UTC NaiveDateTime.
/// When tz is specified, the input is interpreted as local time in that timezone and converted to UTC.
/// When tz is None, the input is treated as UTC.
fn parse_range_arg(value: &str, is_until: bool, tz: Option<Tz>) -> Result<NaiveDateTime, String> {
    let naive = if value.len() == 8 && value.chars().all(|c| c.is_ascii_digit()) {
        let year: i32 = value[0..4].parse().map_err(|_| "invalid year")?;
        let month: u32 = value[4..6].parse().map_err(|_| "invalid month")?;
        let day: u32 = value[6..8].parse().map_err(|_| "invalid day")?;
        let date = NaiveDate::from_ymd_opt(year, month, day).ok_or("invalid date")?;
        let time = if is_until {
            NaiveTime::from_hms_opt(23, 59, 59).unwrap()
        } else {
            NaiveTime::from_hms_opt(0, 0, 0).unwrap()
        };
        NaiveDateTime::new(date, time)
    } else if value.len() == 14 && value.chars().all(|c| c.is_ascii_digit()) {
        let year: i32 = value[0..4].parse().map_err(|_| "invalid year")?;
        let month: u32 = value[4..6].parse().map_err(|_| "invalid month")?;
        let day: u32 = value[6..8].parse().map_err(|_| "invalid day")?;
        let hour: u32 = value[8..10].parse().map_err(|_| "invalid hour")?;
        let min: u32 = value[10..12].parse().map_err(|_| "invalid minute")?;
        let sec: u32 = value[12..14].parse().map_err(|_| "invalid second")?;
        let date = NaiveDate::from_ymd_opt(year, month, day).ok_or("invalid date")?;
        let time = NaiveTime::from_hms_opt(hour, min, sec).ok_or("invalid time")?;
        NaiveDateTime::new(date, time)
    } else {
        return Err("invalid format (use YYYYMMDD or YYYYMMDDhhmmss)".to_string());
    };

    // Convert local time to UTC when timezone is specified
    match tz {
        Some(tz) => {
            let local = tz.from_local_datetime(&naive)
                .single()
                .ok_or("ambiguous or invalid local time for timezone")?;
            Ok(local.naive_utc())
        }
        None => Ok(naive),
    }
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

    let output_format = match cli.output_format.as_str() {
        "table" => clitrace::engine::OutputFormat::Table,
        "json" => clitrace::engine::OutputFormat::Json,
        v => {
            eprintln!("[clitrace] Invalid --output-format: {} (use table|json)", v);
            std::process::exit(1);
        }
    };

    let tz: Option<Tz> = match cli.timezone.as_deref() {
        Some(name) => match name.parse::<Tz>() {
            Ok(tz) => Some(tz),
            Err(_) => {
                eprintln!("[clitrace] Invalid --timezone: {} (use IANA name like Asia/Seoul, US/Eastern)", name);
                std::process::exit(1);
            }
        },
        None => None,
    };

    match cli.command {
        Commands::Trace { claude_root, db_path, full_rescan, startup_group_by, session_id, project } => {
            let mut config = build_config(claude_root, db_path);
            if full_rescan {
                config = config.with_full_rescan(true);
            }
            if session_id.is_some() {
                config = config.with_session_filter(session_id);
            }
            if project.is_some() {
                config = config.with_project_filter(project);
            }
            config = config.with_tz(tz);

            // Parse --startup-group-by
            let group_by = match startup_group_by.as_deref() {
                Some("hour") => Some(clitrace::engine::ReportGroupBy::Hour),
                Some("day") => Some(clitrace::engine::ReportGroupBy::Date),
                Some("week") => Some(clitrace::engine::ReportGroupBy::Week { start_of_week: Weekday::Mon }),
                Some("month") => Some(clitrace::engine::ReportGroupBy::Month),
                Some("year") => Some(clitrace::engine::ReportGroupBy::Year),
                Some(v) => {
                    eprintln!("[clitrace] Invalid --startup-group-by: {} (use hour|day|week|month|year)", v);
                    std::process::exit(1);
                }
                None => None,
            };

            // Guard: hour requires existing checkpoints and cannot be used with --full-rescan
            if matches!(group_by, Some(clitrace::engine::ReportGroupBy::Hour)) {
                if full_rescan {
                    eprintln!("[clitrace] --startup-group-by hour cannot be used with --full-rescan");
                    std::process::exit(1);
                }
                if !config.db_path.exists() {
                    eprintln!("[clitrace] --startup-group-by hour requires existing checkpoints (run trace once first)");
                    std::process::exit(1);
                }
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

            let handle = match clitrace::start(config, group_by, output_format) {
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
        Commands::Report { claude_root, since, until, group_by_session, session_id, project, command } => {
            let config = build_config(claude_root, None);
            println!("[clitrace] Running report...");
            println!("[clitrace] Claude Code root: {}", config.claude_code_root);

            let parser = clitrace::providers::claude_code::ClaudeCodeParser;
            let session_filter = session_id.as_deref();
            let project_filter = project.as_deref();

            // Validate: --group-by-session cannot be used with subcommands
            if group_by_session && command.is_some() {
                eprintln!("[clitrace] --group-by-session cannot be used with time-based subcommands (daily/weekly/etc.)");
                std::process::exit(1);
            }

            let command = match command {
                Some(cmd) => cmd,
                None => {
                    // No subcommand: total report (with optional since/until filter)
                    let since_dt = match since.as_deref() {
                        Some(v) => match parse_range_arg(v, false, tz) {
                            Ok(dt) => Some(dt),
                            Err(e) => {
                                eprintln!("[clitrace] Invalid --since: {} ({})", v, e);
                                std::process::exit(1);
                            }
                        },
                        None => None,
                    };
                    let until_dt = match until.as_deref() {
                        Some(v) => match parse_range_arg(v, true, tz) {
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

                    if group_by_session {
                        let filter = clitrace::engine::ReportFilter { since: since_dt, until: until_dt, tz };
                        let checkpoints = std::collections::HashMap::new();
                        if let Err(e) = clitrace::engine::cold_start_report_by_session(
                            &parser,
                            &config.claude_code_root,
                            &checkpoints,
                            filter,
                            session_filter,
                            project_filter,
                            output_format,
                        ) {
                            eprintln!("[clitrace] Report failed: {}", e);
                            std::process::exit(1);
                        }
                    } else if since_dt.is_some() || until_dt.is_some() {
                        let filter = clitrace::engine::ReportFilter { since: since_dt, until: until_dt, tz };
                        let checkpoints = std::collections::HashMap::new();
                        match clitrace::engine::cold_start_report_filtered(
                            &parser,
                            &config.claude_code_root,
                            &checkpoints,
                            filter,
                            session_filter,
                            project_filter,
                        ) {
                            Ok(summaries) => clitrace::engine::print_summary(&summaries, output_format),
                            Err(e) => {
                                eprintln!("[clitrace] Report failed: {}", e);
                                std::process::exit(1);
                            }
                        }
                    } else if let Err(e) = clitrace::engine::cold_start_report(&parser, &config.claude_code_root, output_format, session_filter, project_filter) {
                        eprintln!("[clitrace] Report failed: {}", e);
                        std::process::exit(1);
                    }
                    return;
                }
            };

            // Extract filter args and group_by from subcommand
            let (filter_args, group_by) = match &command {
                ReportCommands::Hourly { filter } => (filter.clone(), clitrace::engine::ReportGroupBy::Hour),
                ReportCommands::Daily { filter } => (filter.clone(), clitrace::engine::ReportGroupBy::Date),
                ReportCommands::Weekly { start_of_week, filter } => {
                    let start = match start_of_week.as_deref().unwrap_or("mon") {
                        "mon" => Weekday::Mon,
                        "tue" => Weekday::Tue,
                        "wed" => Weekday::Wed,
                        "thu" => Weekday::Thu,
                        "fri" => Weekday::Fri,
                        "sat" => Weekday::Sat,
                        "sun" => Weekday::Sun,
                        _ => {
                            eprintln!("[clitrace] Invalid start-of-week (use mon|tue|wed|thu|fri|sat|sun)");
                            std::process::exit(1);
                        }
                    };
                    (filter.clone(), clitrace::engine::ReportGroupBy::Week { start_of_week: start })
                }
                ReportCommands::Monthly { filter } => (filter.clone(), clitrace::engine::ReportGroupBy::Month),
                ReportCommands::Yearly { filter } => (filter.clone(), clitrace::engine::ReportGroupBy::Year),
            };

            // Parse filter values
            let since_dt = match filter_args.since.as_deref() {
                Some(v) => match parse_range_arg(v, false, tz) {
                    Ok(dt) => Some(dt),
                    Err(e) => {
                        eprintln!("[clitrace] Invalid --since: {} ({})", v, e);
                        std::process::exit(1);
                    }
                },
                None => None,
            };
            let until_dt = match filter_args.until.as_deref() {
                Some(v) => match parse_range_arg(v, true, tz) {
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

            // Guard: hourly/daily/weekly require --since or --from-beginning
            let requires_range = matches!(
                group_by,
                clitrace::engine::ReportGroupBy::Hour
                    | clitrace::engine::ReportGroupBy::Date
                    | clitrace::engine::ReportGroupBy::Week { .. }
            );
            if requires_range && !filter_args.from_beginning && since_dt.is_none() {
                eprintln!("[clitrace] hourly/daily/weekly requires --since or --from-beginning");
                std::process::exit(1);
            }

            // Merge session filters: subcommand --session-id takes precedence, then parent-level
            let effective_session_filter = filter_args.session_id.as_deref().or(session_filter);
            let effective_project_filter = filter_args.project.as_deref().or(project_filter);

            let filter = clitrace::engine::ReportFilter { since: since_dt, until: until_dt, tz };
            let checkpoints = std::collections::HashMap::new();
            if let Err(e) = clitrace::engine::cold_start_report_grouped(
                &parser,
                &config.claude_code_root,
                group_by,
                &checkpoints,
                filter,
                output_format,
                effective_session_filter,
                effective_project_filter,
            ) {
                eprintln!("[clitrace] Report failed: {}", e);
                std::process::exit(1);
            }
        }
    }
}

extern "C" fn sigint_handler(_: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}
