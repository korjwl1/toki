use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::BroadcastSink;
use crate::common::schema::ProviderSchema;
use crate::db::Database;

/// Maximum number of concurrent connection workers (handshake + report). One
/// cap bounds both the short-lived handshake threads (a client that connects and
/// never sends still holds a worker until its read times out) and the report
/// workers, and the permit is acquired BEFORE any blocking read so an idle-client
/// flood can never exhaust threads.
const MAX_CONN_WORKERS: usize = 16;

/// RAII permit for a connection worker slot. `try_acquire` bumps the shared
/// counter without exceeding `MAX_CONN_WORKERS` (atomic compare-exchange, no
/// check-then-increment race); `Drop` releases it, covering every exit path
/// including thread spawn failure (the closure that owns the permit is dropped).
struct ConnPermit {
    count: Arc<AtomicUsize>,
}

impl ConnPermit {
    fn try_acquire(count: &Arc<AtomicUsize>) -> Option<ConnPermit> {
        let mut cur = count.load(Ordering::Acquire);
        loop {
            if cur >= MAX_CONN_WORKERS {
                return None;
            }
            match count.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(ConnPermit { count: Arc::clone(count) }),
                Err(actual) => cur = actual,
            }
        }
    }
}

impl Drop for ConnPermit {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Run the UDS listener in a loop, accepting new clients.
/// Clients send a command on the first line: TRACE or REPORT.
/// Blocks until `stop_rx` fires or the listener is dropped.
pub fn run_listener(
    sock_path: &Path,
    broadcast: Arc<BroadcastSink>,
    dbs: Vec<(String, Arc<Database>)>,
    stop_rx: crossbeam_channel::Receiver<()>,
    windows_hub: Option<Arc<crate::claude_poll::PollerHub>>,
) {
    // Clean up stale socket
    if sock_path.exists() {
        let _ = std::fs::remove_file(sock_path);
    }

    // Owner-only from the instant it exists. bind() applies the process
    // umask, so a post-bind chmod alone leaves a window in which another local
    // user can connect (and such a connection survives the chmod). Closing it
    // by flipping the process-global umask would be worse: this daemon is
    // multi-threaded by now, and any DIRECTORY another thread creates in that
    // window would lose its x bit. Instead make the PARENT directory
    // owner-only — an unreachable path is unreachable regardless of the
    // socket's own mode.
    // ONLY when the socket lives in toki's own config dir: daemon_sock is a
    // user-settable path, and chmod-ing an arbitrary parent would silently
    // lock down $HOME (or attempt /tmp) — never mutate directories we do not
    // own. The 0600 socket mode plus the peer-uid check below carry the
    // security property; this is defense in depth.
    #[cfg(unix)]
    if let Some(parent) = sock_path.parent() {
        use std::os::unix::fs::PermissionsExt;
        let own_dir = crate::config::settings_file_path()
            .parent()
            .map(|p| p == parent)
            .unwrap_or(false);
        if own_dir {
            if let Err(e) = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)) {
                eprintln!("[toki:daemon] could not restrict {}: {}", parent.display(), e);
            }
        }
    }
    let bind_result = UnixListener::bind(sock_path);
    let listener = match bind_result {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[toki:daemon] Failed to bind {}: {}", sock_path.display(), e);
            return;
        }
    };

    // Belt and braces, and FATAL on failure: everything this socket serves —
    // usage data, auth state, forced provider-API refreshes — belongs to this
    // user's own tools. Serving it on a world/group-connectable socket is
    // worse than not serving it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o600)) {
            eprintln!("[toki:daemon] socket chmod failed ({}), refusing to listen", e);
            let _ = std::fs::remove_file(sock_path);
            return;
        }
    }

    // Non-blocking accept with 100ms sleep between attempts.
    // SO_RCVTIMEO does NOT affect accept() on macOS, so we use non-blocking mode.
    listener.set_nonblocking(true).ok();

    eprintln!("[toki:daemon] Listening on {}", sock_path.display());

    let conn_count = Arc::new(AtomicUsize::new(0));

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        match listener.accept() {
            Ok((stream, _addr)) => {
                stream.set_nonblocking(false).ok();
                // Defense in depth: reject any peer that is not this daemon's
                // own uid, whatever the socket mode ended up being (a
                // connection established during a mode gap, or an
                // administrator loosening it later).
                #[cfg(unix)]
                {
                    if !peer_is_owner(&stream) {
                        eprintln!("[toki:daemon] rejecting connection from another uid");
                        continue;
                    }
                }
                // Acquire a worker permit BEFORE spawning: at capacity we reject
                // inline (no thread) so a flood of idle clients can never exhaust
                // threads. The permit is moved into the worker and dropped when it
                // returns — releasing the slot on every exit path.
                let permit = match ConnPermit::try_acquire(&conn_count) {
                    Some(p) => p,
                    None => {
                        eprintln!("[toki:daemon] Too many concurrent connections, rejecting");
                        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(1)));
                        let busy = serde_json::json!({
                            "ok": false,
                            "error": "server busy, too many concurrent connections"
                        });
                        let _ = writeln!(&stream, "{}", serde_json::to_string(&busy).unwrap_or_default());
                        continue;
                    }
                };
                // Handle each connection on its own thread. The initial command
                // read (handle_connection) can block up to 5s, so doing it inline
                // would let one idle client stall every other pending connection.
                let broadcast = Arc::clone(&broadcast);
                let dbs = dbs.clone();
                let hub = windows_hub.clone();
                let spawned = std::thread::Builder::new()
                    .name("toki-conn".to_string())
                    .spawn(move || {
                        // `_permit` releases the worker slot when this returns; on
                        // spawn failure below the closure (and permit) is dropped.
                        let _permit = permit;
                        handle_connection(stream, &broadcast, &dbs, hub.as_ref());
                    });
                if let Err(e) = spawned {
                    eprintln!("[toki:daemon] Failed to spawn connection thread: {}", e);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("[toki:daemon] Accept error: {}", e);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }

    // Cleanup socket file on exit
    let _ = std::fs::remove_file(sock_path);
    eprintln!("[toki:daemon] Listener stopped");
}

