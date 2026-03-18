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
    #[command(subcommand)]
    command: Commands,
}

/// Options shared by trace and report commands.
#[derive(Args, Clone)]
struct ClientOptions {
    /// Output format override: table or json
    #[arg(long)]
    output_format: Option<String>,
    /// Output sink(s): print (default), uds://<path>, http://<url>
    #[arg(long)]
    sink: Vec<String>,
    /// Timezone override (e.g. Asia/Seoul, US/Eastern)
    #[arg(long, short = 'z')]
    timezone: Option<String>,
    /// Disable cost calculation
    #[arg(long)]
    no_cost: bool,
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
        #[command(flatten)]
        opts: ClientOptions,
    },
    /// Report mode: one-shot summary from TSDB
    Report {
        #[command(flatten)]
        opts: ClientOptions,
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
        /// Filter by provider (e.g. claude_code, codex)
        #[arg(long)]
        provider: Option<String>,
        #[command(subcommand)]
        command: Option<ReportCommands>,
    },
    /// Open settings TUI, or set a value non-interactively
    Settings {
        /// Database path (default: ~/.config/toki/toki.fjall)
        #[arg(long)]
        db_path: Option<PathBuf>,
        #[command(subcommand)]
        command: Option<SettingsCommands>,
    },
}

#[derive(Subcommand)]
enum SettingsCommands {
    /// Set a configuration value (e.g. toki settings set timezone Asia/Seoul)
    /// For providers, use --add/--remove (e.g. toki settings set providers --add codex)
    Set {
        /// Setting key
        key: String,
        /// Setting value (not used for providers --add/--remove)
        value: Option<String>,
        /// Add a provider (only for `providers` key)
        #[arg(long)]
        add: Option<String>,
        /// Remove a provider (only for `providers` key)
        #[arg(long)]
        remove: Option<String>,
    },
    /// Get a configuration value
    Get {
        /// Setting key
        key: String,
    },
    /// List all settings
    List,
}

#[derive(Subcommand)]
enum DaemonCommands {
    /// Start the daemon. Detaches to background by default.
    Start {
        /// Run in foreground (don't detach). Useful for debugging.
        #[arg(long)]
        foreground: bool,
    },
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

/// Build Config from defaults + settings file + CLI overrides.
fn build_config(cli_tz: Option<Tz>, cli_no_cost: bool, cli_output_format: Option<&str>) -> Config {
    // Config::new() auto-loads from ~/.config/toki/settings.json
    let mut config = Config::new();

    // CLI overrides take precedence over settings file
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

    match file.try_lock_exclusive() {
        Ok(()) => Ok(file),
        Err(_) => {
            // Lock held — check if the holder is still alive via PID file
            let pidfile = toki::daemon::default_pidfile_path();
            let stale = match toki::daemon::daemon_status(&pidfile) {
                Some(_pid) => false, // Process alive, lock is legit
                None => true,        // No PID or dead process — stale lock
            };

            if stale {
                // Remove stale lock and retry
                drop(file);
                let _ = std::fs::remove_file(&lock_path);
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(&lock_path)?;
                file.try_lock_exclusive()?;
                Ok(file)
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "lock held by running daemon"))
            }
        }
    }
}

fn resolve_output_format(config: &Config) -> toki::sink::OutputFormat {
    match config.output_format.as_str() {
        "json" => toki::sink::OutputFormat::Json,
        _ => toki::sink::OutputFormat::Table,
    }
}

