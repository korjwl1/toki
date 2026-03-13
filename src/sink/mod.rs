pub(crate) mod json;
mod print;
mod uds;
mod http;

pub use print::{PrintSink, OutputFormat};
pub use uds::UdsSink;
pub use self::http::HttpSink;

use std::collections::HashMap;

use crate::common::types::{ModelUsageSummary, UsageEvent};
use crate::pricing::PricingTable;

/// Output sink for emitting usage data.
/// All implementations must be thread-safe (used in watch mode worker thread).
pub trait Sink: Send + Sync {
    fn emit_summary(&self, summaries: &HashMap<String, ModelUsageSummary>, pricing: Option<&PricingTable>);
    fn emit_grouped(&self, grouped: &HashMap<String, HashMap<String, ModelUsageSummary>>, type_name: &str, pricing: Option<&PricingTable>);
    fn emit_event(&self, event: &UsageEvent, pricing: Option<&PricingTable>);
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
    fn emit_summary(&self, summaries: &HashMap<String, ModelUsageSummary>, pricing: Option<&PricingTable>) {
        for s in &self.sinks { s.emit_summary(summaries, pricing); }
    }

    fn emit_grouped(&self, grouped: &HashMap<String, HashMap<String, ModelUsageSummary>>, type_name: &str, pricing: Option<&PricingTable>) {
        for s in &self.sinks { s.emit_grouped(grouped, type_name, pricing); }
    }

    fn emit_event(&self, event: &UsageEvent, pricing: Option<&PricingTable>) {
        for s in &self.sinks { s.emit_event(event, pricing); }
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
            eprintln!("[clitrace] Unknown sink: {} (use: print, uds://<path>, http://<url>)", spec);
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
pub(crate) fn shorten_id(id: &str) -> &str {
    if id.len() > 8 { &id[..8] } else { id }
}

/// Extract a human-readable label from a source file path.
///   Parent:   .../projects/<dir>/<UUID>.jsonl        → "<UUID short>"
///   Subagent: .../<UUID>/subagents/agent-<id>.jsonl  → "<UUID short>/agent-<id short>"
pub(crate) fn format_source_label(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').collect();
    let filename = parts.first().map_or("", |s| s.trim_end_matches(".jsonl"));

    if parts.len() >= 3 && parts[1] == "subagents" {
        let session_id = shorten_id(parts[2]);
        let agent_id = shorten_id(filename);
        return format!("{}/{}", session_id, agent_id);
    }

    shorten_id(filename).to_string()
}
