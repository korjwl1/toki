use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use clap::{Args, Parser, Subcommand};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Weekday};
use chrono_tz::Tz;
use clitrace::Config;
use fs2::FileExt;

static RUNNING: AtomicBool = AtomicBool::new(true);

#[derive(Parser)]
#[command(name = "clitrace", version, about = "AI CLI tool token usage tracker")]
struct Cli {
    /// Output format for print sink: table (default) or json
    #[arg(long, default_value = "table", global = true)]
    output_format: String,
    /// Output sink(s): print (default), uds://<path>, http://<url>
    #[arg(long, global = true)]
    sink: Vec<String>,
    /// Timezone for bucketing and --since/--until interpretation (e.g. Asia/Seoul, US/Eastern)
    #[arg(long, short = 'z', global = true)]
    timezone: Option<String>,
    /// Disable cost calculation (skip pricing fetch)
    #[arg(long, global = true)]
    no_cost: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Daemon management: start/stop/status
    Daemon {
        #[command(subcommand)]
        command: DaemonCommands,
    },
    /// Connect to running daemon and stream real-time events
    Trace {
        /// Daemon socket path (default: ~/.config/clitrace/daemon.sock)
        #[arg(long)]
        sock: Option<String>,
    },
    /// Report mode: one-shot summary from TSDB or JSONL
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

#[derive(Subcommand)]
enum DaemonCommands {
    /// Start the daemon (foreground). Watches files and writes to TSDB.
    Start {
        /// Claude Code root directory (default: ~/.claude)
        #[arg(long)]
        claude_root: Option<String>,
        /// DB path (default: ~/.config/clitrace/clitrace.fjall)
        #[arg(long)]
        db_path: Option<PathBuf>,
        /// Clear checkpoints and perform full rescan on startup
        #[arg(long)]
        full_rescan: bool,
        /// Daemon socket path (default: ~/.config/clitrace/daemon.sock)
        #[arg(long)]
        sock: Option<String>,
        /// Group startup summary by time period: hour, day, week, month, year
        #[arg(long = "startup-group-by")]
        startup_group_by: Option<String>,
        /// Filter by session ID prefix
        #[arg(long = "session-id")]
        session_id: Option<String>,
        /// Filter by project name (substring match)
        #[arg(long)]
        project: Option<String>,
    },
    /// Stop a running daemon
    Stop {
        /// Daemon socket path
        #[arg(long)]
        sock: Option<String>,
    },
    /// Check daemon status
    Status {
        /// Daemon socket path
        #[arg(long)]
        sock: Option<String>,
    },
}

#[derive(Args, Clone)]
struct ReportFilterArgs {
    /// Filter start time (inclusive): YYYYMMDD or YYYYMMDDhhmmss
    #[arg(long)]
    since: Option<String>,
    /// Filter end time (inclusive): YYYYMMDD or YYYYMMDDhhmmss
    #[arg(long)]
    until: Option<String>,
    /// Allow full scan without --since (for hourly/daily/weekly)
    #[arg(long)]
    from_beginning: bool,
    /// Filter by session ID prefix
    #[arg(long = "session-id")]
    session_id: Option<String>,
    /// Filter by project name (substring match)
    #[arg(long)]
    project: Option<String>,
}

#[derive(Subcommand)]
enum ReportCommands {
    Daily {
        #[command(flatten)]
        filter: ReportFilterArgs,
    },
    Weekly {
        #[arg(long = "start-of-week", short = 'w')]
        start_of_week: Option<String>,
        #[command(flatten)]
        filter: ReportFilterArgs,
    },
    Monthly {
        #[command(flatten)]
        filter: ReportFilterArgs,
    },
    Yearly {
        #[command(flatten)]
        filter: ReportFilterArgs,
    },
    Hourly {
        #[command(flatten)]
        filter: ReportFilterArgs,
    },
}

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

fn resolve_sock_path(sock_arg: Option<String>) -> PathBuf {
    sock_arg
        .map(PathBuf::from)
        .or_else(|| std::env::var("CLITRACE_DAEMON_SOCK").ok().map(PathBuf::from))
        .unwrap_or_else(clitrace::daemon::default_sock_path)
}

fn main() {
    let cli = Cli::parse();

    let output_format = match cli.output_format.as_str() {
        "table" => clitrace::sink::OutputFormat::Table,
        "json" => clitrace::sink::OutputFormat::Json,
        v => {
            eprintln!("[clitrace] Invalid --output-format: {} (use table|json)", v);
            std::process::exit(1);
        }
    };

    let sink_specs = if cli.sink.is_empty() {
        vec!["print".to_string()]
    } else {
        cli.sink.clone()
    };

    let tz: Option<Tz> = match cli.timezone.as_deref() {
        Some(name) => match name.parse::<Tz>() {
            Ok(tz) => Some(tz),
            Err(_) => {
                eprintln!("[clitrace] Invalid --timezone: {} (use IANA name like Asia/Seoul)", name);
                std::process::exit(1);
            }
        },
        None => None,
    };

    match cli.command {
        Commands::Daemon { command } => handle_daemon(command, tz, cli.no_cost),
        Commands::Trace { sock } => handle_trace(sock, &sink_specs, output_format),
        Commands::Report { claude_root, since, until, group_by_session, session_id, project, command } => {
            handle_report(claude_root, since, until, group_by_session, session_id, project, command,
                          &sink_specs, output_format, tz, cli.no_cost);
        }
    }
}

// ── Daemon ──────────────────────────────────────────────

fn handle_daemon(command: DaemonCommands, tz: Option<Tz>, no_cost: bool) {
    match command {
        DaemonCommands::Start { claude_root, db_path, full_rescan, sock, startup_group_by, session_id, project } => {
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

            let sock_path = resolve_sock_path(sock);

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

            if matches!(group_by, Some(clitrace::engine::ReportGroupBy::Hour)) {
                if full_rescan {
                    eprintln!("[clitrace] --startup-group-by hour cannot be used with --full-rescan");
                    std::process::exit(1);
                }
                if !config.db_path.exists() {
                    eprintln!("[clitrace] --startup-group-by hour requires existing checkpoints");
                    std::process::exit(1);
                }
            }

            // Check for existing daemon
            let pidfile = clitrace::daemon::default_pidfile_path();
            if let Some(pid) = clitrace::daemon::daemon_status(&pidfile) {
                eprintln!("[clitrace] Daemon already running (PID {})", pid);
                std::process::exit(1);
            }

            let _lock = match acquire_trace_lock(&config.db_path) {
                Ok(f) => f,
                Err(_) => {
                    eprintln!("[clitrace] Another instance is already running.");
                    std::process::exit(1);
                }
            };

            // Create BroadcastSink (no clients initially = zero overhead)
            let broadcast = Arc::new(clitrace::daemon::BroadcastSink::new());

            eprintln!("[clitrace:daemon] Starting...");
            eprintln!("[clitrace:daemon] Claude Code root: {}", config.claude_code_root);
            eprintln!("[clitrace:daemon] Database: {}", config.db_path.display());
            eprintln!("[clitrace:daemon] Socket: {}", sock_path.display());

            // Start engine with BroadcastSink
            let handle = match clitrace::start(config, group_by, Box::new(broadcast.clone()), no_cost) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("[clitrace:daemon] Failed to start: {}", e);
                    std::process::exit(1);
                }
            };

            // Write PID file
            clitrace::daemon::write_pidfile(&pidfile);

            // Start UDS listener in a thread
            let (listener_stop_tx, listener_stop_rx) = crossbeam_channel::bounded::<()>(1);
            let listener_sock = sock_path.clone();
            let listener_broadcast = broadcast.clone();
            let listener_handle = std::thread::Builder::new()
                .name("clitrace-listener".to_string())
                .spawn(move || {
                    clitrace::daemon::run_listener(&listener_sock, listener_broadcast, listener_stop_rx);
                })
                .expect("Failed to spawn listener thread");

            // Register signal handlers
            unsafe {
                libc::signal(libc::SIGINT, sighandler as *const () as libc::sighandler_t);
                libc::signal(libc::SIGTERM, sighandler as *const () as libc::sighandler_t);
            }

            eprintln!("[clitrace:daemon] Running (PID {}). Send SIGTERM or use 'clitrace daemon stop' to stop.",
                std::process::id());

            // Wait until signal
            while RUNNING.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }

