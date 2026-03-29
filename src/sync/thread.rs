use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::db::Database;
use super::backoff::Backoff;
use super::client::{AuthError, SyncClient, BATCH_SIZE};
use super::protocol::SyncItem;

/// Sync configuration read from settings DB.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    pub server_addr: String,   // host:port (e.g. "sync.example.com:9090")
    pub access_token: String,  // JWT
    pub device_name: String,
    /// Stable UUID that uniquely identifies this device.
    /// Generated once at `toki sync enable`, persisted in settings.
    pub device_key: String,
    pub provider: String,
    /// Whether to use TLS for the sync TCP connection.
    /// Defaults to true for non-localhost servers.
    pub use_tls: bool,
    /// Whether to skip TLS certificate verification (for self-signed certs).
    pub tls_insecure: bool,
}

impl SyncConfig {
    /// Returns the default device name (hostname or "unknown").
    pub fn default_device_name() -> String {
        gethostname()
    }

    /// Load from toki settings DB. Returns None if sync is not configured.
    pub fn load(provider: &str) -> Option<Self> {
        let enabled = crate::config::get_setting("sync_enabled")?;
        if enabled != "true" {
            return None;
        }
        let server = crate::config::get_setting("sync_server")?;
        let token = crate::config::get_setting("sync_access_token")?;
        let device = crate::config::get_setting("sync_device_name")
            .unwrap_or_else(gethostname);
        // device_key: Keychain is authoritative (survives settings wipe).
        // Priority: Keychain → settings DB → generate new (save to both).
        let device_key = crate::sync::credentials::load()
            .filter(|c| !c.device_key.is_empty())
            .map(|c| c.device_key.clone())
            .or_else(|| crate::config::get_setting("sync_device_key"))
            .unwrap_or_else(|| {
                let key = uuid::Uuid::new_v4().to_string();
                let _ = crate::config::set_setting("sync_device_key", &key);
                // Also persist to Keychain so it survives a settings DB wipe
                if let Some(mut creds) = crate::sync::credentials::load() {
                    creds.device_key = key.clone();
                    let _ = crate::sync::credentials::save(&creds);
                }
                key
            });
        // TLS: default to true unless explicitly "false" or server is localhost
        let use_tls = match crate::config::get_setting("sync_tls") {
            Some(v) if v == "false" => false,
            Some(v) if v == "true" => true,
            _ => {
                // Auto-detect: disable TLS for localhost/127.0.0.1, enable otherwise
                let host = server.split(':').next().unwrap_or(&server);
                host != "localhost" && host != "127.0.0.1" && host != "::1"
            }
        };

        let tls_insecure = crate::config::get_setting("sync_tls_insecure")
            .map(|v| v == "true")
            .unwrap_or(false);

        Some(SyncConfig {
            server_addr: server,
            access_token: token,
            device_name: device,
            device_key,
            provider: provider.to_string(),
            use_tls,
            tls_insecure,
        })
    }
}

fn gethostname() -> String {
    let mut buf = vec![0u8; 256];
    let ret = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut i8, buf.len()) };
    if ret != 0 {
        return "unknown".to_string();
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

const PING_INTERVAL: Duration = Duration::from_secs(60);

/// Flush notification handle: a Condvar + dirty flag shared with DbWriter.
pub type FlushNotify = Arc<(Mutex<bool>, Condvar)>;

/// Sync toggle: (enabled flag, condvar). When disabled, the sync thread
/// waits on the condvar instead of actively syncing.
pub type SyncToggle = Arc<(Mutex<bool>, Condvar)>;

/// Always spawn a sync thread for the given provider.
/// The thread uses `sync_toggle` to sleep when sync is disabled (CPU 0%).
/// When enabled via settings hot-reload, it wakes up, loads config, and runs.
///
/// If the sync loop panics, it is automatically restarted after a 5-second delay.
/// The thread only exits on a normal stop signal.
pub fn start_sync_thread(
    db: Arc<Database>,
    flush_notify: FlushNotify,
    stop_rx: crossbeam_channel::Receiver<()>,
    provider: String,
    sync_toggle: SyncToggle,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("toki-sync-{provider}"))
        .spawn(move || {
            loop {
                // Check if we should stop before (re)starting
                if stop_rx.try_recv().is_ok() {
                    return;
                }

                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_sync_loop(
                        db.clone(),
                        flush_notify.clone(),
                        stop_rx.clone(),
                        provider.clone(),
                        sync_toggle.clone(),
                    );
                }));

                match result {
                    Ok(()) => return, // Normal exit (stop signal received)
                    Err(e) => {
                        let msg = if let Some(s) = e.downcast_ref::<&str>() {
                            s.to_string()
                        } else if let Some(s) = e.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "unknown panic".to_string()
                        };
                        eprintln!("[toki:sync:{}] thread panicked: {}, restarting in 5s...", provider, msg);

                        // Wait before respawn, but check stop signal
                        if stop_rx.recv_timeout(Duration::from_secs(5)).is_ok() {
                            return;
                        }
                    }
                }
            }
        })
        .expect("failed to spawn sync thread")
}

