use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::common::schema::ProviderSchema;
use crate::common::types::{ModelUsageSummary, UsageEvent};

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
    /// Rate to charge cached-input tokens at when LiteLLM publishes no
    /// `cache_read_input_token_cost` for the model.
    ///
    /// Falling back to 0.0 would be a silent guess that cached input is free.
    /// It never is, and for schemas that move cached tokens *out* of the input
    /// bucket (see `CodexSchema`) a zero rate deletes the bulk of the bill:
    /// cached input is routinely 90%+ of a Codex prompt. The undiscounted
    /// input rate is the conservative choice — it is the same number these
    /// tokens were billed at before the bucket split, so a missing discount
    /// costs us the discount, not the charge.
    fn cache_read_rate(&self) -> f64 {
        self.cache_read_input_token_cost
            .unwrap_or(self.input_cost_per_token)
    }

    fn cost(&self, input: u64, output: u64, cache_create: u64, cache_read: u64) -> f64 {
        (input as f64) * self.input_cost_per_token
            + (output as f64) * self.output_cost_per_token
            + (cache_create as f64) * self.cache_creation_input_token_cost.unwrap_or(0.0)
            + (cache_read as f64) * self.cache_read_rate()
    }
}

/// Cached pricing table. Exact model name match only — no fuzzy matching
/// to avoid mismatched pricing (e.g. opus-4 vs opus-4-6).
pub struct PricingTable {
    prices: HashMap<String, ModelPricing>,
}

/// Walk every provider's fast-multiplier table to resolve `<base>` → multiplier.
/// Returns the first match. Compile-time `const` slices, so iteration is
/// O(total provider rows) with zero runtime allocation.
fn fast_multiplier(base: &str) -> Option<f64> {
    [
        crate::providers::claude_code::FAST_MULTIPLIER,
        crate::providers::codex::FAST_MULTIPLIER,
    ]
    .iter()
    .flat_map(|table| table.iter())
    .find_map(|(k, v)| (*k == base).then_some(*v))
}

impl PricingTable {
    pub fn new(prices: HashMap<String, ModelPricing>) -> Self {
        PricingTable { prices }
    }

    pub fn is_empty(&self) -> bool {
        self.prices.is_empty()
    }

    /// Look up effective pricing for a model name.
    ///
    /// Resolution order:
    /// 1. Direct exact match in LiteLLM data — zero-copy `Cow::Borrowed`.
    ///    Takes precedence if/when LiteLLM publishes `-fast` rows.
    /// 2. `<base>-fast` suffix fallback: base model pricing scaled by the
    ///    per-provider multiplier table — `Cow::Owned` (single allocation).
    pub fn get(&self, model: &str) -> Option<Cow<'_, ModelPricing>> {
        if let Some(p) = self.prices.get(model) {
            return Some(Cow::Borrowed(p));
        }
        let base = model.strip_suffix("-fast")?;
        let mul = fast_multiplier(base)?;
        let base_p = self.prices.get(base)?;
        Some(Cow::Owned(ModelPricing {
            input_cost_per_token: base_p.input_cost_per_token * mul,
            output_cost_per_token: base_p.output_cost_per_token * mul,
            cache_creation_input_token_cost: base_p
                .cache_creation_input_token_cost
                .map(|c| c * mul),
            cache_read_input_token_cost: base_p.cache_read_input_token_cost.map(|c| c * mul),
        }))
    }

    /// Calculate cost for a ModelUsageSummary.
    pub fn summary_cost(&self, s: &ModelUsageSummary) -> Option<f64> {
        self.get(&s.model).map(|p| {
            p.cost(
                s.input_tokens,
                s.output_tokens,
                s.cache_creation_input_tokens,
                s.cache_read_input_tokens,
            )
        })
    }

    pub fn summary_cost_for_schema(
        &self,
        s: &ModelUsageSummary,
        schema: &dyn ProviderSchema,
    ) -> Option<f64> {
        let (input, output, cache_create, cache_read) = schema.billing_tokens(s);
        self.get(&s.model)
            .map(|p| p.cost(input, output, cache_create, cache_read))
    }

    /// Calculate cost for a single UsageEvent.
    pub fn event_cost(&self, e: &UsageEvent) -> Option<f64> {
        self.get(&e.model).map(|p| {
            p.cost(
                e.input_tokens,
                e.output_tokens,
                e.cache_creation_input_tokens,
                e.cache_read_input_tokens,
            )
        })
    }

    /// Calculate cost for a UsageEventWithTs.
    pub fn event_cost_with_ts(&self, e: &crate::common::types::UsageEventWithTs) -> Option<f64> {
        self.get(&e.model).map(|p| {
            p.cost(
                e.input_tokens,
                e.output_tokens,
                e.cache_creation_input_tokens,
                e.cache_read_input_tokens,
            )
        })
    }

    pub fn event_cost_with_ts_for_schema(
        &self,
        e: &crate::common::types::UsageEventWithTs,
        schema: &dyn ProviderSchema,
    ) -> Option<f64> {
        let summary = ModelUsageSummary {
            model: e.model.clone(),
            input_tokens: e.input_tokens,
            output_tokens: e.output_tokens,
            cache_creation_input_tokens: e.cache_creation_input_tokens,
            cache_read_input_tokens: e.cache_read_input_tokens,
            event_count: 0,
            cost_usd: None,
        };
        self.summary_cost_for_schema(&summary, schema)
    }
}

