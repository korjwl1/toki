use std::collections::HashMap;

use crate::common::schema::{ClaudeCodeSchema, ProviderSchema};
use crate::common::types::{ModelUsageSummary, UsageEvent};
use crate::pricing::PricingTable;
use super::format_source_label;

/// Build a JSON entry for a single model summary.
pub(crate) fn summary_to_json(s: &ModelUsageSummary, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) -> serde_json::Value {
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

    if let Some(cost) = pricing.and_then(|p| p.summary_cost(s)) {
        entry["cost_usd"] = serde_json::json!(cost);
    }
    entry
}

/// Build JSON payload for a flat summary.
pub(crate) fn summaries_to_json(summaries: &HashMap<String, ModelUsageSummary>, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) -> serde_json::Value {
    let mut sorted: Vec<_> = summaries.values().collect();
    sorted.sort_by(|a, b| b.event_count.cmp(&a.event_count));
    let data: Vec<_> = sorted.iter().map(|s| summary_to_json(s, pricing, schema)).collect();
    serde_json::json!({ "type": "summary", "data": data })
}

/// Build JSON payload for a grouped summary.
pub(crate) fn grouped_to_json(
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
pub(crate) fn event_to_json(event: &UsageEvent, pricing: Option<&PricingTable>, schema: Option<&dyn ProviderSchema>) -> serde_json::Value {
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

    let cost = pricing.and_then(|p| p.event_cost(event));
    let mut data = serde_json::json!({
        "model": event.model,
        "source": format_source_label(&event.source_file),
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