/// True when the connected peer runs as this process's uid. `getpeereid` is
/// the documented BSD/macOS API for local peer credentials; on other unixes
/// the SO_PEERCRED equivalents differ, so we fail open there rather than
/// break the socket (mode 0600 remains the primary control).
#[cfg(unix)]
fn peer_is_owner(stream: &UnixStream) -> bool {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
    {
        use std::os::unix::io::AsRawFd;
        let mut euid: libc::uid_t = 0;
        let mut egid: libc::gid_t = 0;
        let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut euid, &mut egid) };
        if rc != 0 {
            return true; // cannot determine — mode 0600 already gates this
        }
        euid == unsafe { libc::geteuid() }
    }
    // Linux (and anything else with SO_PEERCRED). Previously this arm returned
    // an unconditional `true`, so on Linux the UID check did not exist: with a
    // custom `daemon_sock` under a world-writable parent, a connection that
    // slipped in between bind() and chmod(0600) stayed valid afterwards and
    // could issue REPORT / WINDOWS / forced-refresh commands.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut libc::ucred as *mut libc::c_void,
                &mut len,
            )
        };
        // Fail CLOSED: unlike the BSD arm (whose default socket lives inside a
        // 0700 directory), the case this guard exists for is a socket whose
        // parent directory is public, where mode alone is not a gate.
        if rc != 0 || len as usize != std::mem::size_of::<libc::ucred>() {
            return false;
        }
        cred.uid == unsafe { libc::geteuid() }
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "linux"
    )))]
    {
        let _ = stream;
        true
    }
}

/// Read one '\n'-terminated line with a hard byte cap. `read_line` alone
/// grows its String until the peer sends a newline — a hostile or broken
/// local client could feed an endless unterminated stream and balloon the
/// daemon's memory. Returns None on EOF, error, or cap overflow.
fn read_line_limited(
    reader: &mut BufReader<&UnixStream>,
    max_bytes: u64,
) -> Option<String> {
    use std::io::Read;
    let mut line = String::new();
    let mut limited = reader.take(max_bytes);
    match limited.read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => {
            // A cap-sized read without a trailing newline means truncation.
            if !line.ends_with('\n') && line.len() as u64 >= max_bytes {
                None
            } else {
                Some(line)
            }
        }
        Err(_) => None,
    }
}

