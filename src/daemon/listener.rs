use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;

use super::BroadcastSink;
use crate::db::Database;

/// Run the UDS listener in a loop, accepting new clients.
/// Discriminates between trace clients (long-lived stream) and
/// report clients (request-response) by reading the first line.
/// Blocks until `stop_rx` fires or the listener is dropped.
pub fn run_listener(
    sock_path: &Path,
    broadcast: Arc<BroadcastSink>,
    db: Arc<Database>,
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

    // Set non-blocking so we can check stop_rx periodically
    listener.set_nonblocking(true).ok();

    eprintln!("[toki:daemon] Listening on {}", sock_path.display());

    loop {
        // Check for stop signal
        if stop_rx.try_recv().is_ok() {
            break;
        }

        match listener.accept() {
            Ok((stream, _addr)) => {
                stream.set_nonblocking(false).ok();
                classify_and_handle(stream, &broadcast, &db);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => {
                eprintln!("[toki:daemon] Accept error: {}", e);
                std::thread::sleep(std::time::Duration::from_millis(5));
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
fn classify_and_handle(stream: UnixStream, broadcast: &Arc<BroadcastSink>, db: &Arc<Database>) {
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
            // Report client — handle in a dedicated thread
            let db = db.clone();
            std::thread::Builder::new()
                .name("toki-report".to_string())
                .spawn(move || {
                    handle_report_client(stream, &first_line, &db);
                })
                .ok();
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
fn handle_report_client(mut stream: UnixStream, request_line: &str, db: &Database) {
    // Remove read timeout for query execution
    stream.set_read_timeout(None).ok();

    let response = match execute_report_request(request_line, db) {
        Ok(data) => serde_json::json!({ "ok": true, "data": data }),
        Err(e) => serde_json::json!({ "ok": false, "error": e }),
    };

    let line = serde_json::to_string(&response).unwrap_or_default();
    let _ = writeln!(stream, "{}", line);
    let _ = stream.flush();
}

/// Parse and execute a report request.
fn execute_report_request(request_line: &str, db: &Database) -> Result<serde_json::Value, String> {
    let req: ReportRequest = serde_json::from_str(request_line)
        .map_err(|e| format!("invalid request: {}", e))?;

    let tz: Option<chrono_tz::Tz> = req.tz.as_deref()
        .map(|s| s.parse().map_err(|_| format!("invalid timezone: {}", s)))
        .transpose()?;

    let parsed = crate::query_parser::parse(&req.query)
        .map_err(|e| format!("query parse error: {}", e))?;

    // Collect results into JSON (no pricing — client handles cost calculation)
    let collector = CollectorSink::new();
    crate::query::execute_parsed_query(db, &parsed, tz, None, &collector)?;

    let results = collector.take();
    Ok(serde_json::Value::Array(results))
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
    fn emit_summary(&self, summaries: &std::collections::HashMap<String, crate::common::types::ModelUsageSummary>, pricing: Option<&crate::pricing::PricingTable>) {
        let json = crate::sink::json::summaries_to_json(summaries, pricing);
        self.collected.lock().unwrap_or_else(|e| e.into_inner()).push(json);
    }

    fn emit_grouped(&self, grouped: &std::collections::HashMap<String, std::collections::HashMap<String, crate::common::types::ModelUsageSummary>>, type_name: &str, pricing: Option<&crate::pricing::PricingTable>) {
        let json = crate::sink::json::grouped_to_json(grouped, type_name, pricing);
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
