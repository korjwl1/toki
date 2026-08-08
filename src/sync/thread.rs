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
        let device_key = crate::config::device_id();
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
    let ret = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if ret != 0 {
        return "unknown".to_string();
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

const PING_INTERVAL: Duration = Duration::from_secs(60);
/// Minimum interval between window-set uploads (full recent set, idempotent).
const WINDOWS_SYNC_INTERVAL: Duration = Duration::from_secs(300);
/// Window rows younger than this are (re)sent each cycle.
const WINDOWS_SYNC_HORIZON_MS: i64 = 60 * 86_400_000;

/// Whether the server supports SyncWindows (0x24). Older servers DROP the TCP
/// connection on unknown frames without any SyncErr, so support must be
/// confirmed out of band via HTTP GET /api/v1/capabilities before sending.
#[derive(Debug, Clone, Copy, PartialEq)]
enum WindowsCapability {
    /// Not yet determined (probe failed transiently, or not probed).
    Unknown,
    Supported,
    /// Authoritative 404 / missing flag. Re-probed after a long TTL so a
    /// rolling server upgrade doesn't require every daemon to restart.
    Unsupported,
}

/// How long an authoritative "unsupported" verdict holds before re-probing.
/// Must match the server's per-batch keep limit (`toki_sync`'s
/// `MAX_WINDOWS_PER_BATCH`). Rows beyond it are dropped server-side WITHOUT
/// being reported as dropped, so the client selects the survivors itself.
const MAX_WINDOWS_PER_SYNC: usize = 2_000;

const CAP_UNSUPPORTED_TTL: Duration = Duration::from_secs(6 * 3600);

/// Probe the server's capabilities endpoint. Ok(Some(bool)) is authoritative;
/// Ok(None)/Err are transient (retry later, never latch).
fn probe_windows_capability(http_url: &str, tls_insecure: bool) -> Option<bool> {
    let url = format!("{}/api/v1/capabilities", http_url.trim_end_matches('/'));
    // Must honour `sync_tls_insecure` exactly like the TCP path: on a
    // self-signed deployment (a mode this client itself recommends) a strict
    // agent fails the probe forever, so event sync works and window sync
    // silently never starts.
    let agent = if tls_insecure {
        // Connector build failure is transient-ish, not "unsupported".
        insecure_agent()?
    } else {
        ureq::Agent::new()
    };
    match agent.get(&url).timeout(Duration::from_secs(5)).call() {
        Ok(resp) => {
            let body: serde_json::Value = resp.into_json().ok()?;
            Some(body.get("sync_windows_v1").and_then(|v| v.as_bool()).unwrap_or(false))
        }
        Err(ureq::Error::Status(404, _)) => Some(false),
        Err(_) => None,
    }
}

/// Agent that skips certificate verification, for self-signed deployments
/// (`sync_tls_insecure`). Mirrors what `toki settings sync enable --insecure`
/// builds for the TCP path.
fn insecure_agent() -> Option<ureq::Agent> {
    let tls = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .ok()?;
    Some(ureq::AgentBuilder::new().tls_connector(std::sync::Arc::new(tls)).build())
}


/// Test-only entry point for the end-to-end integration test, which drives a
/// real `SyncClient` against a containerized server. Returns whether the batch
/// was sent AND acknowledged.
pub fn windows_sync_step_for_test(
    db: &crate::db::Database,
    provider: &str,
    now_ms: i64,
    last_fingerprint: (usize, u64),
    client: &mut crate::sync::client::SyncClient,
) -> bool {
    matches!(
        windows_sync_step(db, provider, now_ms, last_fingerprint, client).outcome,
        WindowsSyncOutcome::Sent { .. }
    )
}

/// Anything that can deliver a windows batch. Exists so `windows_sync_step`
/// can be exercised without a TCP connection — the regression this indirection
/// is here to prevent was a build-the-payload-and-drop-it edit that no test
/// could observe.
pub(crate) trait WindowSender {
    fn send_windows(
        &mut self,
        provider: &str,
        items: Vec<toki_sync_protocol::WireWindow>,
    ) -> std::io::Result<()>;
}

impl WindowSender for crate::sync::client::SyncClient {
    fn send_windows(
        &mut self,
        provider: &str,
        items: Vec<toki_sync_protocol::WireWindow>,
    ) -> std::io::Result<()> {
        self.sync_windows(provider, items)
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum WindowsSyncOutcome {
    /// Nothing to send: empty set, or unchanged since the last ACCEPTED upload.
    Skipped,
    Sent { count: usize, fingerprint: (usize, u64) },
    /// Server said no (SyncErr). Connection stays usable, fingerprint must not
    /// latch, so the same set is retried.
    Rejected(String),
    /// Transport failure — caller drops the connection and reconnects.
    Disconnected(String),
}

pub(crate) struct WindowsSyncStep {
    pub outcome: WindowsSyncOutcome,
    /// Rows the per-sync cap excluded, for the caller's (throttled) log.
    pub over_cap_dropped: usize,
    /// True when there was nothing to send AND re-checking is cheap: either
    /// the store has not been written since the last attempt (one atomic
    /// load), or it holds no rows in the horizon at all. The caller must NOT
    /// burn the 5-minute interval on this — it is exactly the state a
    /// just-started daemon is in while its backfill is still running, and
    /// waiting it out delayed the first upload by a full interval.
    pub nothing_stored_yet: bool,
}

impl WindowsSyncStep {
    fn skipped() -> Self {
        WindowsSyncStep {
            outcome: WindowsSyncOutcome::Skipped,
            over_cap_dropped: 0,
            nothing_stored_yet: false,
        }
    }
}

/// One windows upload attempt: fingerprint the eligible set, decide whether it
/// changed, and if so build and send exactly the rows the server will keep.
pub(crate) fn windows_sync_step(
    db: &crate::db::Database,
    provider: &str,
    now_ms: i64,
    last_fingerprint: (usize, u64),
    sender: &mut dyn WindowSender,
) -> WindowsSyncStep {
    windows_sync_step_gated(db, provider, now_ms, last_fingerprint, None, sender).0
}

/// As above, but skips the scan entirely when the DB's write counter proves
/// nothing has been stored since the last attempt. The steady state is
/// "unchanged, skip", and reaching that conclusion should not cost a full
/// keyspace scan with a bincode decode and three String allocations per row
/// every five minutes. Returns the counter to carry into the next call.
pub(crate) fn windows_sync_step_gated(
    db: &crate::db::Database,
    provider: &str,
    now_ms: i64,
    last_fingerprint: (usize, u64),
    last_writes: Option<u64>,
    sender: &mut dyn WindowSender,
) -> (WindowsSyncStep, u64) {
    let writes = db.window_writes();
    if last_writes == Some(writes) {
        let mut step = WindowsSyncStep::skipped();
        step.nothing_stored_yet = true;
        return (step, writes);
    }
    (windows_sync_step_inner(db, provider, now_ms, last_fingerprint, sender), writes)
}

fn windows_sync_step_inner(
    db: &crate::db::Database,
    provider: &str,
    now_ms: i64,
    last_fingerprint: (usize, u64),
    sender: &mut dyn WindowSender,
) -> WindowsSyncStep {
    // Pass 1: fingerprint only (no WireWindow/String allocation) — the common
    // steady-state outcome is "unchanged, skip", and it shouldn't pay the full
    // item build every 5 minutes.
    let mut count = 0usize;
    let mut acc: u64 = 0;
    let _ = db.for_each_window_in(now_ms - WINDOWS_SYNC_HORIZON_MS, i64::MAX, |key, snap| {
        count += 1;
        acc = acc.wrapping_mul(31).wrapping_add(window_row_fold(key, &snap));
    });

    // Over the server's cap the server keeps only the newest
    // MAX_WINDOWS_PER_SYNC and drops the rest WITHOUT reporting them as
    // dropped — so sending the whole set would latch a fingerprint for rows
    // that were never stored, losing them permanently and silently. Decide
    // here which rows are ours to send, and fingerprint exactly those.
    //
    // Selection = the newest rows by (anchor, key hash), which pass 2
    // reproduces with the identical comparison. Costs one extra scan, but only
    // in the over-cap case, which uploads anyway.
    let mut cutoff: Option<(i64, u64)> = None;
    let mut over_cap_dropped = 0usize;
    let (count, acc) = if count > MAX_WINDOWS_PER_SYNC {
        let mut ids: Vec<(i64, u64, u64)> = Vec::with_capacity(count);
        let _ = db.for_each_window_in(now_ms - WINDOWS_SYNC_HORIZON_MS, i64::MAX, |key, snap| {
            ids.push((
                crate::windows::window_key_anchor_ms(key).unwrap_or(i64::MIN),
                crate::windows::hash_bytes(key),
                window_row_fold(key, &snap),
            ));
        });
        ids.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        over_cap_dropped = ids.len().saturating_sub(MAX_WINDOWS_PER_SYNC);
        ids.truncate(MAX_WINDOWS_PER_SYNC);
        let mut acc2: u64 = 0;
        for &(_, _, fold) in &ids {
            acc2 = acc2.wrapping_mul(31).wrapping_add(fold);
        }
        if let Some(&(a, k, _)) = ids.last() {
            cutoff = Some((a, k));
        }
        (ids.len(), acc2)
    } else {
        (count, acc)
    };

    let fingerprint = (count, acc);
    if fingerprint == last_fingerprint || count == 0 {
        return WindowsSyncStep {
            outcome: WindowsSyncOutcome::Skipped,
            over_cap_dropped,
            // An empty horizon is the backfill-still-running case; an
            // unchanged fingerprint means we genuinely looked at real rows and
            // they had not moved, which IS worth the full interval.
            nothing_stored_yet: count == 0,
        };
    }
    let items = collect_window_items(db, now_ms, cutoff, count);
    let n = items.len();
    let outcome = match sender.send_windows(provider, items) {
        Ok(()) => WindowsSyncOutcome::Sent { count: n, fingerprint },
        // ErrorKind::Other is how the client reports a server-side SyncErr;
        // anything else means the link itself is gone.
        Err(e) if e.kind() == std::io::ErrorKind::Other => {
            WindowsSyncOutcome::Rejected(e.to_string())
        }
        Err(e) => WindowsSyncOutcome::Disconnected(e.to_string()),
    };
    WindowsSyncStep { outcome, over_cap_dropped, nothing_stored_yet: false }
}

/// Pass 2 of the windows upload: materialize the wire items for the rows the
/// fingerprint pass selected. `cutoff` is `None` below the per-sync cap and
/// otherwise the `(anchor, key_hash)` low-water mark of the selected set —
/// applied with the identical comparison, so the two passes cannot diverge.
fn collect_window_items(
    db: &crate::db::Database,
    now_ms: i64,
    cutoff: Option<(i64, u64)>,
    capacity: usize,
) -> Vec<toki_sync_protocol::WireWindow> {
    let mut items = Vec::with_capacity(capacity);
    let _ = db.for_each_window_in(now_ms - WINDOWS_SYNC_HORIZON_MS, i64::MAX, |key, snap| {
        if let Some((ca, ck)) = cutoff {
            let a = crate::windows::window_key_anchor_ms(key).unwrap_or(i64::MIN);
            if (a, crate::windows::hash_bytes(key)) < (ca, ck) {
                return;
            }
        }
        items.push(crate::windows::wire_from_stored(key, &snap));
    });
    items
}

/// Per-row fingerprint contribution. Extracted so the fingerprint pass and the
/// over-cap selection pass cannot drift: if they disagree, the client latches a
/// fingerprint for a set it never sent.
fn window_row_fold(key: &[u8], snap: &crate::windows::WindowSnapshotV1) -> u64 {
    let mut acc: u64 = 0;
    let mut fold = |v: u64| acc = acc.wrapping_mul(31).wrapping_add(v);
    for &b in key {
        fold(b as u64);
    }
    fold(snap.peak_pct_x100 as u64);
    fold(snap.last_pct_x100 as u64);
    fold(snap.observed_ts_ms as u64);
    fold(snap.raw_resets_at_ms as u64);
    fold(snap.first_seen_ms as u64);
    fold(snap.window_minutes as u64);
    fold(snap.finalized as u64);
    fold(snap.maxed_out as u64);
    fold(snap.limit_reached_kind as u64);
    fold(snap.active_ms);
    fold(snap.time_to_100_ms as u64);
    fold(snap.last_sample_gap_ms as u64);
    fold(snap.sampled_active_fraction as u64);
    fold(snap.n_samples as u64);
    fold(crate::windows::hash_str(&snap.plan));
    acc
}

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
    let mut sw = SyncStateWriter::new();
    let mut windows_cap = WindowsCapability::Unknown;
    // Start "due": first successful connection uploads the window set right away.
    let mut last_windows_sync = Instant::now() - WINDOWS_SYNC_INTERVAL;
    // Transient capability-probe failures retry with a backoff, not per-wake.
    let mut next_cap_probe = Instant::now();
    let mut cap_probe_failure_logged = false;
    let mut last_over_cap_logged: usize = 0;
    let mut last_windows_writes: Option<u64> = None;
    // Fingerprint of the last uploaded window set — identical sets skip the
    // resend entirely (serialization + network) while staying cursorless.
    // Folds every merge-visible field: a finalize-only change keeps count,
    // observed_ts, and peak identical, and skipping it would leave the
    // server's copy unfinalized forever.
    let mut last_windows_fingerprint: (usize, u64) = (0, 0);

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

        // Wait for flush notification or PING timeout.
        // Skip wait if connecting or doing initial catch-up.
        if client.is_some() && !needs_initial_sync {
            let (lock, cvar) = &**flush_notify;
            let guard = lock.lock().unwrap();
            let timeout = PING_INTERVAL.saturating_sub(last_ping.elapsed());
            let (mut guard, _) = cvar.wait_timeout_while(
                guard, timeout, |dirty| !*dirty
            ).unwrap();
            *guard = false;

            // Check stop/toggle after wakeup
            if stop_rx.try_recv().is_ok() { return; }
            let enabled = sync_toggle.0.lock().unwrap();
            if !*enabled { return; }
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
                            // A reconnect may face a restored/reset server:
                            // never let a stale fingerprint suppress the first
                            // upload of the new session.
                            last_windows_fingerprint = (0, 0);
                            last_windows_sync = Instant::now() - WINDOWS_SYNC_INTERVAL;
                            auth_failure_notified = false;
                            sw.set("sync_status", "connected");
                            sw.set("sync_last_success", &now_epoch().to_string());
                        }
                        Err(AuthError::Rejected { reason, reset_required }) => {
                            eprintln!("[toki:sync] auth rejected: {reason}");

                            if reason.contains("device_removed") {
                                eprintln!("[toki:sync] device was removed from server — disabling sync");
                                let _ = crate::config::set_setting("sync_enabled", "false");
                                sw.set("sync_status", "device_removed");
                                {
                                    let mut enabled = sync_toggle.0.lock().unwrap();
                                    *enabled = false;
                                }
                                send_sync_notification(
                                    "toki sync: device removed",
                                    "This device was removed from the server. Sync has been disabled. Re-enable with: toki settings sync enable --server ...",
                                );
                                return;
                            }

                            // JWT expired — try refresh before giving up
                            if reason.contains("Expired") || reason.contains("expired") {
                                if try_refresh_token(config) {
                                    eprintln!("[toki:sync] token refreshed after expiry, retrying");
                                    backoff.reset();
                                    last_refresh = Instant::now();
                                    continue; // retry auth immediately
                                }
                            }

                            sw.set("sync_status", "auth_failed");
                            sw.set("sync_last_error", &reason);
                            sw.set("sync_last_error_at", &now_epoch().to_string());
                            if reset_required {
                                eprintln!("[toki:sync] schema mismatch — clearing sync cursor");
                                let key = format!("sync_last_ts_{}", config.provider);
                                sw.set(&key, "0");
                            }
                        }
                        Err(e) => {
                            eprintln!("[toki:sync] auth error: {e}");
                            if try_refresh_token(config) {
                                eprintln!("[toki:sync] token refreshed, retrying auth");
                                backoff.reset();
                                last_refresh = Instant::now();
                            } else {
                                sw.set("sync_status", "token_expired");
                                sw.set("sync_last_error", &format!("{e}"));
                                sw.set("sync_last_error_at", &now_epoch().to_string());
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
                    sw.set("sync_status", "disconnected");
                    sw.set("sync_last_error", &format!("{e}"));
                    sw.set("sync_last_error_at", &now_epoch().to_string());
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

        if client.is_none() { continue; }

        // Sync cycle: upload everything until server ts == local ts.
        // After catching up, re-check dirty flag to avoid missing events
        // that arrived during the sync cycle.
        {
            let mut sync_error = false;
            loop {
                // Check stop/disable between batches
                if stop_rx.try_recv().is_ok() { return; }
                {
                    let enabled = sync_toggle.0.lock().unwrap();
                    if !*enabled { return; }
                }

                let c = client.as_mut().unwrap();
                match sync_new_events(c, db, &mut dict_cache, &config.provider, &mut sw) {
                    Ok(synced) => {
                        if synced > 0 {
                            eprintln!("[toki:sync] synced {synced} events");
                            sw.set("sync_last_success", &now_epoch().to_string());
                        } else {
                            // No more events in DB — but check if new ones arrived
                            // while we were syncing (race between flush and sync)
                            let still_dirty = {
                                let mut guard = flush_notify.0.lock().unwrap();
                                let d = *guard;
                                *guard = false;
                                d
                            };
                            if still_dirty {
                                // New data arrived during sync, go around again
                                continue;
                            }
                            break; // truly caught up
                        }
                    }
                    Err(e) => {
                        eprintln!("[toki:sync] sync error: {e}");
                        sync_error = true;
                        break;
                    }
                }
            }

            if sync_error {
                client = None;
                continue;
            }

            if needs_initial_sync {
                eprintln!("[toki:sync] catch-up complete — entering flush-driven mode");
                needs_initial_sync = false;
            }
            sw.set("sync_status", "connected");
            sw.set("sync_last_success", &now_epoch().to_string());
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
            if let Some(ref mut c) = client {
                match c.ping() {
                    Ok(()) => { last_ping = Instant::now(); }
                    Err(e) => {
                        eprintln!("[toki:sync] ping failed: {e}");
                        client = None;
                    }
                }
            }
        }

        // Windows sync: full recent set, throttled; field-wise server merge
        // makes the resend idempotent, so no cursor exists (a cursor on
        // window_end would permanently miss peak updates under a fixed key).
        // `client.is_some()`: the ping-failure path above nulls the client and
        // falls through here, where both passes would run and be thrown away.
        if client.is_some() && last_windows_sync.elapsed() >= WINDOWS_SYNC_INTERVAL {
            if windows_cap == WindowsCapability::Unsupported
                && Instant::now() >= next_cap_probe
            {
                windows_cap = WindowsCapability::Unknown;
            }
            if windows_cap == WindowsCapability::Unknown && Instant::now() >= next_cap_probe {
                if let Some(creds) = crate::sync::credentials::load() {
                    if !creds.http_url.is_empty() {
                        match probe_windows_capability(&creds.http_url, config.tls_insecure) {
                            Some(true) => {
                                windows_cap = WindowsCapability::Supported;
                                eprintln!("[toki:sync] server supports windows sync");
                            }
                            Some(false) => {
                                windows_cap = WindowsCapability::Unsupported;
                                next_cap_probe = Instant::now() + CAP_UNSUPPORTED_TTL;
                                eprintln!("[toki:sync] server predates windows sync (re-probe in 6h)");
                            }
                            None => {
                                // Transient (network/TLS/5xx): back off instead
                                // of re-probing on every flush wake. Logged once
                                // — a silent None left "window sync never
                                // started" with no diagnostic anywhere.
                                if !cap_probe_failure_logged {
                                    cap_probe_failure_logged = true;
                                    eprintln!(
                                        "[toki:sync] windows capability probe failed (retrying); \
                                         window sync is paused until it succeeds"
                                    );
                                }
                                next_cap_probe = Instant::now() + Duration::from_secs(60);
                            }
                        }
                    }
                }
            }
            if windows_cap == WindowsCapability::Supported {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                // The decision itself lives in `windows_sync_step` so it can be
                // tested without a socket; this arm only applies the outcome.
                let step = match client {
                    Some(ref mut c) => {
                        let (s, w) = windows_sync_step_gated(
                            &db,
                            &config.provider,
                            now_ms,
                            last_windows_fingerprint,
                            last_windows_writes,
                            c,
                        );
                        // Only latch the counter on an ACCEPTED upload: a
                        // rejection or a dropped link must leave the set
                        // looking dirty so the next cycle retries it.
                        if matches!(s.outcome, WindowsSyncOutcome::Sent { .. } | WindowsSyncOutcome::Skipped) {
                            last_windows_writes = Some(w);
                        }
                        s
                    }
                    None => WindowsSyncStep::skipped(),
                };
                if step.over_cap_dropped != last_over_cap_logged {
                    last_over_cap_logged = step.over_cap_dropped;
                    if step.over_cap_dropped > 0 {
                        eprintln!(
                            "[toki:sync] {} oldest windows exceed the per-sync cap; \
                             sending the newest {MAX_WINDOWS_PER_SYNC}",
                            step.over_cap_dropped
                        );
                    }
                }
                match step.outcome {
                    // Nothing stored since the last look: leave the throttle
                    // alone so the very next wake retries. That is the state a
                    // fresh daemon sits in while its backfill runs, and burning
                    // the interval there postponed the first upload by 5
                    // minutes for no reason. The write-counter gate makes the
                    // re-check a single atomic load.
                    WindowsSyncOutcome::Skipped if step.nothing_stored_yet => {}
                    WindowsSyncOutcome::Skipped => {
                        last_windows_sync = Instant::now();
                    }
                    WindowsSyncOutcome::Sent { count, fingerprint } => {
                        last_windows_sync = Instant::now();
                        last_windows_fingerprint = fingerprint;
                        eprintln!("[toki:sync] synced {count} window snapshots");
                    }
                    // A SyncErr keeps the connection; the fingerprint does NOT
                    // latch, so the set is retried next cycle.
                    WindowsSyncOutcome::Rejected(e) => {
                        eprintln!("[toki:sync] windows sync error: {e}");
                        // Surface in `toki settings sync status` — eprintln
                        // alone left rejections invisible.
                        sw.set("sync_last_error", &format!("windows: {e}"));
                        last_windows_sync = Instant::now();
                    }
                    // Transport is gone: reconnect path.
                    WindowsSyncOutcome::Disconnected(e) => {
                        eprintln!("[toki:sync] windows sync error: {e}");
                        sw.set("sync_last_error", &format!("windows: {e}"));
                        client = None;
                        continue;
                    }
                }
            }
        }

    }
}

/// Keeps a file descriptor open for /tmp/toki/sync_state.json.
/// Multiple sync threads (one per provider) share this file, so
/// flock is used to serialize read-modify-write operations.
/// If open fails, all writes silently no-op.
struct SyncStateWriter {
    file: Option<std::fs::File>,
    state: HashMap<String, String>,
}

impl SyncStateWriter {
    fn new() -> Self {
        let dir = std::path::Path::new("/tmp/toki");
        let _ = std::fs::create_dir_all(dir);
        let path = dir.join("sync_state.json");
        let file = std::fs::OpenOptions::new()
            .create(true).write(true).read(true)
            .open(&path).ok();
        Self { file, state: HashMap::new() }
    }

    fn set(&mut self, key: &str, value: &str) {
        // Acquire exclusive lock to prevent concurrent read-modify-write
        // from other provider threads. flock blocks until lock is available.
        if let Some(ref f) = self.file {
            use std::os::unix::io::AsRawFd;
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX); }
        }

        self.reload();
        self.state.insert(key.to_string(), value.to_string());
        self.flush();

        if let Some(ref f) = self.file {
            use std::os::unix::io::AsRawFd;
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN); }
        }
    }

    fn reload(&mut self) {
        let Some(ref mut f) = self.file else { return };
        use std::io::{Read, Seek};
        let _ = f.seek(std::io::SeekFrom::Start(0));
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_ok() {
            if let Ok(disk) = serde_json::from_str::<HashMap<String, serde_json::Value>>(&buf) {
                // Merge disk values — overwrite in-memory with disk state
                // so we pick up writes from other provider threads
                for (k, v) in disk {
                    if let Some(s) = v.as_str() {
                        self.state.insert(k, s.to_string());
                    }
                }
            }
        }
    }

    fn flush(&mut self) {
        use std::io::{Seek, Write};
        let Some(ref mut f) = self.file else { return };
        if let Ok(json) = serde_json::to_string_pretty(&self.state) {
            let _ = f.seek(std::io::SeekFrom::Start(0));
            let _ = f.set_len(0);
            let _ = f.write_all(json.as_bytes());
            let _ = f.flush();
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
    sw: &mut SyncStateWriter,
) -> Result<usize, String> {
    // Get server's last known ts
    let server_last_ts = client.get_last_ts(provider)
        .map_err(|e| format!("get_last_ts failed: {e}"))?;

    let cursor_key = format!("sync_last_ts_{provider}");
    let mut total_synced = 0;

    // Build the resume key: [ts_bytes].
    // First batch starts from server_last_ts + 1 (ms boundary).
    // Subsequent batches use the exact last key to avoid skipping same-ms events.
    let mut last_key: Vec<u8> = Vec::new();
    let mut use_after_key = false;

    loop {
        let events = if use_after_key && !last_key.is_empty() {
            db.query_events_after_key(&last_key, i64::MAX, BATCH_SIZE)
                .map_err(|e| format!("query_events_after_key failed: {e}"))?
        } else {
            db.query_events_range_limit(server_last_ts.saturating_add(1), i64::MAX, BATCH_SIZE)
                .map_err(|e| format!("query_events_range failed: {e}"))?
        };

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

        let items: Vec<SyncItem> = events.iter().map(|(ts_ms, msg_id, event)| {
            let usage_total = match provider {
                "codex" => event.input_tokens + event.output_tokens,
                _ => event.input_tokens + event.output_tokens
                    + event.cache_creation_input_tokens + event.cache_read_input_tokens,
            };

            SyncItem {
                ts_ms: *ts_ms,
                message_id: crate::db::Database::bare_msg_id(msg_id).to_string(),
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
                usage_total,
                ..Default::default()
            }
        }).collect();

        let token_columns: Vec<String> = match provider {
            "codex" => vec!["input".into(), "output".into(), "reasoning_output".into(), "cached_input".into()],
            _ => vec!["input".into(), "output".into(), "cache_create".into(), "cache_read".into()],
        };

        // Record the last event key for exact resume (avoids +1ms skip)
        if let Some((last_ts, last_msg, _)) = events.last() {
            let mut key = last_ts.to_be_bytes().to_vec();
            key.extend_from_slice(last_msg.as_bytes());
            last_key = key;
            use_after_key = true;
        }

        match client.sync_batch(items, dict, provider, token_columns) {
            Ok(ack_ts) => {
                total_synced += events.len();
                // Persist cursor locally, keyed per provider
                sw.set(&cursor_key, &ack_ts.to_string());
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

    // Always record cursor position (even if 0 events synced)
    // so status display shows all providers
    if total_synced == 0 && server_last_ts > 0 {
        sw.set(&cursor_key, &server_last_ts.to_string());
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

#[cfg(test)]
mod windows_sync_tests {
    use super::*;
    use crate::windows::{window_key, WindowKind, WindowSnapshotV1, REACHED_NONE};

    /// Records what was handed to it, so a step that builds a payload and never
    /// delivers it is observable. Window sync once shipped in exactly that
    /// state — the payload was built and dropped — and no test could see it.
    pub(crate) struct RecordingSender {
        pub(crate) sent: Vec<(String, usize)>,
        result: fn() -> std::io::Result<()>,
    }

    impl RecordingSender {
        pub(crate) fn ok() -> Self {
            RecordingSender { sent: Vec::new(), result: || Ok(()) }
        }
        fn rejecting() -> Self {
            RecordingSender {
                sent: Vec::new(),
                result: || Err(std::io::Error::other("2 of 5 window items rejected")),
            }
        }
        fn broken_pipe() -> Self {
            RecordingSender {
                sent: Vec::new(),
                result: || Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
            }
        }
    }

    impl WindowSender for RecordingSender {
        fn send_windows(
            &mut self,
            provider: &str,
            items: Vec<toki_sync_protocol::WireWindow>,
        ) -> std::io::Result<()> {
            self.sent.push((provider.to_string(), items.len()));
            (self.result)()
        }
    }

    pub(crate) fn temp_db() -> (crate::db::Database, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Database::open(&dir.path().join("t.fjall")).unwrap();
        (db, dir)
    }

    pub(crate) fn put(db: &crate::db::Database, now_ms: i64, i: u64, peak: u16) {
        // 10-minute spacing keeps even a full over-cap set (2000+ rows) inside
        // the 60-day sync horizon; at hourly spacing the horizon would exclude
        // most of them and the cap would never engage.
        let anchor = now_ms - (i as i64) * 600_000;
        let key = window_key(WindowKind::Session, i, 7, anchor);
        let snap = WindowSnapshotV1 {
            peak_pct_x100: peak,
            last_pct_x100: peak,
            observed_ts_ms: anchor - 1000,
            raw_resets_at_ms: anchor,
            first_seen_ms: anchor - 300_000,
            window_minutes: 300,
            finalized: true,
            maxed_out: false,
            limit_reached_kind: REACHED_NONE,
            time_to_100_ms: -1,
            active_ms: 1000,
            last_sample_gap_ms: 1000,
            sampled_active_fraction: 1000,
            n_samples: 3,
            limit_id: format!("l{i}"),
            plan: "max_5x".into(),
            account: "acct".into(),
        };
        db.upsert_window_merge(&key, &snap).unwrap();
    }

    #[test]
    fn changed_set_is_actually_delivered() {
        let (db, _d) = temp_db();
        let now = 1_800_000_000_000i64;
        for i in 0..3 {
            put(&db, now, i, 5000);
        }
        let mut sender = RecordingSender::ok();
        let step = windows_sync_step(&db, "codex", now, (0, 0), &mut sender);

        assert_eq!(sender.sent, vec![("codex".to_string(), 3)], "payload must reach the sender");
        match step.outcome {
            WindowsSyncOutcome::Sent { count, .. } => assert_eq!(count, 3),
            other => panic!("expected Sent, got {other:?}"),
        }
    }

    #[test]
    fn unchanged_set_skips_without_sending() {
        let (db, _d) = temp_db();
        let now = 1_800_000_000_000i64;
        put(&db, now, 0, 5000);

        let mut first = RecordingSender::ok();
        let fp = match windows_sync_step(&db, "codex", now, (0, 0), &mut first).outcome {
            WindowsSyncOutcome::Sent { fingerprint, .. } => fingerprint,
            other => panic!("expected Sent, got {other:?}"),
        };

        let mut second = RecordingSender::ok();
        let step = windows_sync_step(&db, "codex", now, fp, &mut second);
        assert_eq!(step.outcome, WindowsSyncOutcome::Skipped);
        assert!(second.sent.is_empty(), "an unchanged set must not re-upload");
    }

    #[test]
    fn a_changed_peak_re_uploads() {
        let (db, _d) = temp_db();
        let now = 1_800_000_000_000i64;
        put(&db, now, 0, 5000);
        let mut first = RecordingSender::ok();
        let fp = match windows_sync_step(&db, "codex", now, (0, 0), &mut first).outcome {
            WindowsSyncOutcome::Sent { fingerprint, .. } => fingerprint,
            other => panic!("expected Sent, got {other:?}"),
        };
        // Same key, higher peak — the fingerprint must notice.
        put(&db, now, 0, 9000);
        let mut second = RecordingSender::ok();
        match windows_sync_step(&db, "codex", now, fp, &mut second).outcome {
            WindowsSyncOutcome::Sent { count, .. } => assert_eq!(count, 1),
            other => panic!("expected Sent after a peak change, got {other:?}"),
        }
    }

    #[test]
    fn rejection_does_not_latch_the_fingerprint() {
        let (db, _d) = temp_db();
        let now = 1_800_000_000_000i64;
        put(&db, now, 0, 5000);
        let mut sender = RecordingSender::rejecting();
        let step = windows_sync_step(&db, "codex", now, (0, 0), &mut sender);
        // Rejected carries no fingerprint, so the caller cannot latch one and
        // the same set is retried next cycle.
        assert!(matches!(step.outcome, WindowsSyncOutcome::Rejected(_)));
    }

    #[test]
    fn transport_failure_is_distinguished_from_rejection() {
        let (db, _d) = temp_db();
        let now = 1_800_000_000_000i64;
        put(&db, now, 0, 5000);
        let mut sender = RecordingSender::broken_pipe();
        let step = windows_sync_step(&db, "codex", now, (0, 0), &mut sender);
        assert!(
            matches!(step.outcome, WindowsSyncOutcome::Disconnected(_)),
            "an IO error must trigger reconnect, not a quiet retry"
        );
    }

    #[test]
    fn empty_set_sends_nothing() {
        let (db, _d) = temp_db();
        let mut sender = RecordingSender::ok();
        let step = windows_sync_step(&db, "codex", 1_800_000_000_000, (0, 0), &mut sender);
        assert_eq!(step.outcome, WindowsSyncOutcome::Skipped);
        assert!(sender.sent.is_empty());
    }

    #[test]
    fn over_cap_sends_exactly_the_cap_and_fingerprints_that_same_set() {
        let (db, _d) = temp_db();
        let now = 1_800_000_000_000i64;
        let total = MAX_WINDOWS_PER_SYNC + 25;
        for i in 0..total as u64 {
            put(&db, now, i, 5000);
        }
        let mut sender = RecordingSender::ok();
        let step = windows_sync_step(&db, "codex", now, (0, 0), &mut sender);

        assert_eq!(step.over_cap_dropped, 25);
        assert_eq!(sender.sent[0].1, MAX_WINDOWS_PER_SYNC, "must send exactly the cap");
        let fp = match step.outcome {
            WindowsSyncOutcome::Sent { count, fingerprint } => {
                assert_eq!(count, MAX_WINDOWS_PER_SYNC);
                fingerprint
            }
            other => panic!("expected Sent, got {other:?}"),
        };
        // The fingerprint must describe the SENT set, not the whole set: if it
        // described rows the server never stored, latching it would drop them
        // permanently and silently.
        assert_eq!(fp.0, MAX_WINDOWS_PER_SYNC);

        // And it must be stable — a second pass over the unchanged DB skips.
        let mut again = RecordingSender::ok();
        assert_eq!(
            windows_sync_step(&db, "codex", now, fp, &mut again).outcome,
            WindowsSyncOutcome::Skipped
        );
    }

    #[test]
    fn over_cap_selection_keeps_the_newest_windows() {
        let (db, _d) = temp_db();
        let now = 1_800_000_000_000i64;
        for i in 0..(MAX_WINDOWS_PER_SYNC + 5) as u64 {
            put(&db, now, i, 5000);
        }
        // Pass 2 reproduces pass 1's selection, so the delivered rows are the
        // newest by anchor: index 0 is newest, index N-1 oldest.
        let items = {
            let mut captured: Vec<toki_sync_protocol::WireWindow> = Vec::new();
            struct Capture<'a>(&'a mut Vec<toki_sync_protocol::WireWindow>);
            impl WindowSender for Capture<'_> {
                fn send_windows(
                    &mut self,
                    _p: &str,
                    items: Vec<toki_sync_protocol::WireWindow>,
                ) -> std::io::Result<()> {
                    *self.0 = items;
                    Ok(())
                }
            }
            let mut c = Capture(&mut captured);
            windows_sync_step(&db, "codex", now, (0, 0), &mut c);
            captured
        };
        assert_eq!(items.len(), MAX_WINDOWS_PER_SYNC);
        let oldest_sent = items.iter().map(|w| w.window_end_ms).min().unwrap();
        let dropped_anchor = now - ((MAX_WINDOWS_PER_SYNC + 4) as i64) * 600_000;
        assert!(
            oldest_sent > dropped_anchor,
            "the excluded rows must be the OLDEST, not an arbitrary subset"
        );
    }


    /// A just-started daemon runs its first window sync BEFORE the backfill has
    /// written anything. Burning the 5-minute interval on that empty look
    /// postponed the first upload by a full interval; the write-counter gate
    /// makes re-checking a single atomic load, so the throttle must be left
    /// alone until there is actually something to look at.
    #[test]
    fn empty_store_does_not_burn_the_interval() {
        let (db, _d) = temp_db();
        let now = 1_800_000_000_000i64;
        let mut s0 = RecordingSender::ok();

        // Nothing stored, nothing ever uploaded: this is the "look once" case,
        // so it is NOT flagged as gated.
        let (first, w0) = windows_sync_step_gated(&db, "codex", now, (0, 0), None, &mut s0);
        assert_eq!(first.outcome, WindowsSyncOutcome::Skipped);
        assert!(
            first.nothing_stored_yet,
            "an empty horizon is the backfill-still-running case, not a real look"
        );

        // Still nothing stored: the gate short-circuits and the caller is told
        // not to spend its interval.
        let (second, _) = windows_sync_step_gated(&db, "codex", now, (0, 0), Some(w0), &mut s0);
        assert_eq!(second.outcome, WindowsSyncOutcome::Skipped);
        assert!(second.nothing_stored_yet, "an untouched store must not burn the interval");

        // Backfill lands -> the very next attempt uploads.
        put(&db, now, 0, 5000);
        let (third, _) = windows_sync_step_gated(&db, "codex", now, (0, 0), Some(w0), &mut s0);
        assert!(!third.nothing_stored_yet);
        assert!(matches!(third.outcome, WindowsSyncOutcome::Sent { .. }));
    }

    #[test]
    fn unchanged_store_skips_the_scan_entirely() {
        let (db, _d) = temp_db();
        let now = 1_800_000_000_000i64;
        put(&db, now, 0, 5000);

        let mut s1 = RecordingSender::ok();
        let (step1, w1) = windows_sync_step_gated(&db, "codex", now, (0, 0), None, &mut s1);
        assert!(matches!(step1.outcome, WindowsSyncOutcome::Sent { .. }));

        // Same counter -> skipped without touching the keyspace.
        let mut s2 = RecordingSender::ok();
        let (step2, w2) = windows_sync_step_gated(&db, "codex", now, (0, 0), Some(w1), &mut s2);
        assert_eq!(step2.outcome, WindowsSyncOutcome::Skipped);
        assert_eq!(w2, w1);
        assert!(s2.sent.is_empty());

        // A new write moves the counter, so the next cycle scans and uploads.
        put(&db, now, 1, 6000);
        let mut s3 = RecordingSender::ok();
        let (step3, w3) = windows_sync_step_gated(&db, "codex", now, (0, 0), Some(w1), &mut s3);
        assert!(w3 > w1, "a write must move the counter");
        assert!(matches!(step3.outcome, WindowsSyncOutcome::Sent { .. }));
        assert_eq!(s3.sent[0].1, 2);
    }
}