/// Read the first line (command) and dispatch to the appropriate handler.
fn handle_connection(
    stream: UnixStream,
    broadcast: &Arc<BroadcastSink>,
    dbs: &[(String, Arc<Database>)],
    windows_hub: Option<&Arc<crate::claude_poll::PollerHub>>,
) {
    // 5 second timeout to read the command line
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();

    let mut reader = BufReader::new(&stream);
    let Some(command_line) = read_line_limited(&mut reader, 4 * 1024) else {
        return;
    };
    let command = command_line.trim();

    match command {
        "TRACE" => {
            // TRACE hands the stream to the broadcaster and releases its
            // worker permit immediately (two long-lived threads per client),
            // so the 16-worker cap does NOT bound it. Cap subscribers
            // explicitly or N connections become 2N threads + N fds.
            stream.set_read_timeout(None).ok();
            let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
            // Keep a handle for the reject path so the client gets the same
            // busy JSON the connection cap sends, not a bare EOF.
            let reject_handle = stream.try_clone().ok();
            // add_client admits atomically (CAS) and returns false at the cap —
            // a count-then-add check could admit past it under concurrent
            // accepts.
            if !broadcast.add_client(stream) {
                eprintln!("[toki:daemon] Too many trace clients, rejecting");
                if let Some(h) = reject_handle {
                    let busy = serde_json::json!({
                        "ok": false,
                        "error": "too many trace clients"
                    });
                    let _ = writeln!(&h, "{}", serde_json::to_string(&busy).unwrap_or_default());
                }
                return;
            }
            eprintln!("[toki:daemon] Trace client connected ({} total)", broadcast.client_count());
        }
        "REPORT" => {
            // Read the next line as JSON payload, then serve inline. This worker
            // already holds a ConnPermit (acquired in the accept loop), so the
            // report runs under the single connection-worker cap — no separate
            // thread and no racy per-report counter.
            stream.set_read_timeout(Some(std::time::Duration::from_secs(60))).ok();
            // Bound the RESPONSE too: a peer that stops reading would
            // otherwise pin this connection worker forever (responses exceed
            // the socket buffer).
            stream.set_write_timeout(Some(std::time::Duration::from_secs(30))).ok();
            let Some(payload_line) = read_line_limited(&mut reader, 1024 * 1024) else {
                return;
            };
            handle_report_client(stream, &payload_line, dbs);
        }
        "WINDOWS" => {
            // Optional JSON request line: {"max_age_ms": 0} forces a bounded
            // revalidation; absent/large max_age serves the cached state.
            // The payload line is OPTIONAL, so this read is expected to time
            // out for clients that omit it — keep it short or every such
            // request would pin a connection worker for 5s.
            stream.set_read_timeout(Some(std::time::Duration::from_millis(300))).ok();
            stream.set_write_timeout(Some(std::time::Duration::from_secs(10))).ok();
            let payload_line = read_line_limited(&mut reader, 4 * 1024).unwrap_or_default();
            handle_windows_client(stream, &payload_line, dbs, windows_hub);
        }
        _ => {
            let error_resp = serde_json::json!({
                "ok": false,
                "error": format!("unknown command: {}", command)
            });
            let _ = writeln!(&stream, "{}", serde_json::to_string(&error_resp).unwrap_or_default());
        }
    }
}

