use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;

use crate::common::types::{ModelUsageSummary, UsageEvent};
use crate::pricing::PricingTable;
use crate::sink::Sink;
use crate::sink::json::{event_to_json, summaries_to_json, grouped_to_json};

/// A client connection with buffered writer.
struct ClientConn {
    writer: BufWriter<UnixStream>,
}

/// Broadcast sink that fans out events to connected trace clients.
/// When no clients are connected, all emit methods are no-ops (zero overhead).
pub struct BroadcastSink {
    clients: Mutex<Vec<ClientConn>>,
}

impl BroadcastSink {
    pub fn new() -> Self {
        BroadcastSink {
            clients: Mutex::new(Vec::new()),
        }
    }

    /// Add a new client connection.
    pub fn add_client(&self, stream: UnixStream) {
        // Set write timeout to avoid blocking engine thread on slow clients
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(1)));
        let writer = BufWriter::new(stream);
        self.clients.lock().unwrap_or_else(|e| e.into_inner())
            .push(ClientConn { writer });
    }

    /// Number of connected clients.
    pub fn client_count(&self) -> usize {
        self.clients.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Send a pre-serialized JSON line to all clients.
    /// Removes any client that fails to write.
    fn broadcast(&self, json: &serde_json::Value) {
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if clients.is_empty() {
            return;
        }
        let line = serde_json::to_string(json).unwrap();
        clients.retain_mut(|c| {
            writeln!(c.writer, "{}", line).is_ok()
                && c.writer.flush().is_ok()
        });
    }
}

impl Sink for BroadcastSink {
    fn emit_event(&self, event: &UsageEvent, pricing: Option<&PricingTable>) {
        let json = event_to_json(event, pricing);
        self.broadcast(&json);
    }

    fn emit_summary(&self, summaries: &HashMap<String, ModelUsageSummary>, pricing: Option<&PricingTable>) {
        let json = summaries_to_json(summaries, pricing);
        self.broadcast(&json);
    }

    fn emit_grouped(&self, grouped: &HashMap<String, HashMap<String, ModelUsageSummary>>, type_name: &str, pricing: Option<&PricingTable>) {
        let json = grouped_to_json(grouped, type_name, pricing);
        self.broadcast(&json);
    }
}

impl Sink for std::sync::Arc<BroadcastSink> {
    fn emit_event(&self, event: &UsageEvent, pricing: Option<&PricingTable>) {
        (**self).emit_event(event, pricing);
    }

    fn emit_summary(&self, summaries: &HashMap<String, ModelUsageSummary>, pricing: Option<&PricingTable>) {
        (**self).emit_summary(summaries, pricing);
    }

    fn emit_grouped(&self, grouped: &HashMap<String, HashMap<String, ModelUsageSummary>>, type_name: &str, pricing: Option<&PricingTable>) {
        (**self).emit_grouped(grouped, type_name, pricing);
    }
}
