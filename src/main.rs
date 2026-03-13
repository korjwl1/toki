use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use clap::{Args, Parser, Subcommand};
use chrono::{NaiveDateTime, Weekday};
use chrono_tz::Tz;
use toki::Config;
use fs2::FileExt;

static RUNNING: AtomicBool = AtomicBool::new(true);

#[derive(Parser)]
#[command(name = "toki", version, about = "AI CLI tool token usage tracker")]
struct Cli {
    /// Output format override: table or json (overrides DB setting)
    #[arg(long, global = true)]
    output_format: Option<String>,
    /// Output sink(s): print (default), uds://<path>, http://<url>
    #[arg(long, global = true)]
    sink: Vec<String>,
    /// Timezone override (e.g. Asia/Seoul, US/Eastern)
    #[arg(long, short = 'z', global = true)]
    timezone: Option<String>,
    /// Disable cost calculation (overrides DB setting)
    #[arg(long, global = true)]
    no_cost: bool,
    /// Database path (default: ~/.config/toki/toki.fjall)
    #[arg(long, global = true)]
    db_path: Option<PathBuf>,
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
    Trace,
    /// Report mode: one-shot summary from TSDB
    Report {
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
    /// Open settings TUI
    Settings,
}

#[derive(Subcommand)]
enum DaemonCommands {
    /// Start the daemon (foreground). Watches files and writes to TSDB.
    Start,
    /// Stop a running daemon
    Stop,
    /// Restart the daemon (stop + start). Picks up settings changes.
    Restart,
    /// Check daemon status
    Status,
    /// Delete all data and reset the database
    Reset,
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
    /// Execute a PromQL-style query (e.g. 'usage{model="claude-opus-4-6"}[1h] by (model)')
    Query {
        /// Query string
        query: String,
    },
}

/// Build Config from defaults + DB settings + CLI overrides.
fn build_config(db_path: Option<PathBuf>, cli_tz: Option<Tz>, cli_no_cost: bool, cli_output_format: Option<&str>) -> Config {
    let mut config = Config::new();

    // CLI --db-path override
    if let Some(path) = db_path {
        config = config.with_db_path(path);
    }

    // Load settings from DB (if DB exists)
    if let Ok(db) = toki::db::Database::open(&config.db_path) {
        config.load_from_db(&db);
    }

    // CLI overrides take precedence over DB settings
    if let Some(tz) = cli_tz {
        config = config.with_tz(Some(tz));
    }
    if cli_no_cost {
        config.no_cost = true;
    }
    if let Some(fmt) = cli_output_format {
        config.output_format = fmt.to_string();
    }

    config
}

fn acquire_trace_lock(db_path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let lock_path = db_path.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    file.try_lock_exclusive()?;
    Ok(file)
}

fn resolve_output_format(config: &Config) -> toki::sink::OutputFormat {
    match config.output_format.as_str() {
        "json" => toki::sink::OutputFormat::Json,
        _ => toki::sink::OutputFormat::Table,
    }
}

fn main() {
    let cli = Cli::parse();

    // Parse CLI timezone
    let cli_tz: Option<Tz> = match cli.timezone.as_deref() {
        Some(name) => match name.parse::<Tz>() {
            Ok(tz) => Some(tz),
            Err(_) => {
                eprintln!("[toki] Invalid --timezone: {} (use IANA name like Asia/Seoul)", name);
                std::process::exit(1);
            }
        },
        None => None,
    };

    // Validate CLI --output-format if provided
    let cli_output_format = cli.output_format.as_deref();
    if let Some(fmt) = cli_output_format {
        if fmt != "table" && fmt != "json" {
            eprintln!("[toki] Invalid --output-format: {} (use table|json)", fmt);
            std::process::exit(1);
        }
    }

    match cli.command {
        Commands::Settings => {
            let config = Config::new();
            let db_path = cli.db_path.unwrap_or(config.db_path);
            toki::settings::run_settings(&db_path);
        }
        Commands::Daemon { command } => {
            let config = build_config(cli.db_path, cli_tz, cli.no_cost, cli_output_format);
            handle_daemon(command, &config);
        }
        Commands::Trace => {
            let config = build_config(cli.db_path, cli_tz, cli.no_cost, cli_output_format);
            let output_format = resolve_output_format(&config);
            let sink_specs = if cli.sink.is_empty() {
                vec!["print".to_string()]
            } else {
                cli.sink.clone()
            };
            handle_trace(&config, &sink_specs, output_format);
        }
        Commands::Report { since, until, group_by_session, session_id, project, command } => {
            let config = build_config(cli.db_path, cli_tz, cli.no_cost, cli_output_format);
            let output_format = resolve_output_format(&config);
            let sink_specs = if cli.sink.is_empty() {
                vec!["print".to_string()]
            } else {
                cli.sink.clone()
            };
            handle_report(since, until, group_by_session, session_id, project, command,
                          &config, &sink_specs, output_format);
        }
    }
}

// ── Daemon ──────────────────────────────────────────────

fn stop_running_daemon(config: &Config) -> bool {
    let pidfile = toki::daemon::default_pidfile_path();
    let sock_path = &config.daemon_sock;
    match toki::daemon::stop_daemon(&pidfile, sock_path) {
        Ok(true) => { println!("[toki] Daemon stopped."); true }
        Ok(false) => { println!("[toki] Daemon is not running."); false }
        Err(e) => {
            eprintln!("[toki] Error stopping daemon: {}", e);
            std::process::exit(1);
        }
    }
}

fn run_daemon_foreground(config: &Config) {
    let sock_path = config.daemon_sock.clone();
    let pidfile = toki::daemon::default_pidfile_path();

    if let Some(pid) = toki::daemon::daemon_status(&pidfile) {
        eprintln!("[toki] Daemon already running (PID {})", pid);
        std::process::exit(1);
    }

    let _lock = match acquire_trace_lock(&config.db_path) {
        Ok(f) => f,
        Err(_) => {
            eprintln!("[toki] Another instance is already running.");
            std::process::exit(1);
        }
    };

    let broadcast = Arc::new(toki::daemon::BroadcastSink::new());

    eprintln!("[toki:daemon] Starting...");
    eprintln!("[toki:daemon] Claude Code root: {}", config.claude_code_root);
    eprintln!("[toki:daemon] Database: {}", config.db_path.display());
    eprintln!("[toki:daemon] Socket: {}", sock_path.display());

    let handle = match toki::start(config.clone(), Box::new(broadcast.clone())) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[toki:daemon] Failed to start: {}", e);
            std::process::exit(1);
        }
    };

    toki::daemon::write_pidfile(&pidfile);

    let (listener_stop_tx, listener_stop_rx) = crossbeam_channel::bounded::<()>(1);
    let listener_sock = sock_path.clone();
    let listener_broadcast = broadcast.clone();
    let listener_handle = std::thread::Builder::new()
        .name("toki-listener".to_string())
        .spawn(move || {
            toki::daemon::run_listener(&listener_sock, listener_broadcast, listener_stop_rx);
        })
        .expect("Failed to spawn listener thread");

    unsafe {
        libc::signal(libc::SIGINT, sighandler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, sighandler as *const () as libc::sighandler_t);
    }

    eprintln!("[toki:daemon] Running (PID {}). Send SIGTERM or use 'toki daemon stop' to stop.",
        std::process::id());

    while RUNNING.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    eprintln!("\n[toki:daemon] Shutting down...");
    let _ = listener_stop_tx.send(());
    let _ = listener_handle.join();
    handle.stop();
    toki::daemon::remove_pidfile(&pidfile);
    let _ = std::fs::remove_file(&sock_path);
    eprintln!("[toki:daemon] Done.");
}