/// Serve the latest rate-limit window state: recent window rows from each
/// provider DB plus per-provider auth status. `max_age_ms=0` joins the
/// poller's single-flight refresh with a bounded wait (2s) — the response is
/// flagged `refreshing:true` when it returns stale data instead of blocking.
fn handle_windows_client(
    stream: UnixStream,
    payload_line: &str,
    dbs: &[(String, Arc<Database>)],
    windows_hub: Option<&Arc<crate::claude_poll::PollerHub>>,
) {
    #[derive(serde::Deserialize, Default)]
    struct WindowsRequest {
        #[serde(default)]
        max_age_ms: Option<i64>,
    }
    let req: WindowsRequest = serde_json::from_str(payload_line.trim()).unwrap_or_default();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let Some(hub) = windows_hub else {
        let resp = serde_json::json!({
            "ok": false,
            "error": "window tracking is disabled (settings set window_tracking true)"
        });
        let _ = writeln!(&stream, "{}", serde_json::to_string(&resp).unwrap_or_default());
        return;
    };

    // Claude live-state freshness: join the poller when the caller asked for
    // fresher data than we have.
    let mut refreshing = false;
    let mut claude_state = hub.state_snapshot();
    if hub.polling_enabled() && hub.poller_running() {
        if let Some(max_age) = req.max_age_ms {
            if now_ms - claude_state.last_success_ms > max_age.max(0) {
                let (st, timed_out) =
                    hub.request_refresh(std::time::Duration::from_secs(2));
                claude_state = st;
                refreshing = timed_out;
            }
        }
    }

    // Recent window rows (8 days) from each provider DB — a handful of rows.
    const ROW_HORIZON_MS: i64 = 8 * 86_400_000;
    let mut providers = serde_json::Map::new();
    for (name, db) in dbs {
        let mut rows: Vec<crate::windows::WindowRow> = Vec::new();
        let read_err = db
            .for_each_window_in(now_ms - ROW_HORIZON_MS, i64::MAX, |key, snap| {
                rows.push(crate::windows::WindowRow::from_stored(key, &snap, now_ms));
            })
            .err()
            .map(|e| e.to_string());
        rows.sort_by_key(|r| r.window_end_ms);

        let auth = match name.as_str() {
            "claude_code" => {
                // The poller's classification is authoritative only once it
                // has actually attempted a poll. A daemon (re)started on an
                // idle machine never polls (activity gate) and its default
                // state is Missing — serving that told the monitor the user
                // was logged out, forever. Fall back to the 30s-cached
                // keychain classification until the first attempt.
                // Authoritative only while FRESH. On an idle machine the
                // activity gate means no further polls, so a logout after the
                // last successful poll would keep publishing "ok" forever —
                // the mirror of the pre-first-poll "missing" bug. Past the
                // active horizon, defer to the (30s-cached) keychain read.
                let published_fresh = claude_state.last_poll_ms > 0
                    && now_ms - claude_state.last_poll_ms < 30 * 60_000;
                // poller_running too: if the thread died, its last
                // classification must not be served as authoritative for the
                // next 30 minutes.
                if hub.polling_enabled() && hub.poller_running() && published_fresh {
                    claude_state.auth_status.label().to_string()
                } else {
                    hub.claude_auth_cached().label().to_string()
                }
            }
            "codex" => hub.codex_auth_cached().label().to_string(),
            _ => "unknown".to_string(),
        };

        // Current account scope so a client can ignore rows still open under
        // a previous login (they linger until their own reset).
        let current_account = match name.as_str() {
            // Cached: this runs at widget-poll rate and account_scope() is a
            // full read+parse of auth.json (parity with the two auth caches).
            "codex" => Some(hub.codex_account_cached()),
            // Published by the poller after profile resolution — without it
            // the monitor cannot tell a superseded login's rows from the
            // current one's for Claude either. A daemon restarted on an idle
            // machine never polls (activity gate), so fall back to the newest
            // stored row's account (already in hand — no extra I/O).
            "claude_code" if !claude_state.account.is_empty() => {
                Some(claude_state.account.clone())
            }
            "claude_code" => rows
                .iter()
                .max_by_key(|r| r.observed_ts_ms)
                .map(|r| r.account.clone())
                .filter(|a| !a.is_empty()),
            _ => None,
        };
        let mut entry = serde_json::json!({
            "windows": serde_json::to_value(&rows).unwrap_or(serde_json::Value::Array(vec![])),
            "auth_status": auth,
            "current_account": current_account,
            // How this provider's windows are obtained. Response-only: it is a
            // property of the provider, not of a stored row, so it costs no
            // schema change. Claude has no passive source (window state exists
            // only behind the API); Codex has no active one (it is extracted
            // from rollout files the daemon already watches, zero API calls),
            // and the difference explains why freshness behaves differently
            // between them.
            "source": match name.as_str() {
                "claude_code" => "active-poll",
                "codex" => "passive-extract",
                _ => "unknown",
            },
        });
        if let Some(err) = read_err {
            // Distinguish "storage failed" from "no windows yet": the client
            // otherwise silently treats a broken DB as an empty one.
            entry["error"] = serde_json::json!(err);
        }
        if name == "claude_code" {
            entry["extra_usage_enabled"] = serde_json::json!(claude_state.extra_usage_enabled);
            entry["last_success_ms"] = serde_json::json!(claude_state.last_success_ms);
            entry["last_poll_ms"] = serde_json::json!(claude_state.last_poll_ms);
            entry["plan"] = serde_json::json!(claude_state.plan);
            entry["polling_enabled"] = serde_json::json!(hub.polling_enabled());
        }
        providers.insert(name.clone(), entry);
    }

    let resp = serde_json::json!({
        "ok": true,
        "schema": 1,
        "now_ms": now_ms,
        "refreshing": refreshing,
        "providers": providers,
    });
    let _ = writeln!(&stream, "{}", serde_json::to_string(&resp).unwrap_or_default());
}