fn run_sync_loop(
    db: Arc<Database>,
    flush_notify: FlushNotify,
    stop_rx: crossbeam_channel::Receiver<()>,
    provider: String,
    sync_toggle: SyncToggle,
) {
    loop {
        // Wait until sync is enabled (or stop is signaled)
        {
            let (lock, cvar) = &*sync_toggle;
            let mut enabled = lock.lock().unwrap();
            while !*enabled {
                if stop_rx.try_recv().is_ok() {
                    return;
                }
                let (guard, _) = cvar.wait_timeout(enabled, Duration::from_secs(5)).unwrap();
                enabled = guard;
            }
        }

        // Check stop after waking
        if stop_rx.try_recv().is_ok() {
            return;
        }

        // Try to load sync config. Even though toggle says enabled,
        // config may be incomplete (e.g. server not set yet).
        let Some(mut config) = SyncConfig::load(&provider) else {
            eprintln!("[toki:sync:{}] enabled but config incomplete, waiting...", provider);
            // Sleep briefly and re-check toggle / config
            if stop_rx.recv_timeout(Duration::from_secs(5)).is_ok() {
                return;
            }
            continue;
        };

        eprintln!("[toki:sync:{}] starting sync loop", provider);
        run_sync_inner(&db, &flush_notify, &stop_rx, &mut config, &sync_toggle);
        eprintln!("[toki:sync:{}] sync loop paused", provider);
    }
}

/// Inner sync loop: runs until stop signal or sync gets disabled via toggle.
/// Proactive token refresh interval (30 minutes).
const PROACTIVE_REFRESH_INTERVAL: Duration = Duration::from_secs(1800);

