use std::collections::HashMap;
use std::time::Duration;

use crate::common::types::{ModelUsageSummary, UsageEvent};
use crate::pricing::PricingTable;
use super::{Sink, json};

/// Connect + read timeout for HTTP sink.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// HTTP POST sink: sends JSON payloads to a remote endpoint.
/// Fire-and-forget with 5s timeout — never blocks the watch loop.
pub struct HttpSink {
    agent: ureq::Agent,
    url: String,
}

impl HttpSink {
    pub fn new(url: String) -> Self {
        eprintln!("[toki] HTTP sink: {}", url);
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(HTTP_TIMEOUT)
            .timeout_read(HTTP_TIMEOUT)
            .timeout_write(HTTP_TIMEOUT)
            .build();
        HttpSink { agent, url }
    }

    fn send(&self, value: &serde_json::Value) {
        let body = match serde_json::to_string(value) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[toki] HTTP: serialization error: {}", e);
                return;
            }
        };

        if let Err(e) = self.agent.post(&self.url)
            .set("Content-Type", "application/json")
            .send_string(&body)
        {
            eprintln!("[toki] HTTP sink error: {}", e);
        }
    }
}

impl Sink for HttpSink {
    fn emit_summary(&self, summaries: &HashMap<String, ModelUsageSummary>, pricing: Option<&PricingTable>) {
        self.send(&json::summaries_to_json(summaries, pricing));
    }

    fn emit_grouped(&self, grouped: &HashMap<String, HashMap<String, ModelUsageSummary>>, type_name: &str, pricing: Option<&PricingTable>) {
        self.send(&json::grouped_to_json(grouped, type_name, pricing));
    }

    fn emit_event(&self, event: &UsageEvent, pricing: Option<&PricingTable>) {
        self.send(&json::event_to_json(event, pricing));
    }

    fn emit_list(&self, items: &[String], type_name: &str) {
        self.send(&serde_json::json!({ "type": type_name, "items": items }));
    }
}