/// Handle a report query: parse request, execute query, send response.
fn handle_report_client(mut stream: UnixStream, request_line: &str, dbs: &[(String, Arc<Database>)]) {
    stream.set_read_timeout(None).ok();

    let response = match execute_report_request(request_line, dbs) {
        Ok((data, meta)) => serde_json::json!({ "ok": true, "data": data, "meta": meta }),
        Err(e) => serde_json::json!({ "ok": false, "error": e }),
    };

    let line = serde_json::to_string(&response).unwrap_or_default();
    let _ = writeln!(stream, "{}", line);
    let _ = stream.flush();
}

/// Parse and execute a report request against all provider DBs, merging results.
/// Returns (data, meta) where meta contains query metadata for the information block.
fn execute_report_request(
    request_line: &str,
    dbs: &[(String, Arc<Database>)],
) -> Result<(serde_json::Value, serde_json::Value), String> {
    let req: ReportRequest =
        serde_json::from_str(request_line).map_err(|e| format!("invalid request: {}", e))?;

    let tz: Option<chrono_tz::Tz> = req
        .tz
        .as_deref()
        .map(|s| s.parse().map_err(|_| format!("invalid timezone: {}", s)))
        .transpose()?;

    // start_of_week only affects weekly buckets. Absent (older client) → Monday,
    // matching the client/server default so bucket edges stay consistent. A
    // present-but-invalid value is a client bug, not a Monday request: reject it
    // instead of silently shifting weekly bucket edges. Case-insensitive.
    let start_of_week = match req.start_of_week.as_deref() {
        None => chrono::Weekday::Mon,
        Some(s) => crate::config::parse_weekday(&s.to_lowercase())
            .ok_or_else(|| format!("invalid start_of_week: {}", s))?,
    };

    let mut parsed =
        crate::query_parser::parse(&req.query).map_err(|e| format!("query parse error: {}", e))?;

    // Resolve time range from request start/end fields.
    let since_ms = req.start.as_deref()
        .map(|s| crate::query::parse_range_time(s, false, tz)
            .map(|d| d.and_utc().timestamp_millis()))
        .transpose()?
        .unwrap_or(0);
    let until_ms = req.end.as_deref()
        .map(|s| crate::query::parse_range_time(s, true, tz)
            .map(|d| d.and_utc().timestamp_millis()))
        .transpose()?
        .unwrap_or(i64::MAX);

    // Strip "provider" from group_by (handled at DB routing level)
    parsed.group_by.retain(|k| k != "provider");

    // Take provider filter out of parsed query (handled at DB routing level)
    let provider_filter = parsed.provider.take();

    // Select which DBs to query based on provider filter
    let target_dbs: Vec<(&str, &Arc<Database>)> = if let Some(ref pf) = provider_filter {
        dbs.iter()
            .filter(|(name, _)| name == pf)
            .map(|(name, db)| (name.as_str(), db))
            .collect()
    } else {
        dbs.iter().map(|(name, db)| (name.as_str(), db)).collect()
    };

    if target_dbs.is_empty() {
        let meta = serde_json::json!({
            "since": req.start,
            "until": req.end,
            "data_since": serde_json::Value::Null,
            "data_until": serde_json::Value::Null,
        });
        return Ok((serde_json::Value::Array(Vec::new()), meta));
    }

    let mut all_results: Vec<serde_json::Value> = Vec::new();

    for (provider_name, db) in &target_dbs {
        let collector = CollectorSink::new();
        crate::query::execute_parsed_query(db, &parsed, tz, start_of_week, None, &collector, since_ms, until_ms)?;

        let mut provider_results = collector.take();
        for item in &mut provider_results {
            item["schema"] = serde_json::json!(provider_name);
        }
        all_results.extend(provider_results);
    }

    // Get actual data range from all queried DBs (O(1) per DB — B-tree first/last)
    let mut global_min: Option<i64> = None;
    let mut global_max: Option<i64> = None;
    for (_, db) in &target_dbs {
        if let Some((min_ts, max_ts)) = db.data_range() {
            global_min = Some(global_min.map_or(min_ts, |v: i64| v.min(min_ts)));
            global_max = Some(global_max.map_or(max_ts, |v: i64| v.max(max_ts)));
        }
    }

    let meta = serde_json::json!({
        "since": req.start,
        "until": req.end,
        "data_since": global_min,
        "data_until": global_max,
    });

    Ok((serde_json::Value::Array(all_results), meta))
}