/// Parse and validate ClientOptions into config overrides.
fn parse_client_opts(opts: &ClientOptions) -> (Option<Tz>, bool, Option<&str>) {
    let cli_tz: Option<Tz> = match opts.timezone.as_deref() {
        Some(name) => match name.parse::<Tz>() {
            Ok(tz) => Some(tz),
            Err(_) => {
                eprintln!("[toki] Invalid --timezone: {} (use IANA name like Asia/Seoul)", name);
                std::process::exit(1);
            }
        },
        None => None,
    };

    let cli_output_format = opts.output_format.as_deref();
    if let Some(fmt) = cli_output_format {
        if fmt != "table" && fmt != "json" {
            eprintln!("[toki] Invalid --output-format: {} (use table|json)", fmt);
            std::process::exit(1);
        }
    }

    (cli_tz, opts.no_cost, cli_output_format)
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Settings { db_path: _, command } => {
            match command {
                None => {
                    let restart_requested = toki::settings::run_settings();
                    // Check if TUI requested daemon restart
                    if restart_requested {
                        let config = build_config(None, false, None);
                        stop_running_daemon(&config);
                        eprintln!("[toki] Starting daemon...");
                        let toki_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("toki"));
                        std::process::Command::new(toki_bin)
                            .args(["daemon", "start"])
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::inherit())
                            .spawn()
                            .ok();
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        let pidfile = toki::daemon::default_pidfile_path();
                        if toki::daemon::daemon_status(&pidfile).is_some() {
                            eprintln!("[toki] Daemon restarted.");
                        } else {
                            eprintln!("[toki] Daemon may still be starting...");
                        }
                    }
                }
                Some(SettingsCommands::Set { key, value, add, remove }) => {
                    if key == "providers" {
                        handle_providers_set(add.as_deref(), remove.as_deref());
                    } else {
                        let val = value.unwrap_or_else(|| {
                            eprintln!("[toki] Missing value for setting '{}'", key);
                            eprintln!("[toki] Usage: toki settings set {} <value>", key);
                            std::process::exit(1);
                        });
                        handle_settings_set(&key, &val);
                    }
                }
                Some(SettingsCommands::Get { key }) => {
                    handle_settings_get(&key);
                }
                Some(SettingsCommands::List) => {
                    handle_settings_list();
                }
            }
        }
        Commands::Daemon { command } => {
            let config = build_config(None, false, None);
            handle_daemon(command, &config);
        }
        Commands::Trace { opts } => {
            let (cli_tz, cli_no_cost, cli_output_format) = parse_client_opts(&opts);
            let config = build_config(cli_tz, cli_no_cost, cli_output_format);
            let output_format = resolve_output_format(&config);
            let sink_specs = if opts.sink.is_empty() {
                vec!["print".to_string()]
            } else {
                opts.sink.clone()
            };
            handle_trace(&config, &sink_specs, output_format, config.no_cost);
        }
        Commands::Report { opts, since, until, group_by_session, session_id, project, provider, command } => {
            let (cli_tz, cli_no_cost, cli_output_format) = parse_client_opts(&opts);
            let config = build_config(cli_tz, cli_no_cost, cli_output_format);
            let output_format = resolve_output_format(&config);
            let sink_specs = if opts.sink.is_empty() {
                vec!["print".to_string()]
            } else {
                opts.sink.clone()
            };

            // Validate --provider against known providers
            if let Some(ref p) = provider {
                if !toki::providers::KNOWN_PROVIDERS.contains(&p.as_str()) {
                    eprintln!("[toki] Unknown provider: {}", p);
                    eprintln!("[toki] Known providers: {}", toki::providers::KNOWN_PROVIDERS.join(", "));
                    std::process::exit(1);
                }
            }

            handle_report(since, until, group_by_session, session_id, project, provider, command,
                          &config, &sink_specs, output_format, config.no_cost);
        }
    }
}

// ── Settings (non-interactive) ──────────────────────────

const VALID_SETTINGS: &[&str] = &[
    "claude_code_root", "daemon_sock", "timezone", "output_format",
    "start_of_week", "no_cost", "retention_days", "rollup_retention_days",
    "providers",
];

/// Settings that require daemon restart to take effect.
const DAEMON_SETTINGS: &[&str] = &[
    "claude_code_root", "daemon_sock", "retention_days", "rollup_retention_days",
    "providers",
];

