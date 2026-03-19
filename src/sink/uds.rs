use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;

use crate::common::schema::ProviderSchema;
use crate::common::types::{ModelUsageSummary, UsageEventWithTs};
use crate::pricing::PricingTable;
use super::{Sink, json};

/// Unix Domain Socket sink: sends NDJSON (newline-delimited JSON).
pub struct UdsSink {
    path: String,
    conn: Mutex<Option<UnixStream>>,
}

impl UdsSink {
    pub fn new(path: String) -> Self {
        let conn = UnixStream::connect(&path).ok();
        if conn.is_some() {
            eprintln!("[toki] UDS: connected to {}", path);
        } else {
            eprintln!("[toki] UDS: will connect to {} on first event", path);
        }
        UdsSink { path, conn: Mutex::new(conn) }
    }

    fn send(&self, value: &serde_json::Value) {
        let data = match serde_json::to_string(value) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[toki] UDS: serialization error: {}", e);
                return;
            }
        };

        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());

        // Try existing connection
        if let Some(ref mut stream) = *conn {
            if writeln!(stream, "{}", data).is_ok() {
                return;
            }
        }

        // Reconnect and retry
        match UnixStream::connect(&self.path) {
            Ok(mut stream) => {
                let _ = writeln!(stream, "{}", data);
                *conn = Some(stream);
            }
            Err(e) => {
                eprintln!("[toki] UDS: failed to connect to {}: {}", self.path, e);
                *conn = None;
            }
        }
    }
}

impl Sink for UdsSink {
    fn emit_summary(&self, summaries: &HashMap<String, ModelUsageSummary>, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) {
        self.send(&json::summaries_to_json(summaries, pricing, schema));
    }

    fn emit_grouped(&self, grouped: &HashMap<String, HashMap<String, ModelUsageSummary>>, type_name: &str, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) {
        self.send(&json::grouped_to_json(grouped, type_name, pricing, schema));
    }

    fn emit_event(&self, event: &UsageEventWithTs, pricing: Option<&PricingTable>, _schema: Option<&dyn ProviderSchema>) {
        self.send(&json::event_to_json(event, pricing, _schema));
    }

    fn emit_list(&self, items: &[String], type_name: &str) {
        self.send(&serde_json::json!({ "type": type_name, "items": items }));
    }

    fn emit_raw(&self, line: &str) {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref mut stream) = *conn {
            if writeln!(stream, "{}", line).is_ok() {
                return;
            }
        }
        // Reconnect and retry
        if let Ok(mut stream) = UnixStream::connect(&self.path) {
            let _ = writeln!(stream, "{}", line);
            *conn = Some(stream);
        }
    }
}