            eprintln!("\n[clitrace:daemon] Shutting down...");

            // Stop listener
            let _ = listener_stop_tx.send(());
            let _ = listener_handle.join();

            // Stop engine + writer
            handle.stop();

            // Cleanup
            clitrace::daemon::remove_pidfile(&pidfile);
            let _ = std::fs::remove_file(&sock_path);

            eprintln!("[clitrace:daemon] Done.");
        }

        DaemonCommands::Stop { sock } => {
            let sock_path = resolve_sock_path(sock);
            let pidfile = clitrace::daemon::default_pidfile_path();

            match clitrace::daemon::stop_daemon(&pidfile, &sock_path) {
                Ok(true) => println!("[clitrace] Daemon stopped."),
                Ok(false) => println!("[clitrace] Daemon is not running."),
                Err(e) => {
                    eprintln!("[clitrace] Error stopping daemon: {}", e);
                    std::process::exit(1);
                }
            }
        }

        DaemonCommands::Status { sock } => {
            let sock_path = resolve_sock_path(sock);
            let pidfile = clitrace::daemon::default_pidfile_path();

            match clitrace::daemon::daemon_status(&pidfile) {
                Some(pid) => {
                    println!("[clitrace] Daemon is running (PID {})", pid);
                    println!("[clitrace] Socket: {}", sock_path.display());
                }
                None => {
                    println!("[clitrace] Daemon is not running.");
                }
            }
        }
    }
}