/// Parse LiteLLM JSON and extract all model prices.
/// No provider-specific filtering — any model with valid pricing data is included.
/// Model names are stored both as-is and with common provider prefixes stripped
/// (e.g., "anthropic/claude-opus-4-6" → also stored as "claude-opus-4-6"),
/// so exact-match lookups work regardless of how the CLI tool reports the model name.
fn parse_litellm_json(json_str: &str) -> HashMap<String, ModelPricing> {
    let raw: HashMap<String, LiteLLMEntry> = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };

    let mut prices = HashMap::with_capacity(raw.len());

    for (key, entry) in &raw {
        let (input_cost, output_cost) =
            match (entry.input_cost_per_token, entry.output_cost_per_token) {
                (Some(i), Some(o)) if i > 0.0 || o > 0.0 => (i, o),
                _ => continue,
            };

        let pricing = ModelPricing {
            input_cost_per_token: input_cost,
            output_cost_per_token: output_cost,
            cache_creation_input_token_cost: entry.cache_creation_input_token_cost,
            cache_read_input_token_cost: entry.cache_read_input_token_cost,
        };

        // Filter by litellm_provider field
        let provider = match entry.litellm_provider.as_deref() {
            Some(p) => p,
            None => continue,
        };
        if !SUPPORTED_LITELLM_PROVIDERS.contains(&provider) {
            continue;
        }

        // Strip cloud provider prefix from key (azure/, vertex_ai/, bedrock/, etc.)
        // CLI tools report bare model names (e.g., "claude-opus-4-6", "gpt-5.2-codex")
        let model_name = if key.contains('/') {
            key.rsplit('/').next().unwrap_or(key)
        } else {
            key.as_str()
        };
        if model_name.is_empty() {
            continue;
        }

        prices.entry(model_name.to_string()).or_insert(pricing);
    }

    prices
}

/// Raw entry from LiteLLM JSON (only fields we need).
#[derive(Deserialize)]
struct LiteLLMEntry {
    #[serde(default)]
    litellm_provider: Option<String>,
    #[serde(default)]
    input_cost_per_token: Option<f64>,
    #[serde(default)]
    output_cost_per_token: Option<f64>,
    #[serde(default)]
    cache_creation_input_token_cost: Option<f64>,
    #[serde(default)]
    cache_read_input_token_cost: Option<f64>,
}

/// Providers whose models we track pricing for.
/// Bump PRICING_CACHE_VERSION when adding entries.
const SUPPORTED_LITELLM_PROVIDERS: &[&str] = &["anthropic", "openai", "gemini"];