/// Request payload from report client.
#[derive(serde::Deserialize)]
struct ReportRequest {
    query: String,
    #[serde(default)]
    tz: Option<String>,
    /// Time range start (inclusive): YYYYMMDD or YYYYMMDDhhmmss
    #[serde(default)]
    start: Option<String>,
    /// Time range end (inclusive): YYYYMMDD or YYYYMMDDhhmmss
    #[serde(default)]
    end: Option<String>,
    /// Week-start override for weekly buckets (e.g. "mon"). Absent → Monday.
    #[serde(default)]
    start_of_week: Option<String>,
}

/// Sink that collects output as JSON values instead of printing.
struct CollectorSink {
    collected: std::sync::Mutex<Vec<serde_json::Value>>,
}

impl CollectorSink {
    fn new() -> Self {
        CollectorSink {
            collected: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn take(self) -> Vec<serde_json::Value> {
        self.collected
            .into_inner()
            .unwrap_or_else(|e| e.into_inner())
    }
}

impl crate::sink::Sink for CollectorSink {
    fn emit_windows(&self, rows: &[crate::windows::WindowRow]) {
        let data = serde_json::to_value(rows).unwrap_or(serde_json::Value::Array(vec![]));
        self.collected.lock().unwrap().push(serde_json::json!({
            "type": "windows",
            "data": data,
        }));
    }

    fn emit_summary(
        &self,
        summaries: &std::collections::HashMap<String, crate::common::types::ModelUsageSummary>,
        pricing: Option<&crate::pricing::PricingTable>,
        _schema: Option<&dyn ProviderSchema>,
    ) {
        let json = crate::sink::json::summaries_to_json(summaries, pricing, None);
        self.collected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(json);
    }

    fn emit_grouped(
        &self,
        grouped: &std::collections::HashMap<
            String,
            std::collections::HashMap<String, crate::common::types::ModelUsageSummary>,
        >,
        type_name: &str,
        pricing: Option<&crate::pricing::PricingTable>,
        _schema: Option<&dyn ProviderSchema>,
    ) {
        let json = crate::sink::json::grouped_to_json(grouped, type_name, pricing, None);
        self.collected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(json);
    }

    fn emit_event(
        &self,
        event: &crate::common::types::UsageEventWithTs,
        pricing: Option<&crate::pricing::PricingTable>,
        _schema: Option<&dyn crate::common::schema::ProviderSchema>,
    ) {
        let json = crate::sink::json::event_to_json(event, pricing, _schema);
        self.collected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(json);
    }

    fn emit_list(&self, items: &[String], type_name: &str) {
        self.collected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(serde_json::json!({ "type": type_name, "items": items }));
    }

    fn emit_events_batch(
        &self,
        events: &[crate::common::types::RawEvent],
        pricing: Option<&crate::pricing::PricingTable>,
        _schema: Option<&dyn ProviderSchema>,
    ) {
        let json = crate::sink::json::events_batch_to_json(events, pricing, None);
        self.collected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conn_permit_caps_and_releases() {
        let count = Arc::new(AtomicUsize::new(0));
        // Acquire the full cap.
        let mut held: Vec<ConnPermit> = (0..MAX_CONN_WORKERS)
            .map(|_| ConnPermit::try_acquire(&count).expect("under cap must acquire"))
            .collect();
        assert_eq!(count.load(Ordering::SeqCst), MAX_CONN_WORKERS);
        // At cap: further acquisition is refused (no check-then-increment slip).
        assert!(ConnPermit::try_acquire(&count).is_none(), "must refuse beyond cap");
        assert_eq!(count.load(Ordering::SeqCst), MAX_CONN_WORKERS, "refusal must not bump the counter");
        // Releasing one frees exactly one slot.
        held.pop();
        assert_eq!(count.load(Ordering::SeqCst), MAX_CONN_WORKERS - 1);
        assert!(ConnPermit::try_acquire(&count).is_some(), "freed slot must be reusable");
        drop(held);
    }

    #[test]
    fn conn_permit_concurrent_never_exceeds_cap() {
        // Hammer try_acquire from many threads; the live count must never exceed
        // the cap, proving the compare-exchange loop has no check-then-increment race.
        let count = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let count = Arc::clone(&count);
            let max_seen = Arc::clone(&max_seen);
            handles.push(std::thread::spawn(move || {
                for _ in 0..2000 {
                    if let Some(p) = ConnPermit::try_acquire(&count) {
                        let live = count.load(Ordering::SeqCst);
                        max_seen.fetch_max(live, Ordering::SeqCst);
                        assert!(live <= MAX_CONN_WORKERS);
                        drop(p);
                    }
                }
            }));
        }
        for h in handles { h.join().unwrap(); }
        assert!(max_seen.load(Ordering::SeqCst) <= MAX_CONN_WORKERS);
        assert_eq!(count.load(Ordering::SeqCst), 0, "all permits released");
    }

    // ── start_of_week protocol validation (finding #8) ───────────────────────
    // Empty `dbs` makes execute_report_request return early once the request is
    // accepted (no provider to route to), so these exercise only the field's
    // validation: absent/valid/mixed-case accepted, invalid rejected.

    #[test]
    fn report_request_start_of_week_absent_defaults_monday() {
        let r = execute_report_request(r#"{"query":"usage[1w]"}"#, &[]);
        assert!(r.is_ok(), "absent start_of_week must be accepted (older clients)");
    }

    #[test]
    fn report_request_start_of_week_valid_accepted() {
        let r = execute_report_request(r#"{"query":"usage[1w]","start_of_week":"sun"}"#, &[]);
        assert!(r.is_ok(), "valid start_of_week must be accepted");
    }

    #[test]
    fn report_request_start_of_week_mixed_case_accepted() {
        let r = execute_report_request(r#"{"query":"usage[1w]","start_of_week":"Sun"}"#, &[]);
        assert!(r.is_ok(), "mixed-case start_of_week must be accepted");
    }

    #[test]
    fn report_request_start_of_week_invalid_rejected() {
        let r = execute_report_request(r#"{"query":"usage[1w]","start_of_week":"funday"}"#, &[]);
        assert!(r.is_err(), "invalid start_of_week must be a protocol error, not a silent Monday");
        assert!(r.unwrap_err().contains("start_of_week"));
    }
}