// ── Trace (client) ──────────────────────────────────────

fn handle_trace(sock: Option<String>, sink_specs: &[String], output_format: clitrace::sink::OutputFormat) {
    let sock_path = resolve_sock_path(sock);

    let stream = match UnixStream::connect(&sock_path) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("[clitrace] Cannot connect to daemon at {}", sock_path.display());
            eprintln!("[clitrace] Start the daemon first: clitrace daemon start");
            std::process::exit(1);
        }
    };

    let sink = clitrace::sink::create_sinks(sink_specs, output_format);
    let reader = BufReader::new(stream);

    // Register SIGINT to exit cleanly
    unsafe {
        libc::signal(libc::SIGINT, sighandler as *const () as libc::sighandler_t);
    }

    println!("[clitrace] Connected to daemon. Streaming events... (Ctrl+C to stop)");

    for line_result in reader.lines() {
        if !RUNNING.load(Ordering::SeqCst) {
            break;
        }

        let line = match line_result {
            Ok(l) => l,
            Err(_) => {
                eprintln!("[clitrace] Daemon disconnected.");
                break;
            }
        };

        if line.is_empty() {
            continue;
        }

        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        match v["type"].as_str() {
            Some("event") => {
                if let Ok(event) = serde_json::from_value::<clitrace::UsageEvent>(v["data"].clone()) {
                    // Cost is already included in the JSON data, extract if present
                    sink.emit_event(&event, None);
                }
            }
            Some("summary") => {
                // Summary data — print raw JSON for now
                println!("{}", line);
            }
            _ => {
                // Unknown type — pass through
                println!("{}", line);
            }
        }
    }

    println!("[clitrace] Disconnected.");
}

// ── Report ──────────────────────────────────────────────

