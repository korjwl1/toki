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
        Some(SyncConfig {
            server_addr: server,
            access_token: token,
            device_name: device,
            provider: provider.to_string(),
        })
    }
}

fn gethostname() -> String {
    let mut buf = vec![0i8; 64];
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
    config: SyncConfig,
) {
    let mut backoff = Backoff::new();
    let mut client: Option<SyncClient> = None;
    let mut last_ping = Instant::now();
    let mut dict_cache: HashMap<u32, String> = HashMap::new();

    loop {
        // Check stop signal
        if stop_rx.try_recv().is_ok() {
            return;
        }

        // Wait on condvar (with 60s timeout for PING keepalive)
        let (lock, cvar) = &*flush_notify;
        let timeout = PING_INTERVAL.saturating_sub(last_ping.elapsed());
        let wait_result = {
            let guard = lock.lock().unwrap();
            cvar.wait_timeout_while(guard, timeout, |dirty| !*dirty).unwrap()
        };
        let flush_happened = !wait_result.1.timed_out() || *wait_result.0;
        // Reset dirty flag
        *lock.lock().unwrap() = false;

        // Check stop again after wakeup
        if stop_rx.try_recv().is_ok() {
            return;
        }

        // Ensure connection
        if client.is_none() {
            let delay = backoff.next_delay();
            if !delay.is_zero() {
                eprintln!("[toki:sync] reconnecting in {:?}", delay);
                std::thread::sleep(delay);
            }

            match SyncClient::connect(&config.server_addr) {
                Ok(mut c) => {
                    match c.auth(&config.access_token, &config.device_name, &config.provider) {
                        Ok(device_id) => {
                            eprintln!("[toki:sync] connected (device_id={})", truncate(&device_id, 12));
                            backoff.reset();
                            // Reload dict on fresh connection
                            dict_cache = db.load_dict_reverse().unwrap_or_default();
                            client = Some(c);
                            last_ping = Instant::now();
                        }
                        Err(AuthError::Rejected { reason, reset_required }) => {
                            eprintln!("[toki:sync] auth rejected: {reason}");
                            if reset_required {
                                eprintln!("[toki:sync] schema mismatch — clearing sync cursor");
                                // Clear cursor stored in settings
                                let _ = crate::config::set_setting("sync_last_ts", "0");
                            }
                        }
                        Err(e) => {
                            eprintln!("[toki:sync] auth error: {e}");
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[toki:sync] connect failed: {e}");
                }
            }
        }

        let Some(ref mut c) = client else { continue };

        // Sync new events if flush happened
        if flush_happened {
            match sync_new_events(c, &db, &dict_cache, &config.provider) {
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
    dict: &HashMap<u32, String>,
    provider: &str,
) -> Result<usize, String> {
    // Get server's last known ts
    let server_last_ts = client.get_last_ts()
        .map_err(|e| format!("get_last_ts failed: {e}"))?;

    // Also check our local cursor (use max of the two for safety)
    let local_cursor: i64 = crate::config::get_setting("sync_last_ts")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let since_ms = server_last_ts.max(local_cursor);

    // Query events newer than cursor
    let events = db.query_events_range(since_ms, i64::MAX)
        .map_err(|e| format!("query_events_range failed: {e}"))?;

    if events.is_empty() {
        return Ok(0);
    }

    let mut synced = 0;

    // Send in batches of BATCH_SIZE
    for chunk in events.chunks(BATCH_SIZE) {
        let items: Vec<SyncItem> = chunk.iter().map(|(ts_ms, msg_id, event)| {
            SyncItem {
                ts_ms: *ts_ms,
                message_id: msg_id.clone(),
                event: event.clone(),
            }
        }).collect();

        match client.sync_batch(items, dict, provider) {
            Ok(ack_ts) => {
                synced += chunk.len();
                // Persist cursor locally
                let _ = crate::config::set_setting("sync_last_ts", &ack_ts.to_string());
            }
            Err(e) => {
                return Err(format!("sync_batch failed: {e}"));
            }
        }
    }

    Ok(synced)
}

fn truncate(s: &str, n: usize) -> &str {
    let end = s.char_indices().nth(n).map_or(s.len(), |(i, _)| i);
    &s[..end]
}