fn run_sync_inner(
    db: &Arc<Database>,
    flush_notify: &FlushNotify,
    stop_rx: &crossbeam_channel::Receiver<()>,
    config: &mut SyncConfig,
    sync_toggle: &SyncToggle,
) {
    let mut backoff = Backoff::new();
    let mut client: Option<SyncClient> = None;
    let mut last_ping = Instant::now();
    let mut dict_cache: HashMap<u32, String> = HashMap::new();
    let mut needs_initial_sync = false;
    let mut last_loop_time = Instant::now();
    let mut tls_hint_shown = false;
    let mut last_refresh = Instant::now();
    let mut auth_failure_notified = false;

    loop {
        // Check stop signal
        if stop_rx.try_recv().is_ok() {
            return;
        }

        // Check if sync was disabled (finish current iteration, then return)
        {
            let enabled = sync_toggle.0.lock().unwrap();
            if !*enabled {
                return;
            }
        }

        // Wake detection
        let elapsed = last_loop_time.elapsed();
        last_loop_time = Instant::now();
        if client.is_some() && elapsed > PING_INTERVAL * 2 {
            eprintln!("[toki:sync] wake detected, forcing reconnect");
            client = None;
        }

        // Wait for flush or PING timeout
        let flush_happened = if client.is_none() || needs_initial_sync {
            *flush_notify.0.lock().unwrap() = false;
            false
        } else {
            let (lock, cvar) = &**flush_notify;
            let timeout = PING_INTERVAL.saturating_sub(last_ping.elapsed());
            let guard = lock.lock().unwrap();
            let (mut guard, timeout_result) = cvar.wait_timeout_while(guard, timeout, |dirty| !*dirty).unwrap();
            let happened = !timeout_result.timed_out() || *guard;
            *guard = false;
            happened
        };

        // Check stop again after wakeup
        if stop_rx.try_recv().is_ok() {
            return;
        }

        // Check toggle again after wakeup
        {
            let enabled = sync_toggle.0.lock().unwrap();
            if !*enabled {
                return;
            }
        }

        // Ensure connection
        if client.is_none() {
            let delay = backoff.next_delay();
            if !delay.is_zero() {
                eprintln!("[toki:sync] reconnecting in {:?}", delay);
                if stop_rx.recv_timeout(delay).is_ok() {
                    return;
                }
            }

            match SyncClient::connect(&config.server_addr, config.use_tls, config.tls_insecure) {
                Ok(mut c) => {
                    match c.auth(&config.access_token, &config.device_name, &config.device_key, &config.provider) {
                        Ok(device_id) => {
                            eprintln!("[toki:sync] connected (device_id={})", truncate(&device_id, 12));
                            backoff.reset();
                            dict_cache = db.load_dict_reverse().unwrap_or_default();
                            client = Some(c);
                            last_ping = Instant::now();
                            last_refresh = Instant::now();
                            needs_initial_sync = true;
                            auth_failure_notified = false;
                            let _ = crate::config::set_setting("sync_status", "connected");
                            let _ = crate::config::set_setting("sync_last_success", &now_epoch().to_string());
                        }
                        Err(AuthError::Rejected { reason, reset_required }) => {
                            eprintln!("[toki:sync] auth rejected: {reason}");
                            let _ = crate::config::set_setting("sync_status", "auth_failed");
                            let _ = crate::config::set_setting("sync_last_error", &reason);
                            let _ = crate::config::set_setting("sync_last_error_at", &now_epoch().to_string());
                            if reset_required {
                                eprintln!("[toki:sync] schema mismatch — clearing sync cursor");
                                let key = format!("sync_last_ts_{}", config.provider);
                                let _ = crate::config::set_setting(&key, "0");
                            }
                        }
                        Err(e) => {
                            eprintln!("[toki:sync] auth error: {e}");
                            if try_refresh_token(config) {
                                eprintln!("[toki:sync] token refreshed, retrying auth");
                                backoff.reset();
                                last_refresh = Instant::now();
                            } else {
                                let _ = crate::config::set_setting("sync_status", "token_expired");
                                let _ = crate::config::set_setting("sync_last_error", &format!("{e}"));
                                let _ = crate::config::set_setting("sync_last_error_at", &now_epoch().to_string());
                                if !auth_failure_notified {
                                    send_sync_notification(
                                        "toki sync: re-login required",
                                        "Token expired. Run: toki settings sync disable --keep && toki settings sync enable --server ...",
                                    );
                                    auth_failure_notified = true;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[toki:sync] connect failed: {e}");
                    let _ = crate::config::set_setting("sync_status", "disconnected");
                    let _ = crate::config::set_setting("sync_last_error", &format!("{e}"));
                    let _ = crate::config::set_setting("sync_last_error_at", &now_epoch().to_string());
                    if config.use_tls && !tls_hint_shown {
                        tls_hint_shown = true;
                        eprintln!("[toki:sync] TLS connection failed. Options:");
                        eprintln!("[toki:sync]   - Set up a reverse proxy with TLS (recommended)");
                        eprintln!("[toki:sync]   - Use `toki settings set sync_tls_insecure true` for self-signed certs");
                        eprintln!("[toki:sync]   - Use `toki settings set sync_tls false` for plaintext (LAN only)");
                    }
                }
            }
        }

        let Some(ref mut c) = client else { continue };

        // Sync new events
        if flush_happened || needs_initial_sync {
            needs_initial_sync = false;
            match sync_new_events(c, db, &mut dict_cache, &config.provider) {
                Ok(synced) => {
                    if synced > 0 {
                        eprintln!("[toki:sync] synced {synced} events");
                    }
                    let _ = crate::config::set_setting("sync_status", "connected");
                    let _ = crate::config::set_setting("sync_last_success", &now_epoch().to_string());
                }
                Err(e) => {
                    eprintln!("[toki:sync] sync error: {e}");
                    client = None;
                    continue;
                }
            }
        }

        // Proactive token refresh: keep the refresh token rotated to prevent expiry
        if last_refresh.elapsed() > PROACTIVE_REFRESH_INTERVAL {
            if try_refresh_token(config) {
                eprintln!("[toki:sync] proactive token refresh succeeded");
                last_refresh = Instant::now();
            } else {
                eprintln!("[toki:sync] proactive token refresh failed (will retry)");
            }
        }

        // PING keepalive
        if last_ping.elapsed() >= PING_INTERVAL {
            match c.ping() {
                Ok(()) => { last_ping = Instant::now(); }
                Err(e) => {
                    eprintln!("[toki:sync] ping failed: {e}");
                    client = None;
                }
            }
        }
    }
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn send_sync_notification(title: &str, message: &str) {
    #[cfg(target_os = "macos")]
    {
        let esc_title = escape_applescript(title);
        let esc_msg = escape_applescript(message);
        let _ = std::process::Command::new("osascript")
            .args(["-e", &format!(
                "display notification \"{}\" with title \"{}\"", esc_msg, esc_title
            )])
            .spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("notify-send")
            .args([title, message])
            .spawn();
    }
    // Always log
    eprintln!("[toki:sync] {}: {}", title, message);
}

/// Sync events newer than our last cursor to the server.
/// Returns number of events synced.
fn sync_new_events(
    client: &mut SyncClient,
    db: &Database,
    dict: &mut HashMap<u32, String>,
    provider: &str,
) -> Result<usize, String> {
    // Get server's last known ts
    let server_last_ts = client.get_last_ts(provider)
        .map_err(|e| format!("get_last_ts failed: {e}"))?;

    let cursor_key = format!("sync_last_ts_{provider}");
    let mut since_ms = server_last_ts;
    let mut total_synced = 0;

    loop {
        // Query only BATCH_SIZE events at a time
        let events = db.query_events_range_limit(since_ms.saturating_add(1), i64::MAX, BATCH_SIZE)
            .map_err(|e| format!("query_events_range failed: {e}"))?;

        if events.is_empty() {
            break;
        }

        // Check if any dict IDs in this batch are missing from cache; merge if so
        let needs_reload = events.iter().any(|(_, _, event)| {
            [event.model_id, event.session_id, event.source_file_id, event.project_name_id]
                .iter()
                .any(|id| !dict.contains_key(id))
        });
        if needs_reload {
            if let Ok(fresh) = db.load_dict_reverse() {
                dict.extend(fresh);
            }
        }

        let items: Vec<SyncItem> = events.iter().map(|(ts_ms, _msg_id, event)| {
            SyncItem {
                ts_ms: *ts_ms,
                event: toki_sync_protocol::StoredEvent {
                    model_id: event.model_id,
                    session_id: event.session_id,
                    source_file_id: event.source_file_id,
                    project_name_id: event.project_name_id,
                    tokens: vec![
                        event.input_tokens,
                        event.output_tokens,
                        event.cache_creation_input_tokens,
                        event.cache_read_input_tokens,
                    ],
                },
            }
        }).collect();

        let token_columns: Vec<String> = match provider {
            "codex" => vec!["input".into(), "output".into(), "reasoning_output".into(), "cached_input".into()],
            _ => vec!["input".into(), "output".into(), "cache_create".into(), "cache_read".into()],
        };

        match client.sync_batch(items, dict, provider, token_columns) {
            Ok(ack_ts) => {
                total_synced += events.len();
                since_ms = ack_ts;
                // Persist cursor locally, keyed per provider
                let _ = crate::config::set_setting(&cursor_key, &ack_ts.to_string());
            }
            Err(e) => {
                return Err(format!("sync_batch failed: {e}"));
            }
        }

        // If we got fewer than BATCH_SIZE, we've caught up
        if events.len() < BATCH_SIZE {
            break;
        }
    }

    Ok(total_synced)
}

fn try_refresh_token(config: &mut SyncConfig) -> bool {
    // Load credentials from Keychain/file
    let Some(creds) = crate::sync::credentials::load() else { return false };
    if creds.refresh_token.is_empty() { return false; }

    // Build HTTP URL from credentials
    let http_url = if creds.http_url.is_empty() {
        return false;
    } else {
        creds.http_url.clone()
    };

    // POST /token/refresh
    let resp = match ureq::post(&format!("{http_url}/token/refresh"))
        .send_json(ureq::json!({
            "refresh_token": creds.refresh_token,
        })) {
        Ok(r) => r,
        Err(_) => return false,
    };

    let body: serde_json::Value = match resp.into_json() {
        Ok(v) => v,
        Err(_) => return false,
    };

    let new_access = body["access_token"].as_str().unwrap_or_default();
    let new_refresh = body["refresh_token"].as_str().unwrap_or_default();
    if new_access.is_empty() { return false; }

    // Update credentials
    let mut new_creds = creds;
    new_creds.access_token = new_access.to_string();
    if !new_refresh.is_empty() {
        new_creds.refresh_token = new_refresh.to_string();
    }
    let _ = crate::sync::credentials::save(&new_creds);

    // Update config
    config.access_token = new_creds.access_token;

    // Also update settings DB
    let _ = crate::config::set_setting("sync_access_token", &config.access_token);

    true
}

fn truncate(s: &str, n: usize) -> &str {
    let end = s.char_indices().nth(n).map_or(s.len(), |(i, _)| i);
    &s[..end]
}