fn handle_daemon(command: DaemonCommands, config: &Config) {
    match command {
        DaemonCommands::Start => {
            run_daemon_foreground(config);
        }

        DaemonCommands::Stop => {
            stop_running_daemon(config);
        }

        DaemonCommands::Restart => {
            stop_running_daemon(config);
            RUNNING.store(true, Ordering::SeqCst);
            run_daemon_foreground(config);
        }

        DaemonCommands::Status => {
            let pidfile = toki::daemon::default_pidfile_path();
            match toki::daemon::daemon_status(&pidfile) {
                Some(pid) => {
                    println!("[toki] Daemon is running (PID {})", pid);
                    println!("[toki] Socket: {}", config.daemon_sock.display());
                }
                None => {
                    println!("[toki] Daemon is not running.");
                }
            }
        }

        DaemonCommands::Reset => {
            let pidfile = toki::daemon::default_pidfile_path();
            if toki::daemon::daemon_status(&pidfile).is_some() {
                println!("[toki] Stopping daemon first...");
                stop_running_daemon(config);
            }

            if config.db_path.exists() {
                if let Err(e) = std::fs::remove_dir_all(&config.db_path) {
                    eprintln!("[toki] Failed to delete database: {}", e);
                    std::process::exit(1);
                }
                println!("[toki] Database deleted: {}", config.db_path.display());
            } else {
                println!("[toki] No database found at {}", config.db_path.display());
            }
            println!("[toki] Reset complete. Start the daemon to rebuild: toki daemon start");
        }
    }
}

