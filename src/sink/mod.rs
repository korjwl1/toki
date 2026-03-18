pub(crate) mod json;
mod print;
mod uds;
mod http;

pub use print::{PrintSink, OutputFormat};
pub use uds::UdsSink;
pub use self::http::HttpSink;

use std::collections::HashMap;

use crate::common::schema::ProviderSchema;
use crate::common::types::{ModelUsageSummary, UsageEvent};
use crate::pricing::PricingTable;

/// Output sink for emitting usage data.
/// All implementations must be thread-safe (used in watch mode worker thread).
pub trait Sink: Send + Sync {
    fn emit_summary(&self, summaries: &HashMap<String, ModelUsageSummary>, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>);
    fn emit_grouped(&self, grouped: &HashMap<String, HashMap<String, ModelUsageSummary>>, type_name: &str, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>);
    fn emit_event(&self, event: &UsageEvent, pricing: Option<&PricingTable>);
    fn emit_list(&self, items: &[String], type_name: &str);
}

/// Dispatch to multiple sinks simultaneously.
pub struct MultiSink {
    sinks: Vec<Box<dyn Sink>>,
}

impl MultiSink {
    pub fn new(sinks: Vec<Box<dyn Sink>>) -> Self {
        MultiSink { sinks }
    }
}

impl Sink for MultiSink {
    fn emit_summary(&self, summaries: &HashMap<String, ModelUsageSummary>, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) {
        for s in &self.sinks { s.emit_summary(summaries, pricing, schema); }
    }

    fn emit_grouped(&self, grouped: &HashMap<String, HashMap<String, ModelUsageSummary>>, type_name: &str, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) {
        for s in &self.sinks { s.emit_grouped(grouped, type_name, pricing, schema); }
    }

    fn emit_event(&self, event: &UsageEvent, pricing: Option<&PricingTable>) {
        for s in &self.sinks { s.emit_event(event, pricing); }
    }

    fn emit_list(&self, items: &[String], type_name: &str) {
        for s in &self.sinks { s.emit_list(items, type_name); }
    }
}

/// Create sink(s) from `--sink` argument values.
pub fn create_sinks(specs: &[String], print_format: OutputFormat) -> Box<dyn Sink> {
    let mut sinks: Vec<Box<dyn Sink>> = Vec::new();

    for spec in specs {
        if spec == "print" {
            sinks.push(Box::new(PrintSink::new(print_format)));
        } else if let Some(path) = spec.strip_prefix("uds://") {
            sinks.push(Box::new(UdsSink::new(path.to_string())));
        } else if spec.starts_with("http://") || spec.starts_with("https://") {
            sinks.push(Box::new(HttpSink::new(spec.to_string())));
        } else {
            eprintln!("[toki] Unknown sink: {} (use: print, uds://<path>, http://<url>)", spec);
            std::process::exit(1);
        }
    }

    if sinks.is_empty() {
        sinks.push(Box::new(PrintSink::new(print_format)));
    }

    if sinks.len() == 1 {
        sinks.into_iter().next().unwrap()
    } else {
        Box::new(MultiSink::new(sinks))
    }
}

/// Shorten a UUID or agent ID to first 8 chars.
/// Uses `get` to safely handle UTF-8 boundary cases.
pub(crate) fn shorten_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Extract a human-readable label from a source file path.
///   Parent:   .../projects/<dir>/<UUID>.jsonl        → "<UUID short>"
///   Subagent: .../<UUID>/subagents/agent-<id>.jsonl  → "<UUID short>/agent-<id short>"
pub(crate) fn format_source_label(path: &str) -> String {
    let mut parts = path.rsplit('/');
    let filename = parts.next().map_or("", |s| s.trim_end_matches(".jsonl"));
    let dir = parts.next().unwrap_or("");
    let grandparent = parts.next().unwrap_or("");

    if dir == "subagents" && !grandparent.is_empty() {
        let session_id = shorten_id(grandparent);
        let agent_id = shorten_id(filename);
        return format!("{}/{}", session_id, agent_id);
    }

    shorten_id(filename).to_string()
}