/// On-disk pricing cache format.
/// Bump this when the parsing logic changes (e.g., adding new provider prefixes).
/// Forces a full re-fetch even if the server returns 304 Not Modified.
const PRICING_CACHE_VERSION: u32 = 5;

#[derive(Serialize, Deserialize)]
struct PricingCache {
    etag: Option<String>,
    #[serde(default)]
    version: u32,
    prices: HashMap<String, ModelPricing>,
}

/// Default pricing cache file path.
pub fn default_cache_path() -> PathBuf {
    let home = crate::config::home_dir();
    home.join(".config").join("toki").join("pricing.json")
}

/// Load cached pricing from file (no network, no DB).
fn load_cache(path: &Path) -> Option<PricingCache> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Save pricing cache to file (atomic write via temp + rename).
fn save_cache(path: &Path, cache: &PricingCache) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = path.with_extension("tmp");
    if let Ok(data) = serde_json::to_string(cache) {
        if std::fs::write(&tmp, &data).is_ok() {
            std::fs::rename(&tmp, path).ok();
        }
    }
}

/// Fetch pricing with ETag-based caching (file-based).
/// Does HTTP conditional request, falls back to cache on error.
/// Returns (PricingTable, cached_etag).
pub fn fetch_pricing(cache_path: &Path) -> PricingTable {
    let cached = load_cache(cache_path);

    // Invalidate cache if parser version changed (e.g., added new provider support)
    let cache_valid = cached
        .as_ref()
        .is_some_and(|c| c.version == PRICING_CACHE_VERSION);

    // Also invalidate etag if cache file is older than 24 hours.
    // LiteLLM's CDN sometimes returns the same etag even when prices change,
    // so we force a full fetch periodically to pick up updates.
    let cache_age_ok = cache_path
        .metadata()
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().is_ok_and(|age| age.as_secs() < 86400))
        .unwrap_or(false);

    let cached_etag = if cache_valid && cache_age_ok {
        cached.as_ref().and_then(|c| c.etag.clone())
    } else {
        None // Force full fetch (version mismatch or stale cache)
    };

    // Note: ureq's into_string() enforces a 10 MB default response size limit,
    // which is sufficient for the LiteLLM pricing JSON (~5 MB as of 2025).
    // No additional size limit needed.
    let mut req = ureq::get(LITELLM_URL);
    if let Some(ref etag) = cached_etag {
        req = req.set("If-None-Match", etag);
    }

    match req.call() {
        Ok(resp) => {
            if resp.status() == 304 {
                // Cache hit — silent
                return cached
                    .map(|c| PricingTable::new(c.prices))
                    .unwrap_or_else(|| PricingTable::new(HashMap::new()));
            }

            let new_etag = resp.header("ETag").map(|s| s.to_string());
            let body = match resp.into_string() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[toki] Pricing: failed to read response body: {}", e);
                    return fallback(cached);
                }
            };

            let prices = parse_litellm_json(&body);
            if prices.is_empty() {
                eprintln!("[toki] Pricing: no supported models found in response");
                return fallback(cached);
            }

            // Updated — silent
            save_cache(
                cache_path,
                &PricingCache {
                    etag: new_etag,
                    version: PRICING_CACHE_VERSION,
                    prices: prices.clone(),
                },
            );
            PricingTable::new(prices)
        }
        Err(ureq::Error::Status(304, _)) => {
            eprintln!("[toki] Pricing: not modified");
            fallback(cached)
        }
        Err(e) => {
            eprintln!("[toki] Pricing: network error ({})", e);
            fallback(cached)
        }
    }
}

/// Load pricing from cache file only (no network).
pub fn load_cached_pricing(cache_path: &Path) -> PricingTable {
    match load_cache(cache_path) {
        Some(cache) => {
            if !cache.prices.is_empty() {
                // Cached data — silent
            }
            PricingTable::new(cache.prices)
        }
        None => PricingTable::new(HashMap::new()),
    }
}