fn handle_settings_set(key: &str, value: &str) {
    if !VALID_SETTINGS.contains(&key) {
        eprintln!("[toki] Unknown setting: {}", key);
        eprintln!("[toki] Valid keys: {}", VALID_SETTINGS.join(", "));
        std::process::exit(1);
    }

    if let Err(e) = toki::config::set_setting(key, value) {
        eprintln!("[toki] Failed to set {}: {}", key, e);
        std::process::exit(1);
    }
    println!("{} = {}", key, value);

    // Prompt daemon restart for daemon-affecting settings
    if DAEMON_SETTINGS.contains(&key) {
        let pidfile = toki::daemon::default_pidfile_path();
        if toki::daemon::daemon_status(&pidfile).is_some() {
            eprintln!("[toki] This setting requires daemon restart to take effect.");
            eprint!("[toki] Restart daemon now? [y/N]: ");
            let mut input = String::new();
            if std::io::stdin().read_line(&mut input).is_ok() && input.trim().eq_ignore_ascii_case("y") {
                let config = Config::new();
                stop_running_daemon(&config);
                RUNNING.store(true, Ordering::Relaxed);
                eprintln!("[toki] Starting daemon...");
                let toki_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("toki"));
                std::process::Command::new(toki_bin)
                    .args(["daemon", "start"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::inherit())
                    .spawn()
                    .ok();
                std::thread::sleep(std::time::Duration::from_secs(1));
                let pidfile = toki::daemon::default_pidfile_path();
                if toki::daemon::daemon_status(&pidfile).is_some() {
                    eprintln!("[toki] Daemon restarted.");
                } else {
                    eprintln!("[toki] Daemon may still be starting...");
                }
            } else {
                eprintln!("[toki] Run `toki daemon restart` to apply.");
            }
        }
    }
}

fn handle_settings_get(key: &str) {
    if !VALID_SETTINGS.contains(&key) {
        eprintln!("[toki] Unknown setting: {}", key);
        eprintln!("[toki] Valid keys: {}", VALID_SETTINGS.join(", "));
        std::process::exit(1);
    }

    if key == "providers" {
        let enabled = toki::config::get_providers();
        let config = Config::new();
        let all_providers = toki::providers::create_providers(
            &toki::providers::KNOWN_PROVIDERS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &config,
        );

        println!("{:<16} {:<16} {:<30} {}", "ID", "Name", "Root", "Status");
        println!("{}", "-".repeat(76));
        for provider in &all_providers {
            let root = provider.root_dir().unwrap_or_else(|| "(not found)".to_string());
            let status = if enabled.contains(&provider.name().to_string()) {
                "[enabled]"
            } else {
                "[disabled]"
            };
            println!("{:<16} {:<16} {:<30} {}", provider.name(), provider.display_name(), root, status);
        }
        return;
    }

    match toki::config::get_setting(key) {
        Some(v) => println!("{}", v),
        None => println!("(not set)"),
    }
}

