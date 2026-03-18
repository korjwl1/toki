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
/// Discriminates between trace clients (long-lived stream) and
/// report clients (request-response) by reading the first line.
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

    // Use blocking accept with a read timeout so we can periodically check stop_rx.
    // Much more efficient than non-blocking + 5ms sleep polling.
    listener.set_nonblocking(false).ok();
    // set_read_timeout doesn't exist for UnixListener, but we can set SO_RCVTIMEO
    // on the underlying socket. Use a short accept timeout via the socket option.
    use std::os::unix::io::AsRawFd;
    let timeout = libc::timeval {
        tv_sec: 0,
        tv_usec: 100_000, // 100ms
    };
    unsafe {
        libc::setsockopt(
            listener.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &timeout as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
    }

    eprintln!("[toki:daemon] Listening on {}", sock_path.display());

    let report_thread_count = Arc::new(AtomicUsize::new(0));

    loop {
        // Check for stop signal
        if stop_rx.try_recv().is_ok() {
            break;
        }

        match listener.accept() {
            Ok((stream, _addr)) => {
                stream.set_nonblocking(false).ok();
                classify_and_handle(stream, &broadcast, &dbs, &report_thread_count);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {
                // Timeout expired, loop back to check stop_rx
            }
            Err(e) => {
                eprintln!("[toki:daemon] Accept error: {}", e);
            }
        }
    }

    // Cleanup socket file on exit
    let _ = std::fs::remove_file(sock_path);
    eprintln!("[toki:daemon] Listener stopped");
}

/// Classify a new connection as trace (stream) or report (request-response).
/// If the client sends a JSON line within 200ms, it's a report query.
/// Otherwise, it's a trace client.
fn classify_and_handle(stream: UnixStream, broadcast: &Arc<BroadcastSink>, dbs: &[(String, Arc<Database>)], report_thread_count: &Arc<AtomicUsize>) {
    // Clone the stream for reading; keep original for writing/passing
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => {
            // Can't clone — treat as trace
            broadcast.add_client(stream);
            return;
        }
    };

    // Set a short read timeout to detect trace clients (they never write)
    reader_stream.set_read_timeout(Some(std::time::Duration::from_millis(200))).ok();

    let mut reader = BufReader::new(reader_stream);
    let mut first_line = String::new();

    match reader.read_line(&mut first_line) {
        Ok(n) if n > 0 && first_line.trim_start().starts_with('{') => {
            // Report client — handle in a dedicated thread (with concurrency limit)
            let current = report_thread_count.load(Ordering::SeqCst);
            if current >= MAX_REPORT_THREADS {
                eprintln!("[toki:daemon] Too many concurrent report clients ({}), rejecting", current);
                let error_resp = serde_json::json!({ "ok": false, "error": "server busy, too many concurrent requests" });
                let _ = writeln!(&stream, "{}", serde_json::to_string(&error_resp).unwrap_or_default());
            } else {
                let dbs: Vec<(String, Arc<Database>)> = dbs.to_vec();
                let counter = Arc::clone(report_thread_count);
                counter.fetch_add(1, Ordering::SeqCst);
                std::thread::Builder::new()
                    .name("toki-report".to_string())
                    .spawn(move || {
                        handle_report_client(stream, &first_line, &dbs);
                        counter.fetch_sub(1, Ordering::SeqCst);
                    })
                    .ok();
            }
        }
        _ => {
            // Trace client — add to broadcast
            stream.set_read_timeout(None).ok();
            let count = broadcast.client_count() + 1;
            eprintln!("[toki:daemon] Trace client connected ({} total)", count);
            broadcast.add_client(stream);
        }
    }
}

/// Handle a report query: parse request, execute query, send response.
fn handle_report_client(mut stream: UnixStream, request_line: &str, dbs: &[(String, Arc<Database>)]) {
    // Remove read timeout for query execution
    stream.set_read_timeout(None).ok();

    let response = match execute_report_request(request_line, dbs) {
        Ok(data) => serde_json::json!({ "ok": true, "data": data }),
        Err(e) => serde_json::json!({ "ok": false, "error": e }),
    };

    let line = serde_json::to_string(&response).unwrap_or_default();
    let _ = writeln!(stream, "{}", line);
    let _ = stream.flush();
}

/// Parse and execute a report request against all provider DBs, merging results.
fn execute_report_request(request_line: &str, dbs: &[(String, Arc<Database>)]) -> Result<serde_json::Value, String> {
    let req: ReportRequest = serde_json::from_str(request_line)
        .map_err(|e| format!("invalid request: {}", e))?;

    let tz: Option<chrono_tz::Tz> = req.tz.as_deref()
        .map(|s| s.parse().map_err(|_| format!("invalid timezone: {}", s)))
        .transpose()?;

    let parsed = crate::query_parser::parse(&req.query)
        .map_err(|e| format!("query parse error: {}", e))?;

    // Strip "provider" from group_by before passing to per-DB query execution
    // (provider grouping is handled at the DB routing level in the listener)
    let mut parsed = parsed;
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

    // If provider filter specified but no matching DB, return empty
    if target_dbs.is_empty() {
        return Ok(serde_json::Value::Array(Vec::new()));
    }

    // Collect results per provider, each tagged with its schema.
    // Client iterates the array and renders each provider's table separately.
    let mut all_results: Vec<serde_json::Value> = Vec::new();

    for (provider_name, db) in &target_dbs {
        let collector = CollectorSink::new();
        crate::query::execute_parsed_query(db, &parsed, tz, None, &collector)?;

        let mut provider_results = collector.take();
        // Tag each result with this provider's schema
        for item in &mut provider_results {
            item["schema"] = serde_json::json!(provider_name);
        }
        all_results.extend(provider_results);
    }

    Ok(serde_json::Value::Array(all_results))
}

/// Request payload from report client.
#[derive(serde::Deserialize)]
struct ReportRequest {
    query: String,
    #[serde(default)]
    tz: Option<String>,
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
        self.collected.into_inner().unwrap_or_else(|e| e.into_inner())
    }

}

impl crate::sink::Sink for CollectorSink {
    fn emit_summary(&self, summaries: &std::collections::HashMap<String, crate::common::types::ModelUsageSummary>, pricing: Option<&crate::pricing::PricingTable>, _schema: Option<&dyn ProviderSchema>) {
        let json = crate::sink::json::summaries_to_json(summaries, pricing, None);
        self.collected.lock().unwrap_or_else(|e| e.into_inner()).push(json);
    }

    fn emit_grouped(&self, grouped: &std::collections::HashMap<String, std::collections::HashMap<String, crate::common::types::ModelUsageSummary>>, type_name: &str, pricing: Option<&crate::pricing::PricingTable>, _schema: Option<&dyn ProviderSchema>) {
        let json = crate::sink::json::grouped_to_json(grouped, type_name, pricing, None);
        self.collected.lock().unwrap_or_else(|e| e.into_inner()).push(json);
    }

    fn emit_event(&self, event: &crate::common::types::UsageEvent, pricing: Option<&crate::pricing::PricingTable>) {
        let json = crate::sink::json::event_to_json(event, pricing);
        self.collected.lock().unwrap_or_else(|e| e.into_inner()).push(json);
    }

    fn emit_list(&self, items: &[String], type_name: &str) {
        self.collected.lock().unwrap_or_else(|e| e.into_inner())
            .push(serde_json::json!({ "type": type_name, "items": items }));
    }
}