fn handle_report(
    claude_root: Option<String>,
    since: Option<String>,
    until: Option<String>,
    group_by_session: bool,
    session_id: Option<String>,
    project: Option<String>,
    command: Option<ReportCommands>,
    sink_specs: &[String],
    output_format: clitrace::sink::OutputFormat,
    tz: Option<Tz>,
    no_cost: bool,
) {
    let config = build_config(claude_root, None);
    println!("[clitrace] Running report...");
    println!("[clitrace] Claude Code root: {}", config.claude_code_root);

    let sink = clitrace::sink::create_sinks(sink_specs, output_format);

    let db = clitrace::db::Database::open(&config.db_path).ok();

    let pricing = if no_cost {
        None
    } else {
        match db {
            Some(ref d) => {
                let p = clitrace::pricing::fetch_pricing(d);
                if p.is_empty() { None } else { Some(p) }
            }
            None => None,
        }
    };
    let pricing_ref = pricing.as_ref();

    let use_tsdb = db.as_ref().map_or(false, clitrace::query::has_tsdb_data);

    let parser = clitrace::providers::claude_code::ClaudeCodeParser;
    let session_filter = session_id.as_deref();
    let project_filter = project.as_deref();

    if group_by_session && command.is_some() {
        eprintln!("[clitrace] --group-by-session cannot be used with time-based subcommands");
        std::process::exit(1);
    }

    let command = match command {
        Some(cmd) => cmd,
        None => {
            let since_dt = parse_opt_range(&since, false, tz);
            let until_dt = parse_opt_range(&until, true, tz);
            validate_range(since_dt, until_dt);

            let filter = clitrace::engine::ReportFilter { since: since_dt, until: until_dt, tz };

            if group_by_session {
                if use_tsdb {
                    match clitrace::query::report_by_session_from_db(db.as_ref().unwrap(), filter) {
                        Ok(grouped) => sink.emit_grouped(&grouped, "session", pricing_ref),
                        Err(e) => {
                            eprintln!("[clitrace] TSDB query failed: {}, falling back to JSONL scan", e);
                            let cps = std::collections::HashMap::new();
                            if let Err(e) = clitrace::engine::cold_start_report_by_session(
                                &parser, &config.claude_code_root, &cps, filter,
                                session_filter, project_filter, sink.as_ref(), pricing_ref,
                            ) {
                                eprintln!("[clitrace] Report failed: {}", e);
                                std::process::exit(1);
                            }
                        }
                    }
                } else {
                    let cps = std::collections::HashMap::new();
                    if let Err(e) = clitrace::engine::cold_start_report_by_session(
                        &parser, &config.claude_code_root, &cps, filter,
                        session_filter, project_filter, sink.as_ref(), pricing_ref,
                    ) {
                        eprintln!("[clitrace] Report failed: {}", e);
                        std::process::exit(1);
                    }
                }
            } else if use_tsdb {
                match clitrace::query::report_summary_from_db(db.as_ref().unwrap(), filter) {
                    Ok(summaries) => sink.emit_summary(&summaries, pricing_ref),
                    Err(e) => {
                        eprintln!("[clitrace] TSDB query failed: {}, falling back to JSONL scan", e);
                        fallback_total_report(&parser, &config, filter, session_filter, project_filter, sink.as_ref(), pricing_ref);
                    }
                }
            } else {
                fallback_total_report(&parser, &config, filter, session_filter, project_filter, sink.as_ref(), pricing_ref);
            }
            return;
        }
    };

    let (filter_args, group_by) = match &command {
        ReportCommands::Hourly { filter } => (filter.clone(), clitrace::engine::ReportGroupBy::Hour),
        ReportCommands::Daily { filter } => (filter.clone(), clitrace::engine::ReportGroupBy::Date),
        ReportCommands::Weekly { start_of_week, filter } => {
            let start = parse_weekday(start_of_week.as_deref().unwrap_or("mon"));
            (filter.clone(), clitrace::engine::ReportGroupBy::Week { start_of_week: start })
        }
        ReportCommands::Monthly { filter } => (filter.clone(), clitrace::engine::ReportGroupBy::Month),
        ReportCommands::Yearly { filter } => (filter.clone(), clitrace::engine::ReportGroupBy::Year),
    };

    let since_dt = parse_opt_range(&filter_args.since, false, tz);
    let until_dt = parse_opt_range(&filter_args.until, true, tz);
    validate_range(since_dt, until_dt);

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

    let effective_session_filter = filter_args.session_id.as_deref().or(session_filter);
    let effective_project_filter = filter_args.project.as_deref().or(project_filter);

    let filter = clitrace::engine::ReportFilter { since: since_dt, until: until_dt, tz };

    if use_tsdb {
        match clitrace::query::report_grouped_from_db(db.as_ref().unwrap(), group_by, filter) {
            Ok(grouped) => {
                sink.emit_grouped(&grouped, group_by.type_name(), pricing_ref);
            }
            Err(e) => {
                eprintln!("[clitrace] TSDB query failed: {}, falling back to JSONL scan", e);
                let cps = std::collections::HashMap::new();
                if let Err(e) = clitrace::engine::cold_start_report_grouped(
                    &parser, &config.claude_code_root, group_by, &cps, filter,
                    sink.as_ref(), effective_session_filter, effective_project_filter, pricing_ref,
                ) {
                    eprintln!("[clitrace] Report failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
    } else {
        let cps = std::collections::HashMap::new();
        if let Err(e) = clitrace::engine::cold_start_report_grouped(
            &parser, &config.claude_code_root, group_by, &cps, filter,
            sink.as_ref(), effective_session_filter, effective_project_filter, pricing_ref,
        ) {
            eprintln!("[clitrace] Report failed: {}", e);
            std::process::exit(1);
        }
    }
}

// ── Helpers ─────────────────────────────────────────────

fn parse_opt_range(value: &Option<String>, is_until: bool, tz: Option<Tz>) -> Option<NaiveDateTime> {
    value.as_deref().map(|v| {
        parse_range_arg(v, is_until, tz).unwrap_or_else(|e| {
            let label = if is_until { "--until" } else { "--since" };
            eprintln!("[clitrace] Invalid {}: {} ({})", label, v, e);
            std::process::exit(1);
        })
    })
}

fn validate_range(since: Option<NaiveDateTime>, until: Option<NaiveDateTime>) {
    if let (Some(s), Some(u)) = (since, until) {
        if u < s {
            eprintln!("[clitrace] Invalid range: --until is earlier than --since");
            std::process::exit(1);
        }
    }
}

fn parse_weekday(s: &str) -> Weekday {
    match s {
        "mon" => Weekday::Mon, "tue" => Weekday::Tue, "wed" => Weekday::Wed,
        "thu" => Weekday::Thu, "fri" => Weekday::Fri, "sat" => Weekday::Sat,
        "sun" => Weekday::Sun,
        _ => {
            eprintln!("[clitrace] Invalid start-of-week (use mon|tue|wed|thu|fri|sat|sun)");
            std::process::exit(1);
        }
    }
}

fn fallback_total_report(
    parser: &clitrace::providers::claude_code::ClaudeCodeParser,
    config: &clitrace::Config,
    filter: clitrace::engine::ReportFilter,
    session_filter: Option<&str>,
    project_filter: Option<&str>,
    sink: &dyn clitrace::sink::Sink,
    pricing: Option<&clitrace::pricing::PricingTable>,
) {
    let cps = std::collections::HashMap::new();
    if filter.since.is_some() || filter.until.is_some() {
        match clitrace::engine::cold_start_report_filtered(
            parser, &config.claude_code_root, &cps, filter,
            session_filter, project_filter,
        ) {
            Ok(summaries) => sink.emit_summary(&summaries, pricing),
            Err(e) => {
                eprintln!("[clitrace] Report failed: {}", e);
                std::process::exit(1);
            }
        }
    } else if let Err(e) = clitrace::engine::cold_start_report(
        parser, &config.claude_code_root, sink, session_filter, project_filter, pricing,
    ) {
        eprintln!("[clitrace] Report failed: {}", e);
        std::process::exit(1);
    }
}

extern "C" fn sighandler(_: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}