fn handle_settings_list() {
    let settings = toki::config::list_settings();
    for key in VALID_SETTINGS {
        let value = settings.get(*key).map(|s| s.as_str()).unwrap_or("(not set)");
        println!("{:>24} = {}", key, value);
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

fn start_daemon_detached() {
    let toki_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("toki"));

    // Check if already running
    let pidfile = toki::daemon::default_pidfile_path();
    if let Some(pid) = toki::daemon::daemon_status(&pidfile) {
        eprintln!("[toki] Daemon already running (PID {})", pid);
        std::process::exit(1);
    }

    // Truncate log file for fresh start
    let log_path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/toki/daemon.log");

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
        .unwrap_or_else(|e| {
            eprintln!("[toki] Failed to open log file {}: {}", log_path.display(), e);
            std::process::exit(1);
        });

    let mut child = std::process::Command::new(&toki_bin)
        .args(["daemon", "start", "--foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(log_file.try_clone().unwrap_or_else(|_| {
            std::fs::File::open("/dev/null").unwrap()
        }))
        .stderr(log_file)
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("[toki] Failed to start daemon: {}", e);
            std::process::exit(1);
        });

    // Wait for daemon to become ready (PID file + Listening) or die
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(30);

    loop {
        // Check if child already exited (crash)
        if let Ok(Some(status)) = child.try_wait() {
            let log_tail = std::fs::read_to_string(&log_path)
                .unwrap_or_default()
                .lines()
                .rev()
                .take(5)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            eprintln!("[toki] Daemon exited immediately ({})", status);
            if !log_tail.is_empty() {
                eprintln!("{}", log_tail);
            }
            std::process::exit(1);
        }

        // Check if socket is ready (Listening)
        if let Ok(content) = std::fs::read_to_string(&log_path) {
            if content.contains("Listening") {
                let pid = toki::daemon::daemon_status(&pidfile)
                    .map(|p| p as u32)
                    .unwrap_or(child.id());
                eprintln!("[toki] Daemon started (PID {})", pid);
                return;
            }
        }

        if start.elapsed() > timeout {
            eprintln!("[toki] Daemon did not become ready within 30s");
            eprintln!("[toki] Check log: {}", log_path.display());
            return;
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
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
    eprintln!("[toki:daemon] Providers: {:?}", config.providers);
    eprintln!("[toki:daemon] Database dir: {}", config.db_base_dir.display());
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
    let listener_dbs: Vec<(String, Arc<toki::db::Database>)> = handle.dbs().into_iter()
        .map(|(name, db)| (name.to_string(), db.clone()))
        .collect();
    let listener_handle = std::thread::Builder::new()
        .name("toki-listener".to_string())
        .spawn(move || {
            toki::daemon::run_listener(&listener_sock, listener_broadcast, listener_dbs, listener_stop_rx);
        })
        .expect("Failed to spawn listener thread");

    unsafe {
        libc::signal(libc::SIGINT, sighandler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, sighandler as *const () as libc::sighandler_t);
    }

    eprintln!("[toki:daemon] Running (PID {}). Send SIGTERM or use 'toki daemon stop' to stop.",
        std::process::id());

    while RUNNING.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    eprintln!("\n[toki:daemon] Shutting down... (press Ctrl+C again to force)");

    let t0 = std::time::Instant::now();
    let _ = listener_stop_tx.send(());
    let _ = listener_handle.join();
    eprintln!("[toki:daemon] Listener stopped ({}ms)", t0.elapsed().as_millis());

    let t1 = std::time::Instant::now();
    handle.stop();
    eprintln!("[toki:daemon] Engine + writers stopped ({}ms)", t1.elapsed().as_millis());

    toki::daemon::remove_pidfile(&pidfile);
    let _ = std::fs::remove_file(&sock_path);
    eprintln!("[toki:daemon] Done (total {}ms).", t0.elapsed().as_millis());
}

fn handle_daemon(command: DaemonCommands, config: &Config) {
    match command {
        DaemonCommands::Start { foreground } => {
            if foreground {
                run_daemon_foreground(config);
            } else {
                start_daemon_detached();
            }
        }

        DaemonCommands::Stop => {
            stop_running_daemon(config);
        }

        DaemonCommands::Restart => {
            stop_running_daemon(config);
            RUNNING.store(true, Ordering::Relaxed);
            start_daemon_detached();
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

            let mut deleted_any = false;

            // Delete legacy DB path
            if config.db_path.exists() {
                if let Err(e) = std::fs::remove_dir_all(&config.db_path) {
                    eprintln!("[toki] Failed to delete database: {}", e);
                    std::process::exit(1);
                }
                println!("[toki] Database deleted: {}", config.db_path.display());
                deleted_any = true;
            }

            // Delete per-provider DB directories (e.g., claude_code.fjall, codex.fjall)
            for provider_name in toki::providers::KNOWN_PROVIDERS {
                let provider_db = config.db_base_dir.join(format!("{}.fjall", provider_name));
                if provider_db.exists() {
                    if let Err(e) = std::fs::remove_dir_all(&provider_db) {
                        eprintln!("[toki] Failed to delete {}: {}", provider_db.display(), e);
                    } else {
                        println!("[toki] Database deleted: {}", provider_db.display());
                        deleted_any = true;
                    }
                }
            }

            if !deleted_any {
                println!("[toki] No databases found in {}", config.db_base_dir.display());
            }
            println!("[toki] Reset complete. Start the daemon to rebuild: toki daemon start");
        }
    }
}

// ── Trace (client) ──────────────────────────────────────

fn handle_trace(config: &Config, sink_specs: &[String], output_format: toki::sink::OutputFormat, no_cost: bool) {
    let sock_path = config.daemon_sock.clone();

    let stream = match UnixStream::connect(&sock_path) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("[toki] Cannot connect to daemon at {}", sock_path.display());
            eprintln!("[toki] Start the daemon first: toki daemon start");
            std::process::exit(1);
        }
    };

    // Load pricing client-side (ETag-based fetch, file cache)
    let pricing_cache_path = toki::pricing::default_cache_path();
    let mut pricing = if no_cost {
        None
    } else {
        let p = toki::pricing::fetch_pricing(&pricing_cache_path);
        if p.is_empty() { None } else { Some(p) }
    };
    let mut last_pricing_refresh = std::time::Instant::now();

    let sink = toki::sink::create_sinks(sink_specs, output_format);
    let reader = BufReader::new(stream);

    // Register SIGINT to exit cleanly
    unsafe {
        libc::signal(libc::SIGINT, sighandler as *const () as libc::sighandler_t);
    }

    println!("[toki] Connected to daemon. Streaming events... (Ctrl+C to stop)");

    for line_result in reader.lines() {
        if !RUNNING.load(Ordering::Relaxed) {
            break;
        }

        // Periodic pricing refresh (24h)
        if !no_cost && last_pricing_refresh.elapsed() >= std::time::Duration::from_secs(86400) {
            let p = toki::pricing::fetch_pricing(&pricing_cache_path);
            if !p.is_empty() {
                pricing = Some(p);
            }
            last_pricing_refresh = std::time::Instant::now();
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
                    sink.emit_event(&event, pricing.as_ref(), None);
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
    provider: Option<String>,
    command: Option<ReportCommands>,
    config: &Config,
    sink_specs: &[String],
    output_format: toki::sink::OutputFormat,
    no_cost: bool,
) {
    let sink = toki::sink::create_sinks(sink_specs, output_format);
    let tz = config.tz;
    let sock_path = config.daemon_sock.clone();

    // Require daemon to be running
    let pidfile = toki::daemon::default_pidfile_path();
    if toki::daemon::daemon_status(&pidfile).is_none() {
        eprintln!("[toki] Cannot connect to toki daemon.");
        eprintln!("[toki] Start the daemon first: toki daemon start");
        std::process::exit(1);
    }

    if group_by_session && command.is_some() {
        eprintln!("[toki] --group-by-session cannot be used with time-based subcommands");
        std::process::exit(1);
    }

    // Build query string from CLI arguments
    let query_str = if let Some(ReportCommands::Query { ref query }) = command {
        query.clone()
    } else {
        let query = if let Some(cmd) = command {
            // Time-grouped subcommands — destructure directly to avoid cloning
            let (filter_args, group_by) = match cmd {
                ReportCommands::Hourly { filter } => (filter, toki::engine::ReportGroupBy::Hour),
                ReportCommands::Daily { filter } => (filter, toki::engine::ReportGroupBy::Date),
                ReportCommands::Weekly { start_of_week, filter } => {
                    let start = start_of_week.as_deref()
                        .map(parse_weekday)
                        .unwrap_or(config.start_of_week);
                    (filter, toki::engine::ReportGroupBy::Week { start_of_week: start })
                }
                ReportCommands::Monthly { filter } => (filter, toki::engine::ReportGroupBy::Month),
                ReportCommands::Yearly { filter } => (filter, toki::engine::ReportGroupBy::Year),
                ReportCommands::Query { .. } => unreachable!(),
            };

            let eff_since = filter_args.since.or(since.clone());
            let eff_until = filter_args.until.or(until.clone());
            let eff_session = filter_args.session_id.or(session_id.clone());
            let eff_project = filter_args.project.or(project.clone());

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

            // Convert to PromQL-style query string
            build_query_from_flags(
                &eff_since, &eff_until, eff_session.as_deref(), eff_project.as_deref(),
                provider.as_deref(),
                &[], // group_by handled via bucket
            ).to_query_string_with_bucket(group_by)
        } else {
            // No subcommand — summary or session grouping
            build_query_from_flags(
                &since, &until, session_id.as_deref(), project.as_deref(),
                provider.as_deref(),
                if group_by_session { &["session"][..] } else { &[] },
            ).to_query_string()
        };
        query
    };

    // Send query to daemon via UDS
    let response = send_report_query(&sock_path, &query_str, tz);

    // Load pricing client-side (file cache, no DB)
    let pricing = if no_cost {
        None
    } else {
        let p = toki::pricing::fetch_pricing(&toki::pricing::default_cache_path());
        if p.is_empty() { None } else { Some(p) }
    };

    match response {
        Ok(data) => {
            for item in data.as_array().unwrap_or(&vec![]) {
                dispatch_result_to_sink(item, sink.as_ref(), pricing.as_ref());
            }
        }
        Err(e) => {
            eprintln!("[toki] {}", e);
            std::process::exit(1);
        }
    }
}

/// Send a report query to the daemon via UDS and return the response.
fn send_report_query(
    sock_path: &std::path::Path,
    query: &str,
    tz: Option<chrono_tz::Tz>,
) -> Result<serde_json::Value, String> {
    use std::io::{BufRead, Write};

    let mut stream = UnixStream::connect(sock_path)
        .map_err(|_| "Cannot connect to daemon. Start it first: toki daemon start".to_string())?;

    // Send request
    let request = serde_json::json!({
        "query": query,
        "tz": tz.map(|t| t.to_string()),
    });
    let line = serde_json::to_string(&request).unwrap();
    writeln!(stream, "{}", line).map_err(|e| format!("Failed to send query: {}", e))?;
    stream.flush().map_err(|e| format!("Failed to flush: {}", e))?;

    // Read response
    stream.set_read_timeout(Some(std::time::Duration::from_secs(60))).ok();
    let mut reader = std::io::BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line)
        .map_err(|e| format!("Failed to read response: {}", e))?;

    let resp: serde_json::Value = serde_json::from_str(&response_line)
        .map_err(|e| format!("Invalid response: {}", e))?;

    if resp["ok"].as_bool() == Some(true) {
        Ok(resp["data"].clone())
    } else {
        Err(resp["error"].as_str().unwrap_or("Unknown error").to_string())
    }
}