// ── Trace (client) ──────────────────────────────────────

fn handle_trace(config: &Config, sink_specs: &[String], output_format: toki::sink::OutputFormat) {
    let sock_path = config.daemon_sock.clone();

    let stream = match UnixStream::connect(&sock_path) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("[toki] Cannot connect to daemon at {}", sock_path.display());
            eprintln!("[toki] Start the daemon first: toki daemon start");
            std::process::exit(1);
        }
    };

    let sink = toki::sink::create_sinks(sink_specs, output_format);
    let reader = BufReader::new(stream);

    // Register SIGINT to exit cleanly
    unsafe {
        libc::signal(libc::SIGINT, sighandler as *const () as libc::sighandler_t);
    }

    println!("[toki] Connected to daemon. Streaming events... (Ctrl+C to stop)");

    for line_result in reader.lines() {
        if !RUNNING.load(Ordering::SeqCst) {
            break;
        }

        let line = match line_result {
            Ok(l) => l,
            Err(_) => {
                eprintln!("[toki] Daemon disconnected.");
                break;
            }
        };

        if line.is_empty() {
            continue;
        }

        let mut v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                if std::env::var("TOKI_DEBUG").is_ok() {
                    eprintln!("[toki] malformed JSON from daemon: {}", e);
                }
                continue;
            }
        };

        match v["type"].as_str() {
            Some("event") => {
                if let Ok(event) = serde_json::from_value::<toki::UsageEvent>(v["data"].take()) {
                    sink.emit_event(&event, None);
                }
            }
            Some("summary") => {
                println!("{}", line);
            }
            _ => {
                println!("{}", line);
            }
        }
    }

    println!("[toki] Disconnected.");
}

// ── Report ──────────────────────────────────────────────

