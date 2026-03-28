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
        Some(SyncConfig {
            server_addr: server,
            access_token: token,
            device_name: device,
            device_key,
            provider: provider.to_string(),
        })
    }
}

fn gethostname() -> String {
    let mut buf = vec![0i8; 256];
    unsafe {
        libc::gethostname(buf.as_mut_ptr(), buf.len());
    }
    let cstr = buf.iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8 as char)
        .collect::<String>();
    if cstr.is_empty() { "unknown".to_string() } else { cstr }
}

const PING_INTERVAL: Duration = Duration::from_secs(60);

/// Flush notification handle: a Condvar + dirty flag shared with DbWriter.
pub type FlushNotify = Arc<(Mutex<bool>, Condvar)>;

/// Start the sync thread. Returns None if sync is not configured.
pub fn start_sync_thread(
    db: Arc<Database>,
    flush_notify: FlushNotify,
    stop_rx: crossbeam_channel::Receiver<()>,
    provider: String,
) -> Option<std::thread::JoinHandle<()>> {
    let config = SyncConfig::load(&provider)?;

    let handle = std::thread::Builder::new()
        .name(format!("toki-sync-{provider}"))
        .spawn(move || {
            run_sync_loop(db, flush_notify, stop_rx, config);
        })
        .expect("failed to spawn sync thread");

    Some(handle)
}

fn run_sync_loop(
    db: Arc<Database>,
    flush_notify: FlushNotify,
    stop_rx: crossbeam_channel::Receiver<()>,
    mut config: SyncConfig,
) {
    let mut backoff = Backoff::new();
    let mut client: Option<SyncClient> = None;
    let mut last_ping = Instant::now();
    let mut dict_cache: HashMap<u32, String> = HashMap::new();
    // Set after auth succeeds — triggers an immediate initial delta sync
    // without waiting for the next flush notification.
    let mut needs_initial_sync = false;
    let mut last_loop_time = Instant::now();

    loop {
        // Check stop signal
        if stop_rx.try_recv().is_ok() {
            return;
        }

        // Wake detection: if elapsed wall-clock time since last loop iteration
        // is much longer than expected, the machine likely slept.
        let elapsed = last_loop_time.elapsed();
        last_loop_time = Instant::now();
        if client.is_some() && elapsed > PING_INTERVAL * 2 {
            eprintln!("[toki:sync] wake detected, forcing reconnect");
            client = None;
        }

        // Wait for a flush notification or PING timeout.
        // Skip the wait entirely when we are not yet connected (connect immediately
        // on cold start and after disconnect) or when a fresh connection needs its
        // initial delta sync.
        let flush_happened = if client.is_none() || needs_initial_sync {
            // Drain the dirty flag without blocking, then proceed.
            *flush_notify.0.lock().unwrap() = false;
            false
        } else {
            let (lock, cvar) = &*flush_notify;
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

        // Ensure connection
        if client.is_none() {
            let delay = backoff.next_delay();
            if !delay.is_zero() {
                eprintln!("[toki:sync] reconnecting in {:?}", delay);
                if stop_rx.recv_timeout(delay).is_ok() {
                    return; // Stop signal received during backoff
                }
            }

            match SyncClient::connect(&config.server_addr) {
                Ok(mut c) => {
                    match c.auth(&config.access_token, &config.device_name, &config.device_key, &config.provider) {
                        Ok(device_id) => {
                            eprintln!("[toki:sync] connected (device_id={})", truncate(&device_id, 12));
                            backoff.reset();
                            // Reload dict on fresh connection
                            dict_cache = db.load_dict_reverse().unwrap_or_default();
                            client = Some(c);
                            last_ping = Instant::now();
                            // Trigger immediate initial delta sync after auth.
                            needs_initial_sync = true;
                        }
                        Err(AuthError::Rejected { reason, reset_required }) => {
                            eprintln!("[toki:sync] auth rejected: {reason}");
                            if reset_required {
                                eprintln!("[toki:sync] schema mismatch — clearing sync cursor");
                                let key = format!("sync_last_ts_{}", config.provider);
                                let _ = crate::config::set_setting(&key, "0");
                            }
                        }
                        Err(e) => {
                            eprintln!("[toki:sync] auth error: {e}");
                            // Try refreshing the token
                            if try_refresh_token(&mut config) {
                                eprintln!("[toki:sync] token refreshed, retrying auth");
                                backoff.reset();
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[toki:sync] connect failed: {e}");
                }
            }
        }

        let Some(ref mut c) = client else { continue };

        // Sync new events on flush or immediately after a fresh connection.
        if flush_happened || needs_initial_sync {
            needs_initial_sync = false;
            match sync_new_events(c, &db, &mut dict_cache, &config.provider) {
                Ok(synced) => {
                    if synced > 0 {
                        eprintln!("[toki:sync] synced {synced} events");
                    }
                }
                Err(e) => {
                    eprintln!("[toki:sync] sync error: {e}");
                    client = None;
                    continue;
                }
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
                event: event.clone(),
            }
        }).collect();

        match client.sync_batch(items, dict, provider) {
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