/// Dispatch a JSON result from the daemon to the local sink for display.
/// Detects a "schema" field in the response to use the correct provider schema for rendering.
fn dispatch_result_to_sink(
    item: &serde_json::Value,
    sink: &dyn toki::sink::Sink,
    pricing: Option<&toki::pricing::PricingTable>,
) {
    // Detect schema from daemon response tag
    let schema: Option<&dyn toki::common::schema::ProviderSchema> = item["schema"].as_str()
        .map(toki::common::schema::schema_for_provider);

    match item["type"].as_str() {
        Some("summary") => {
            // data is an array of model summaries → convert to HashMap
            if let Ok(summaries_vec) = serde_json::from_value::<Vec<toki::ModelUsageSummary>>(item["data"].clone()) {
                let summaries: std::collections::HashMap<String, toki::ModelUsageSummary> =
                    summaries_vec.into_iter().map(|s| (s.model.clone(), s)).collect();
                sink.emit_summary(&summaries, pricing, schema);
            }
        }
        Some(type_name) if type_name == "sessions" || type_name == "projects" => {
            if let Some(items) = item["items"].as_array() {
                let strings: Vec<String> = items.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                // Show provider label if present
                if let Some(provider) = schema {
                    let name = provider.provider_name();
                    if !name.is_empty() {
                        eprintln!("[toki] {}", name);
                    }
                }
                sink.emit_list(&strings, type_name);
            }
        }
        Some(type_name) => {
            // Grouped data: { data: [ { period: "...", usage_per_models: [...] } ] }
            if let Some(data_arr) = item["data"].as_array() {
                let mut grouped: std::collections::HashMap<String, std::collections::HashMap<String, toki::ModelUsageSummary>> =
                    std::collections::HashMap::new();
                for entry in data_arr {
                    let period = entry["period"].as_str()
                        .or_else(|| entry["provider"].as_str())
                        .or_else(|| entry["session"].as_str())
                        .unwrap_or("total").to_string();
                    if let Ok(models) = serde_json::from_value::<Vec<toki::ModelUsageSummary>>(entry["usage_per_models"].clone()) {
                        let map: std::collections::HashMap<String, toki::ModelUsageSummary> =
                            models.into_iter().map(|s| (s.model.clone(), s)).collect();
                        grouped.insert(period, map);
                    }
                }
                sink.emit_grouped(&grouped, type_name, pricing, schema);
            }
        }
        None => {}
    }
}