fn handle_report(
    since: Option<String>,
    until: Option<String>,
    group_by_session: bool,
    session_id: Option<String>,
    project: Option<String>,
    command: Option<ReportCommands>,
    config: &Config,
    sink_specs: &[String],
    output_format: toki::sink::OutputFormat,
) {
    let sink = toki::sink::create_sinks(sink_specs, output_format);
    let tz = config.tz;

    // Require daemon to be running
    let pidfile = toki::daemon::default_pidfile_path();
    if toki::daemon::daemon_status(&pidfile).is_none() {
        eprintln!("[toki] Cannot connect to toki daemon.");
        eprintln!("[toki] Start the daemon first: toki daemon start");
        std::process::exit(1);
    }

    let db = match toki::db::Database::open(&config.db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("[toki] Cannot open database: {}", e);
            std::process::exit(1);
        }
    };

    if !toki::query::has_tsdb_data(&db) {
        eprintln!("[toki] No data in TSDB. The daemon may still be performing initial scan.");
        std::process::exit(1);
    }

    let pricing = if config.no_cost {
        None
    } else {
        let (p, _etag) = toki::pricing::fetch_pricing(&db);
        if p.is_empty() { None } else { Some(p) }
    };
    let pricing_ref = pricing.as_ref();

    if group_by_session && command.is_some() {
        eprintln!("[toki] --group-by-session cannot be used with time-based subcommands");
        std::process::exit(1);
    }

    // Handle query subcommand
    if let Some(ReportCommands::Query { query: ref query_str }) = command {
        let parsed = match toki::query_parser::parse(query_str) {
            Ok(q) => q,
            Err(e) => {
                eprintln!("[toki] Query parse error: {}", e);
                std::process::exit(1);
            }
        };
        if let Err(e) = toki::query::execute_parsed_query(&db, &parsed, tz, pricing_ref, sink.as_ref()) {
            eprintln!("[toki] Query execution error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    // No subcommand — build a Query from CLI flags and dispatch
    let command = match command {
        Some(cmd) => cmd,
        None => {
            let query = build_query_from_flags(
                &since, &until, session_id.as_deref(), project.as_deref(),
                if group_by_session { &["session"][..] } else { &[] },
            );
            if let Err(e) = toki::query::execute_parsed_query(&db, &query, tz, pricing_ref, sink.as_ref()) {
                eprintln!("[toki] Query failed: {}", e);
                std::process::exit(1);
            }
            return;
        }
    };

    // Time-grouped subcommands (hourly/daily/weekly/monthly/yearly)
    let (filter_args, group_by) = match &command {
        ReportCommands::Hourly { filter } => (filter.clone(), toki::engine::ReportGroupBy::Hour),
        ReportCommands::Daily { filter } => (filter.clone(), toki::engine::ReportGroupBy::Date),
        ReportCommands::Weekly { start_of_week, filter } => {
            let start = start_of_week.as_deref()
                .map(parse_weekday)
                .unwrap_or(config.start_of_week);
            (filter.clone(), toki::engine::ReportGroupBy::Week { start_of_week: start })
        }
        ReportCommands::Monthly { filter } => (filter.clone(), toki::engine::ReportGroupBy::Month),
        ReportCommands::Yearly { filter } => (filter.clone(), toki::engine::ReportGroupBy::Year),
        ReportCommands::Query { .. } => unreachable!("handled above"),
    };

    // Merge top-level with subcommand (subcommand takes precedence)
    let eff_since = filter_args.since.or(since);
    let eff_until = filter_args.until.or(until);
    let eff_session = filter_args.session_id.or(session_id);
    let eff_project = filter_args.project.or(project);
    let since_dt = parse_opt_range(&eff_since, false, tz);
    let until_dt = parse_opt_range(&eff_until, true, tz);
    validate_range(since_dt, until_dt);

    let requires_range = matches!(
        group_by,
        toki::engine::ReportGroupBy::Hour
            | toki::engine::ReportGroupBy::Date
            | toki::engine::ReportGroupBy::Week { .. }
    );
    if requires_range && !filter_args.from_beginning && since_dt.is_none() {
        eprintln!("[toki] hourly/daily/weekly requires a date range.");
        eprintln!("[toki] Usage: toki report daily --since 20250101");
        eprintln!("[toki]        toki report daily --from-beginning");
        std::process::exit(1);
    }

    let filter = toki::engine::ReportFilter { since: since_dt, until: until_dt, tz };

    match toki::query::report_grouped_from_db(&db, group_by, filter, eff_session.as_deref(), eff_project.as_deref()) {
        Ok(grouped) => sink.emit_grouped(&grouped, group_by.type_name(), pricing_ref),
        Err(e) => {
            eprintln!("[toki] Query failed: {}", e);
            std::process::exit(1);
        }
    }
}

/// Build a Query struct from CLI flags.
fn build_query_from_flags(
    since: &Option<String>,
    until: &Option<String>,
    session_id: Option<&str>,
    project: Option<&str>,
    group_by: &[&str],
) -> toki::query_parser::Query {
    use toki::query_parser::{Query, Metric, LabelFilter};
    let mut filters = Vec::new();
    if let Some(s) = session_id {
        filters.push(LabelFilter { key: "session".into(), value: s.into() });
    }
    if let Some(p) = project {
        filters.push(LabelFilter { key: "project".into(), value: p.into() });
    }
    Query {
        metric: Metric::Usage,
        filters,
        bucket: None,
        group_by: group_by.iter().map(|s| s.to_string()).collect(),
        since: since.clone(),
        until: until.clone(),
    }
}

// ── Helpers ─────────────────────────────────────────────

fn parse_opt_range(value: &Option<String>, is_until: bool, tz: Option<chrono_tz::Tz>) -> Option<NaiveDateTime> {
    value.as_deref().map(|v| {
        toki::query::parse_range_time(v, is_until, tz).unwrap_or_else(|e| {
            let label = if is_until { "--until" } else { "--since" };
            eprintln!("[toki] Invalid {}: {} ({})", label, v, e);
            std::process::exit(1);
        })
    })
}

fn validate_range(since: Option<NaiveDateTime>, until: Option<NaiveDateTime>) {
    if let (Some(s), Some(u)) = (since, until) {
        if u < s {
            eprintln!("[toki] Invalid range: --until is earlier than --since");
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
            eprintln!("[toki] Invalid start-of-week (use mon|tue|wed|thu|fri|sat|sun)");
            std::process::exit(1);
        }
    }
}

extern "C" fn sighandler(_: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}
