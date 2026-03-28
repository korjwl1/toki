use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::BroadcastSink;
use crate::common::schema::ProviderSchema;
use crate::db::Database;

/// Maximum number of concurrent report handler threads.
const MAX_REPORT_THREADS: usize = 8;

/// Run the UDS listener in a loop, accepting new clients.
/// Clients send a command on the first line: TRACE or REPORT.
/// Blocks until `stop_rx` fires or the listener is dropped.
pub fn run_listener(
    sock_path: &Path,
    broadcast: Arc<BroadcastSink>,
    dbs: Vec<(String, Arc<Database>)>,
    stop_rx: crossbeam_channel::Receiver<()>,
) {
    // Clean up stale socket
    if sock_path.exists() {
        let _ = std::fs::remove_file(sock_path);
    }

    let listener = match UnixListener::bind(sock_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[toki:daemon] Failed to bind {}: {}", sock_path.display(), e);
            return;
        }
    };

    // Non-blocking accept with 100ms sleep between attempts.
    // SO_RCVTIMEO does NOT affect accept() on macOS, so we use non-blocking mode.
    listener.set_nonblocking(true).ok();

    eprintln!("[toki:daemon] Listening on {}", sock_path.display());

    let report_thread_count = Arc::new(AtomicUsize::new(0));

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        match listener.accept() {
            Ok((stream, _addr)) => {
                stream.set_nonblocking(false).ok();
                handle_connection(stream, &broadcast, &dbs, &report_thread_count);
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

/// Read the first line (command) and dispatch to the appropriate handler.
fn handle_connection(
    stream: UnixStream,
    broadcast: &Arc<BroadcastSink>,
    dbs: &[(String, Arc<Database>)],
    report_thread_count: &Arc<AtomicUsize>,
) {
    // 5 second timeout to read the command line
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();

    let mut reader = BufReader::new(&stream);
    let mut command_line = String::new();
    if reader.read_line(&mut command_line).unwrap_or(0) == 0 {
        return;
    }
    let command = command_line.trim();

    match command {
        "TRACE" => {
            stream.set_read_timeout(None).ok();
            let count = broadcast.client_count() + 1;
            eprintln!("[toki:daemon] Trace client connected ({} total)", count);
            broadcast.add_client(stream);
        }
        "REPORT" => {
            // Read the next line as JSON payload
            stream.set_read_timeout(Some(std::time::Duration::from_secs(60))).ok();
            let mut payload_line = String::new();
            if reader.read_line(&mut payload_line).unwrap_or(0) == 0 {
                return;
            }

            let current = report_thread_count.load(Ordering::SeqCst);
            if current >= MAX_REPORT_THREADS {
                eprintln!(
                    "[toki:daemon] Too many concurrent report clients ({}), rejecting",
                    current
                );
                let error_resp = serde_json::json!({
                    "ok": false,
                    "error": "server busy, too many concurrent requests"
                });
                let _ = writeln!(&stream, "{}", serde_json::to_string(&error_resp).unwrap_or_default());
            } else {
                let dbs: Vec<(String, Arc<Database>)> = dbs.to_vec();
                let counter = Arc::clone(report_thread_count);
                counter.fetch_add(1, Ordering::SeqCst);
                std::thread::Builder::new()
                    .name("toki-report".to_string())
                    .spawn(move || {
                        handle_report_client(stream, &payload_line, &dbs);
                        counter.fetch_sub(1, Ordering::SeqCst);
                    })
                    .ok();
            }
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

    let mut parsed =
        crate::query_parser::parse(&req.query).map_err(|e| format!("query parse error: {}", e))?;

    // Resolve time range from request start/end fields
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
        crate::query::execute_parsed_query(db, &parsed, tz, None, &collector, since_ms, until_ms)?;

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