fn fallback(cached: Option<PricingCache>) -> PricingTable {
    match cached {
        Some(cache) => {
            if !cache.prices.is_empty() {
                // Cached data — silent
            }
            PricingTable::new(cache.prices)
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
                "litellm_provider": "anthropic",
                "input_cost_per_token": 0.000003,
                "output_cost_per_token": 0.000015,
                "cache_creation_input_token_cost": 0.00000375,
                "cache_read_input_token_cost": 0.0000003
            },
            "gpt-4": {
                "litellm_provider": "openai",
                "input_cost_per_token": 0.00003,
                "output_cost_per_token": 0.00006
            },
            "anthropic/claude-opus-4-20250514": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": 0.000015,
                "output_cost_per_token": 0.000075,
                "cache_creation_input_token_cost": 0.00001875,
                "cache_read_input_token_cost": 0.0000015
            },
            "gemini/gemini-2.5-pro": {
                "litellm_provider": "gemini",
                "input_cost_per_token": 0.00000125,
                "output_cost_per_token": 0.00001
            },
            "some-free-model": {
                "litellm_provider": "openai",
                "input_cost_per_token": 0.0,
                "output_cost_per_token": 0.0
            },
            "deepseek-v3": {
                "litellm_provider": "deepseek",
                "input_cost_per_token": 0.000001,
                "output_cost_per_token": 0.000002
            }
        }"#;

        let prices = parse_litellm_json(json);
        // Direct keys (no prefix)
        assert!(prices.contains_key("claude-sonnet-4-20250514"));
        assert!(prices.contains_key("gpt-4"));
        // Prefix-stripped keys (anthropic/, gemini/ stripped)
        assert!(prices.contains_key("claude-opus-4-20250514"));
        assert!(prices.contains_key("gemini-2.5-pro"));
        // Original prefixed keys NOT stored (only stripped version)
        assert!(!prices.contains_key("anthropic/claude-opus-4-20250514"));
        assert!(!prices.contains_key("gemini/gemini-2.5-pro"));
        // Zero-cost models excluded
        assert!(!prices.contains_key("some-free-model"));
        // Unsupported providers excluded
        assert!(!prices.contains_key("deepseek-v3"));
    }

    #[test]
    fn test_pricing_table_exact_match() {
        let mut prices = HashMap::new();
        prices.insert(
            "claude-sonnet-4-20250514".to_string(),
            ModelPricing {
                input_cost_per_token: 0.000003,
                output_cost_per_token: 0.000015,
                cache_creation_input_token_cost: Some(0.00000375),
                cache_read_input_token_cost: Some(0.0000003),
            },
        );
        let table = PricingTable::new(prices);
        assert!(table.get("claude-sonnet-4-20250514").is_some());
    }

    #[test]
    fn test_pricing_table_no_match() {
        let mut prices = HashMap::new();
        prices.insert(
            "claude-sonnet-4-20250514".to_string(),
            ModelPricing {
                input_cost_per_token: 0.000003,
                output_cost_per_token: 0.000015,
                cache_creation_input_token_cost: None,
                cache_read_input_token_cost: None,
            },
        );
        let table = PricingTable::new(prices);
        // Different model name → no match (exact only)
        assert!(table.get("claude-sonnet-4-20250601").is_none());
        assert!(table.get("claude-opus-4-20250514").is_none());
    }

    #[test]
    fn test_summary_cost() {
        let mut prices = HashMap::new();
        prices.insert(
            "claude-sonnet-4-20250514".to_string(),
            ModelPricing {
                input_cost_per_token: 0.000003,
                output_cost_per_token: 0.000015,
                cache_creation_input_token_cost: Some(0.00000375),
                cache_read_input_token_cost: Some(0.0000003),
            },
        );
        let table = PricingTable::new(prices);

        let summary = ModelUsageSummary {
            model: "claude-sonnet-4-20250514".to_string(),
            input_tokens: 1000,
            output_tokens: 500,
            cache_creation_input_tokens: 200,
            cache_read_input_tokens: 3000,
            event_count: 5,
            cost_usd: None,
        };
        let cost = table.summary_cost(&summary).unwrap();
        // 1000*0.000003 + 500*0.000015 + 200*0.00000375 + 3000*0.0000003 = 0.01215
        assert!((cost - 0.01215).abs() < 1e-10);
    }

    #[test]
    fn codex_summary_cost_replaces_cached_input_rate() {
        use crate::common::schema::CodexSchema;

        let table = PricingTable::new(HashMap::from([(
            "gpt-test".to_string(),
            ModelPricing {
                input_cost_per_token: 1.0,
                output_cost_per_token: 2.0,
                cache_creation_input_token_cost: Some(10.0),
                cache_read_input_token_cost: Some(0.25),
            },
        )]));
        let summary = ModelUsageSummary {
            model: "gpt-test".to_string(),
            input_tokens: 100,
            output_tokens: 20,
            cache_creation_input_tokens: 10,
            cache_read_input_tokens: 60,
            ..Default::default()
        };

        let cost = table
            .summary_cost_for_schema(&summary, &CodexSchema)
            .unwrap();
        assert_eq!(cost, 95.0);
    }

    #[test]
    fn unpriced_cached_input_is_not_billed_at_zero() {
        use crate::common::schema::CodexSchema;

        // gpt-5-pro and gpt-5.2-pro (among 75 of 224 rows in the real LiteLLM
        // cache) publish no cache_read cost. The codex schema moves cached
        // tokens OUT of the input bucket, so a 0.0 fallback prices 90%+ of a
        // typical prompt at nothing — and these are the most expensive input
        // rates in the table.
        let table = PricingTable::new(HashMap::from([(
            "gpt-pro-test".to_string(),
            ModelPricing {
                input_cost_per_token: 3.0,
                output_cost_per_token: 5.0,
                cache_creation_input_token_cost: None,
                cache_read_input_token_cost: None,
            },
        )]));
        let summary = ModelUsageSummary {
            model: "gpt-pro-test".to_string(),
            input_tokens: 1000,
            output_tokens: 10,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 900,
            ..Default::default()
        };

        // Billed as (1000 - 900) input + 10 output + 900 cached, with cached
        // falling back to the undiscounted input rate: 300 + 50 + 2700.
        let cost = table
            .summary_cost_for_schema(&summary, &CodexSchema)
            .unwrap();
        assert_eq!(cost, 3050.0);

        // The pre-fix formula silently dropped the 900 cached tokens entirely.
        assert_ne!(cost, 350.0);
    }

    #[test]
    fn a_published_cache_read_rate_still_wins_over_the_fallback() {
        let priced = ModelPricing {
            input_cost_per_token: 3.0,
            output_cost_per_token: 5.0,
            cache_creation_input_token_cost: None,
            cache_read_input_token_cost: Some(0.3),
        };
        // The discount is applied, not the input rate.
        assert_eq!(priced.cost(0, 0, 0, 100), 30.0);
    }

    #[test]
    fn test_event_cost() {
        let mut prices = HashMap::new();
        prices.insert(
            "claude-sonnet-4-20250514".to_string(),
            ModelPricing {
                input_cost_per_token: 0.000003,
                output_cost_per_token: 0.000015,
                cache_creation_input_token_cost: None,
                cache_read_input_token_cost: None,
            },
        );
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
    fn test_fast_suffix_direct_match_wins() {
        // If LiteLLM ever ships a "-fast" row directly, it must take precedence
        // over the multiplier fallback.
        let mut prices = HashMap::new();
        prices.insert(
            "claude-opus-4-7".to_string(),
            ModelPricing {
                input_cost_per_token: 0.000005,
                output_cost_per_token: 0.000025,
                cache_creation_input_token_cost: None,
                cache_read_input_token_cost: None,
            },
        );
        prices.insert(
            "claude-opus-4-7-fast".to_string(),
            ModelPricing {
                // Pretend LiteLLM ships a hand-tuned (not 6x) value — must not be
                // overwritten by the fallback table.
                input_cost_per_token: 0.0000777,
                output_cost_per_token: 0.0001111,
                cache_creation_input_token_cost: None,
                cache_read_input_token_cost: None,
            },
        );
        let table = PricingTable::new(prices);
        let p = table.get("claude-opus-4-7-fast").unwrap();
        assert!((p.input_cost_per_token - 0.0000777).abs() < 1e-12);
        assert!((p.output_cost_per_token - 0.0001111).abs() < 1e-12);
    }

    #[test]
    fn test_fast_suffix_multiplier_fallback() {
        // Base model priced, "-fast" row missing → apply the 2x multiplier.
        let mut prices = HashMap::new();
        prices.insert(
            "claude-opus-5".to_string(),
            ModelPricing {
                input_cost_per_token: 0.000005,
                output_cost_per_token: 0.000025,
                cache_creation_input_token_cost: Some(0.00000625),
                cache_read_input_token_cost: Some(0.0000005),
            },
        );
        let table = PricingTable::new(prices);
        let p = table.get("claude-opus-5-fast").unwrap();
        // $10/$50 against a $5/$25 base.
        assert!((p.input_cost_per_token - 0.000010).abs() < 1e-12);
        assert!((p.output_cost_per_token - 0.000050).abs() < 1e-12);
        assert!((p.cache_creation_input_token_cost.unwrap() - 0.00000625 * 2.0).abs() < 1e-12);
        assert!((p.cache_read_input_token_cost.unwrap() - 0.0000005 * 2.0).abs() < 1e-12);
    }

    #[test]
    fn test_direct_match_is_borrowed() {
        // Hot-path guard: direct LiteLLM matches must not allocate.
        let mut prices = HashMap::new();
        prices.insert(
            "claude-opus-4-7".to_string(),
            ModelPricing {
                input_cost_per_token: 0.000005,
                output_cost_per_token: 0.000025,
                cache_creation_input_token_cost: None,
                cache_read_input_token_cost: None,
            },
        );
        let table = PricingTable::new(prices);
        let cow = table.get("claude-opus-4-7").unwrap();
        assert!(matches!(cow, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn test_fast_fallback_is_owned() {
        let mut prices = HashMap::new();
        prices.insert(
            "claude-opus-5".to_string(),
            ModelPricing {
                input_cost_per_token: 0.000005,
                output_cost_per_token: 0.000025,
                cache_creation_input_token_cost: None,
                cache_read_input_token_cost: None,
            },
        );
        let table = PricingTable::new(prices);
        let cow = table.get("claude-opus-5-fast").unwrap();
        assert!(matches!(cow, std::borrow::Cow::Owned(_)));
    }

    #[test]
    fn test_empty_provider_table_safe() {
        // Codex's FAST_MULTIPLIER is an empty placeholder; the resolver
        // must still find Claude entries from the other provider's table.
        assert_eq!(fast_multiplier("claude-opus-5"), Some(2.0));
        // 4.7 rejects fast requests and 4.6 runs at standard rates, so
        // neither may carry a multiplier.
        assert_eq!(fast_multiplier("claude-opus-4-7"), None);
        assert_eq!(fast_multiplier("claude-opus-4-6"), None);
        // And an unknown base must yield None without panicking on the
        // empty Codex slice.
        assert_eq!(fast_multiplier("gpt-5.5"), None);
    }

    #[test]
    fn test_fast_suffix_unknown_base_returns_none() {
        // "-fast" suffix but base model not in multiplier table → no fallback.
        let mut prices = HashMap::new();
        prices.insert(
            "claude-sonnet-4-5".to_string(),
            ModelPricing {
                input_cost_per_token: 0.000003,
                output_cost_per_token: 0.000015,
                cache_creation_input_token_cost: None,
                cache_read_input_token_cost: None,
            },
        );
        let table = PricingTable::new(prices);
        // Sonnet isn't in FAST_MULTIPLIER → no fallback, even though base exists.
        assert!(table.get("claude-sonnet-4-5-fast").is_none());
    }

    #[test]
    fn test_format_cost() {
        assert_eq!(format_cost(Some(1.5)), "$1.50");
        assert_eq!(format_cost(Some(0.005)), "$0.0050");
        assert_eq!(format_cost(Some(0.0)), "$0.0000");
        assert_eq!(format_cost(None), "-");
    }
}
