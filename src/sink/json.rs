use std::collections::HashMap;

use crate::common::schema::{ClaudeCodeSchema, ProviderSchema};
use crate::common::types::{ModelUsageSummary, RawEvent, UsageEventWithTs};
use crate::pricing::PricingTable;
use super::format_source_label;

/// Build a JSON entry for a single model summary.
pub fn summary_to_json(s: &ModelUsageSummary, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) -> serde_json::Value {
    let schema: &dyn ProviderSchema = schema.unwrap_or(&ClaudeCodeSchema);
    let columns = schema.columns();
    let tokens = schema.extract_tokens(s);
    let total = schema.total_tokens(s);

    let mut entry = serde_json::json!({ "model": s.model });

    for (i, col) in columns.iter().enumerate() {
        entry[col.json_key] = serde_json::json!(tokens[i]);
    }

    entry["total_tokens"] = serde_json::json!(total);
    entry["events"] = serde_json::json!(s.event_count);

    // Pre-computed cost (e.g. from server) takes priority over local pricing
    if let Some(cost) = s.cost_usd.filter(|c| *c > 0.0).or_else(|| pricing.and_then(|p| p.summary_cost(s))) {
        entry["cost_usd"] = serde_json::json!(cost);
    }
    entry
}

/// Build JSON payload for a flat summary.
pub fn summaries_to_json(summaries: &HashMap<String, ModelUsageSummary>, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) -> serde_json::Value {
    let mut sorted: Vec<_> = summaries.values().collect();
    sorted.sort_by(|a, b| b.event_count.cmp(&a.event_count));
    let data: Vec<_> = sorted.iter().map(|s| summary_to_json(s, pricing, schema)).collect();
    serde_json::json!({ "type": "summary", "data": data })
}

/// Build JSON payload for a grouped summary.
pub fn grouped_to_json(
    grouped: &HashMap<String, HashMap<String, ModelUsageSummary>>,
    type_name: &str,
    pricing: Option<&PricingTable>,
    schema: Option<&dyn ProviderSchema>,
) -> serde_json::Value {
    let is_session = type_name == "session";
    let is_provider = type_name == "provider";
    let json_key = if is_session { "session" } else if is_provider { "provider" } else { "period" };

    let mut buckets: Vec<&String> = grouped.keys().collect();
    buckets.sort();

    let data: Vec<_> = buckets.iter().filter_map(|bucket| {
        grouped.get(bucket.as_str()).map(|models| {
            let mut sorted: Vec<_> = models.values().collect();
            sorted.sort_by(|a, b| b.event_count.cmp(&a.event_count));
            let usage: Vec<_> = sorted.iter().map(|s| summary_to_json(s, pricing, schema)).collect();
            serde_json::json!({
                json_key: bucket,
                "usage_per_models": usage,
            })
        })
    }).collect();

    serde_json::json!({ "type": type_name, "data": data })
}

/// Build JSON payload for a single watch-mode event.
pub fn event_to_json(event: &UsageEventWithTs, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) -> serde_json::Value {
    let schema = schema.unwrap_or(&ClaudeCodeSchema);
    let columns = schema.columns();
    let summary = crate::common::types::ModelUsageSummary {
        model: event.model.clone(),
        input_tokens: event.input_tokens,
        output_tokens: event.output_tokens,
        cache_creation_input_tokens: event.cache_creation_input_tokens,
        cache_read_input_tokens: event.cache_read_input_tokens,
        event_count: 0,
        cost_usd: None,
    };
    let tokens = schema.extract_tokens(&summary);

    let cost = pricing.and_then(|p| p.event_cost_with_ts(event));
    let mut data = serde_json::json!({
        "model": event.model,
        "source": format_source_label(&event.source_file),
        "provider": schema.provider_name(),
        "timestamp": event.timestamp,
    });
    for (i, col) in columns.iter().enumerate() {
        if i < tokens.len() {
            data[col.json_key] = serde_json::json!(tokens[i]);
        }
    }
    if let Some(c) = cost {
        data["cost_usd"] = serde_json::json!(c);
    }
    serde_json::json!({ "type": "event", "data": data })
}

/// Build JSON payload for a batch of raw events.
pub fn events_batch_to_json(
    events: &[RawEvent],
    pricing: Option<&PricingTable>,
    schema: Option<&dyn ProviderSchema>,
) -> serde_json::Value {
    let schema: &dyn ProviderSchema = schema.unwrap_or(&ClaudeCodeSchema);
    let columns = schema.columns();

    let data: Vec<serde_json::Value> = events.iter().map(|e| {
        let summary = ModelUsageSummary {
            model: e.model.clone(),
            input_tokens: e.input_tokens,
            output_tokens: e.output_tokens,
            cache_creation_input_tokens: e.cache_creation_input_tokens,
            cache_read_input_tokens: e.cache_read_input_tokens,
            event_count: 0,
            cost_usd: None,
        };
        let tokens = schema.extract_tokens(&summary);
        let total = schema.total_tokens(&summary);

        let mut entry = serde_json::json!({
            "timestamp": e.timestamp,
            "model": e.model,
            "session": e.session,
            "project": e.project,
        });
        for (i, col) in columns.iter().enumerate() {
            entry[col.json_key] = serde_json::json!(tokens[i]);
        }
        entry["total_tokens"] = serde_json::json!(total);

        if let Some(cost) = pricing.and_then(|p| p.summary_cost(&summary)) {
            entry["cost_usd"] = serde_json::json!(cost);
        }
        entry
    }).collect();

    serde_json::json!({ "type": "events", "data": data })
}
