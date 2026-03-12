use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::common::types::{ModelUsageSummary, UsageEvent};
use crate::db::Database;

const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// Per-model pricing (cost per token).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input_cost_per_token: f64,
    pub output_cost_per_token: f64,
    #[serde(default)]
    pub cache_creation_input_token_cost: Option<f64>,
    #[serde(default)]
    pub cache_read_input_token_cost: Option<f64>,
}

impl ModelPricing {
    /// Calculate cost from token counts.
    fn cost(&self, input: u64, output: u64, cache_create: u64, cache_read: u64) -> f64 {
        (input as f64) * self.input_cost_per_token
            + (output as f64) * self.output_cost_per_token
            + (cache_create as f64) * self.cache_creation_input_token_cost.unwrap_or(0.0)
            + (cache_read as f64) * self.cache_read_input_token_cost.unwrap_or(0.0)
    }
}

/// Cached pricing table. Exact model name match only — no fuzzy matching
/// to avoid mismatched pricing (e.g. opus-4 vs opus-4-6).
pub struct PricingTable {
    prices: HashMap<String, ModelPricing>,
}

impl PricingTable {
    pub fn new(prices: HashMap<String, ModelPricing>) -> Self {
        PricingTable { prices }
    }

    pub fn is_empty(&self) -> bool {
        self.prices.is_empty()
    }

    /// Look up pricing for a model name (exact match only).
    pub fn get(&self, model: &str) -> Option<&ModelPricing> {
        self.prices.get(model)
    }

    /// Calculate cost for a ModelUsageSummary.
    pub fn summary_cost(&self, s: &ModelUsageSummary) -> Option<f64> {
        self.get(&s.model).map(|p| p.cost(
            s.input_tokens, s.output_tokens,
            s.cache_creation_input_tokens, s.cache_read_input_tokens,
        ))
    }

    /// Calculate cost for a single UsageEvent.
    pub fn event_cost(&self, e: &UsageEvent) -> Option<f64> {
        self.get(&e.model).map(|p| p.cost(
            e.input_tokens, e.output_tokens,
            e.cache_creation_input_tokens, e.cache_read_input_tokens,
        ))
    }
}

/// Parse LiteLLM JSON and extract Claude model prices.
/// Uses streaming key inspection to skip non-Claude entries without full deserialization.
fn parse_litellm_json(json_str: &str) -> HashMap<String, ModelPricing> {
    // serde_json requires full parse for a JSON object, but LiteLLMEntry is only 4 Option<f64>
    // fields — serde skips unknown fields by default, so non-matching values are cheap.
    let raw: HashMap<String, LiteLLMEntry> = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };

    let mut prices = HashMap::with_capacity(32);

    for (key, entry) in &raw {
        let model_name = if let Some(stripped) = key.strip_prefix("anthropic/") {
            stripped
        } else if key.starts_with("claude-") {
            key.as_str()
        } else {
            continue;
        };

        let (input_cost, output_cost) = match (entry.input_cost_per_token, entry.output_cost_per_token) {
            (Some(i), Some(o)) => (i, o),
            _ => continue,
        };

        prices.entry(model_name.to_string()).or_insert(ModelPricing {
            input_cost_per_token: input_cost,
            output_cost_per_token: output_cost,
            cache_creation_input_token_cost: entry.cache_creation_input_token_cost,
            cache_read_input_token_cost: entry.cache_read_input_token_cost,
        });
    }

    prices
}

/// Raw entry from LiteLLM JSON (only fields we need).
#[derive(Deserialize)]
struct LiteLLMEntry {
    #[serde(default)]
    input_cost_per_token: Option<f64>,
    #[serde(default)]
    output_cost_per_token: Option<f64>,
    #[serde(default)]
    cache_creation_input_token_cost: Option<f64>,
    #[serde(default)]
    cache_read_input_token_cost: Option<f64>,
}

/// Fetch pricing with ETag-based caching.
/// Falls back to cached data on network errors. Returns empty table if no cache available.
pub fn fetch_pricing(db: &Database) -> PricingTable {
    let cached_etag = db.get_setting("pricing_etag").ok().flatten();
    let cached_data = db.get_setting("pricing_data").ok().flatten();

    let mut req = ureq::get(LITELLM_URL);
    if let Some(ref etag) = cached_etag {
        req = req.set("If-None-Match", etag);
    }

    match req.call() {
        Ok(resp) => {
            if resp.status() == 304 {
                if let Some(ref data) = cached_data {
                    let prices: HashMap<String, ModelPricing> =
                        serde_json::from_str(data).unwrap_or_default();
                    eprintln!("[clitrace] Pricing: cached (not modified), {} models", prices.len());
                    return PricingTable::new(prices);
                }
            }

            let new_etag = resp.header("ETag").map(|s| s.to_string());
            let body = match resp.into_string() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[clitrace] Pricing: failed to read response body: {}", e);
                    return fallback_cached(cached_data);
                }
            };

            let prices = parse_litellm_json(&body);
            if prices.is_empty() {
                eprintln!("[clitrace] Pricing: no Claude models found in response");
                return fallback_cached(cached_data);
            }

            if let Ok(json) = serde_json::to_string(&prices) {
                let _ = db.set_setting("pricing_data", &json);
            }
            if let Some(etag) = new_etag {
                let _ = db.set_setting("pricing_etag", &etag);
            }

            eprintln!("[clitrace] Pricing: updated, {} models", prices.len());
            PricingTable::new(prices)
        }
        Err(ureq::Error::Status(304, _)) => {
            if let Some(ref data) = cached_data {
                let prices: HashMap<String, ModelPricing> =
                    serde_json::from_str(data).unwrap_or_default();
                eprintln!("[clitrace] Pricing: cached (not modified), {} models", prices.len());
                return PricingTable::new(prices);
            }
            PricingTable::new(HashMap::new())
        }
        Err(e) => {
            eprintln!("[clitrace] Pricing: network error ({}), using cache", e);
            fallback_cached(cached_data)
        }
    }
}