/// Build a Query struct from CLI flags.
fn build_query_from_flags(
    since: &Option<String>,
    until: &Option<String>,
    session_id: Option<&str>,
    project: Option<&str>,
    provider: Option<&str>,
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
        provider: provider.map(|s| s.to_string()),
    }
}

// ── Provider management (via settings set providers) ─────

fn handle_providers_set(add: Option<&str>, remove: Option<&str>) {
    match (add, remove) {
        (Some(name), None) => {
            // --add
            if !toki::providers::KNOWN_PROVIDERS.contains(&name) {
                eprintln!("[toki] Unknown provider: {}", name);
                eprintln!("[toki] Known providers: {}", toki::providers::KNOWN_PROVIDERS.join(", "));
                std::process::exit(1);
            }

            let mut providers = toki::config::get_providers();
            if providers.contains(&name.to_string()) {
                println!("[toki] Provider '{}' is already enabled.", name);
                return;
            }

            providers.push(name.to_string());
            if let Err(e) = toki::config::set_setting_array("providers", &providers) {
                eprintln!("[toki] Failed to save: {}", e);
                std::process::exit(1);
            }
            println!("[toki] Provider '{}' added.", name);
            println!("[toki] Active providers: [{}]", providers.join(", "));

            let pidfile = toki::daemon::default_pidfile_path();
            if toki::daemon::daemon_status(&pidfile).is_some() {
                eprintln!("[toki] Restart the daemon to pick up the new provider:");
                eprintln!("[toki]   toki daemon restart");
            }
        }

        (None, Some(name)) => {
            // --remove
            let mut providers = toki::config::get_providers();
            if !providers.contains(&name.to_string()) {
                println!("[toki] Provider '{}' is not enabled.", name);
                return;
            }

            providers.retain(|p| p != name);
            if let Err(e) = toki::config::set_setting_array("providers", &providers) {
                eprintln!("[toki] Failed to save: {}", e);
                std::process::exit(1);
            }
            println!("[toki] Provider '{}' removed.", name);
            println!("[toki] Active providers: [{}]", providers.join(", "));

            let pidfile = toki::daemon::default_pidfile_path();
            if toki::daemon::daemon_status(&pidfile).is_some() {
                eprintln!("[toki] Restart the daemon to apply:");
                eprintln!("[toki]   toki daemon restart");
            }
        }

        (None, None) => {
            eprintln!("[toki] Usage: toki settings set providers --add <name>");
            eprintln!("[toki]        toki settings set providers --remove <name>");
            eprintln!("[toki] To view providers: toki settings get providers");
            eprintln!("[toki] Known providers: {}", toki::providers::KNOWN_PROVIDERS.join(", "));
            std::process::exit(1);
        }

        (Some(_), Some(_)) => {
            eprintln!("[toki] Cannot use --add and --remove at the same time.");
            std::process::exit(1);
        }
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
    if !RUNNING.load(Ordering::Relaxed) {
        // Second signal — force exit immediately
        std::process::exit(1);
    }
    RUNNING.store(false, Ordering::Relaxed);
}