/// Load pricing from DB cache only (no network).
pub fn load_cached_pricing(db: &Database) -> PricingTable {
    fallback_cached(db.get_setting("pricing_data").ok().flatten())
}

fn fallback_cached(cached_data: Option<String>) -> PricingTable {
    match cached_data {
        Some(data) => {
            let prices: HashMap<String, ModelPricing> =
                serde_json::from_str(&data).unwrap_or_default();
            if !prices.is_empty() {
                eprintln!("[clitrace] Pricing: using cached data, {} models", prices.len());
            }
            PricingTable::new(prices)
        }
        None => PricingTable::new(HashMap::new()),
    }
}

/// Format cost as string for display.
pub fn format_cost(cost: Option<f64>) -> String {
    match cost {
        Some(c) if c < 0.01 => format!("${:.4}", c),
        Some(c) => format!("${:.2}", c),
        None => "-".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_litellm_json() {
        let json = r#"{
            "claude-sonnet-4-20250514": {
                "input_cost_per_token": 0.000003,
                "output_cost_per_token": 0.000015,
                "cache_creation_input_token_cost": 0.00000375,
                "cache_read_input_token_cost": 0.0000003
            },
            "gpt-4": {
                "input_cost_per_token": 0.00003,
                "output_cost_per_token": 0.00006
            },
            "anthropic/claude-opus-4-20250514": {
                "input_cost_per_token": 0.000015,
                "output_cost_per_token": 0.000075,
                "cache_creation_input_token_cost": 0.00001875,
                "cache_read_input_token_cost": 0.0000015
            }
        }"#;

        let prices = parse_litellm_json(json);
        assert!(prices.contains_key("claude-sonnet-4-20250514"));
        assert!(prices.contains_key("claude-opus-4-20250514"));
        assert!(!prices.contains_key("gpt-4"));
    }

    #[test]
    fn test_pricing_table_exact_match() {
        let mut prices = HashMap::new();
        prices.insert("claude-sonnet-4-20250514".to_string(), ModelPricing {
            input_cost_per_token: 0.000003,
            output_cost_per_token: 0.000015,
            cache_creation_input_token_cost: Some(0.00000375),
            cache_read_input_token_cost: Some(0.0000003),
        });
        let table = PricingTable::new(prices);
        assert!(table.get("claude-sonnet-4-20250514").is_some());
    }

    #[test]
    fn test_pricing_table_no_match() {
        let mut prices = HashMap::new();
        prices.insert("claude-sonnet-4-20250514".to_string(), ModelPricing {
            input_cost_per_token: 0.000003,
            output_cost_per_token: 0.000015,
            cache_creation_input_token_cost: None,
            cache_read_input_token_cost: None,
        });
        let table = PricingTable::new(prices);
        // Different model name → no match (exact only)
        assert!(table.get("claude-sonnet-4-20250601").is_none());
        assert!(table.get("claude-opus-4-20250514").is_none());
    }

    #[test]
    fn test_summary_cost() {
        let mut prices = HashMap::new();
        prices.insert("claude-sonnet-4-20250514".to_string(), ModelPricing {
            input_cost_per_token: 0.000003,
            output_cost_per_token: 0.000015,
            cache_creation_input_token_cost: Some(0.00000375),
            cache_read_input_token_cost: Some(0.0000003),
        });
        let table = PricingTable::new(prices);

        let summary = ModelUsageSummary {
            model: "claude-sonnet-4-20250514".to_string(),
            input_tokens: 1000,
            output_tokens: 500,
            cache_creation_input_tokens: 200,
            cache_read_input_tokens: 3000,
            event_count: 5,
        };
        let cost = table.summary_cost(&summary).unwrap();
        // 1000*0.000003 + 500*0.000015 + 200*0.00000375 + 3000*0.0000003 = 0.01215
        assert!((cost - 0.01215).abs() < 1e-10);
    }

    #[test]
    fn test_event_cost() {
        let mut prices = HashMap::new();
        prices.insert("claude-sonnet-4-20250514".to_string(), ModelPricing {
            input_cost_per_token: 0.000003,
            output_cost_per_token: 0.000015,
            cache_creation_input_token_cost: None,
            cache_read_input_token_cost: None,
        });
        let table = PricingTable::new(prices);

        let event = UsageEvent {
            event_key: "test".to_string(),
            source_file: "test.jsonl".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            input_tokens: 1000,
            output_tokens: 500,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        let cost = table.event_cost(&event).unwrap();
        assert!((cost - 0.0105).abs() < 1e-10);
    }

    #[test]
    fn test_format_cost() {
        assert_eq!(format_cost(Some(1.5)), "$1.50");
        assert_eq!(format_cost(Some(0.005)), "$0.0050");
        assert_eq!(format_cost(Some(0.0)), "$0.0000");
        assert_eq!(format_cost(None), "-");
    }
}
