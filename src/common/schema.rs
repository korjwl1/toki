use crate::common::types::ModelUsageSummary;

/// Describes a single token column in table/JSON output.
pub struct TokenColumn {
    pub header: &'static str,
    pub json_key: &'static str,
    /// Short label for compact event output (e.g., "in", "out", "cc", "cr")
    pub short: &'static str,
}

/// Provider-specific schema for token columns and extraction.
pub trait ProviderSchema: Send + Sync {
    fn columns(&self) -> &[TokenColumn];
    fn provider_name(&self) -> &str;
    /// Extract token values from ModelUsageSummary in column order.
    /// Returns Vec<u64>. Only called during rendering (display-only, cold path).
    fn extract_tokens(&self, s: &ModelUsageSummary) -> Vec<u64>;
    /// Compute total tokens from a ModelUsageSummary.
    fn total_tokens(&self, s: &ModelUsageSummary) -> u64;
    /// Return mutually exclusive billable input/output/cache buckets.
    fn billing_tokens(&self, s: &ModelUsageSummary) -> (u64, u64, u64, u64) {
        (
            s.input_tokens,
            s.output_tokens,
            s.cache_creation_input_tokens,
            s.cache_read_input_tokens,
        )
    }
}

// ── Claude Code schema ──────────────────────────────────────────────────────

static CLAUDE_CODE_COLUMNS: &[TokenColumn] = &[
    TokenColumn { header: "Input",          json_key: "input_tokens",                  short: "in" },
    TokenColumn { header: "Output",         json_key: "output_tokens",                 short: "out" },
    TokenColumn { header: "Cache\nCreate",  json_key: "cache_creation_input_tokens",   short: "cc" },
    TokenColumn { header: "Cache\nRead",    json_key: "cache_read_input_tokens",       short: "cr" },
];

pub struct ClaudeCodeSchema;

impl ProviderSchema for ClaudeCodeSchema {
    fn columns(&self) -> &[TokenColumn] { CLAUDE_CODE_COLUMNS }
    fn provider_name(&self) -> &str { "Claude Code" }

    fn extract_tokens(&self, s: &ModelUsageSummary) -> Vec<u64> {
        vec![
            s.input_tokens,
            s.output_tokens,
            s.cache_creation_input_tokens,
            s.cache_read_input_tokens,
        ]
    }

    fn total_tokens(&self, s: &ModelUsageSummary) -> u64 {
        s.input_tokens
            .saturating_add(s.output_tokens)
            .saturating_add(s.cache_creation_input_tokens)
            .saturating_add(s.cache_read_input_tokens)
    }
}

// ── Codex schema ────────────────────────────────────────────────────────────

static CODEX_COLUMNS: &[TokenColumn] = &[
    TokenColumn { header: "Input",            json_key: "input_tokens",              short: "in" },
    TokenColumn { header: "Output",           json_key: "output_tokens",             short: "out" },
    TokenColumn { header: "Cached\nInput",    json_key: "cached_input_tokens",       short: "ci" },
    TokenColumn { header: "Reasoning\nOutput", json_key: "reasoning_output_tokens",  short: "ro" },
];

pub struct CodexSchema;

impl ProviderSchema for CodexSchema {
    fn columns(&self) -> &[TokenColumn] { CODEX_COLUMNS }
    fn provider_name(&self) -> &str { "Codex CLI" }

    fn extract_tokens(&self, s: &ModelUsageSummary) -> Vec<u64> {
        // slot 3 = cache_creation_input_tokens = reasoning_output_tokens
        // slot 4 = cache_read_input_tokens = cached_input_tokens
        vec![
            s.input_tokens,
            s.output_tokens,
            s.cache_read_input_tokens,            // cached input
            s.cache_creation_input_tokens,         // reasoning output
        ]
    }

    fn total_tokens(&self, s: &ModelUsageSummary) -> u64 {
        // cached_input ⊂ input, reasoning_output ⊂ output — don't double-count
        s.input_tokens.saturating_add(s.output_tokens)
    }

    fn billing_tokens(&self, s: &ModelUsageSummary) -> (u64, u64, u64, u64) {
        // LiteLLM's cache-read rate replaces the normal input rate for cached
        // tokens. Reasoning is already included in output and has no separate
        // billing bucket.
        (
            s.input_tokens.saturating_sub(s.cache_read_input_tokens),
            s.output_tokens,
            0,
            s.cache_read_input_tokens,
        )
    }
}

/// Return the appropriate schema for a given provider name.
pub fn schema_for_provider(name: &str) -> &'static dyn ProviderSchema {
    match name {
        "codex" => &CodexSchema,
        "claude_code" => &ClaudeCodeSchema,
        _ => &ClaudeCodeSchema,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_code_schema_columns() {
        let schema = ClaudeCodeSchema;
        assert_eq!(schema.columns().len(), 4);
        assert_eq!(schema.columns()[0].json_key, "input_tokens");
        assert_eq!(schema.columns()[3].json_key, "cache_read_input_tokens");
    }

    #[test]
    fn test_codex_schema_columns() {
        let schema = CodexSchema;
        assert_eq!(schema.columns().len(), 4);
        assert_eq!(schema.columns()[2].json_key, "cached_input_tokens");
        assert_eq!(schema.columns()[3].json_key, "reasoning_output_tokens");
    }

    #[test]
    fn codex_billing_tokens_do_not_double_count_subsets() {
        let schema = CodexSchema;
        let summary = ModelUsageSummary {
            input_tokens: 100,
            output_tokens: 20,
            cache_creation_input_tokens: 10,
            cache_read_input_tokens: 60,
            ..Default::default()
        };
        assert_eq!(schema.billing_tokens(&summary), (40, 20, 0, 60));
    }

    #[test]
    fn claude_billing_tokens_keep_the_four_buckets_disjoint() {
        // Anthropic and OpenAI use OPPOSITE conventions, and nothing pinned
        // the Anthropic side: cache_creation_input_tokens and
        // cache_read_input_tokens are separate, separately-priced fields that
        // are NOT contained in input_tokens. Applying the codex subtraction
        // here would under-bill every Claude event, and would have passed the
        // entire suite before this test existed.
        let schema = ClaudeCodeSchema;
        let summary = ModelUsageSummary {
            input_tokens: 100,
            output_tokens: 20,
            cache_creation_input_tokens: 10,
            cache_read_input_tokens: 60,
            ..Default::default()
        };
        assert_eq!(schema.billing_tokens(&summary), (100, 20, 10, 60));

        // Stated as the invariant rather than the numbers: the input bucket is
        // passed through untouched, and the cache buckets are billed in full.
        let (input, output, cache_create, cache_read) = schema.billing_tokens(&summary);
        assert_eq!(input, summary.input_tokens);
        assert_eq!(output, summary.output_tokens);
        assert_eq!(cache_create, summary.cache_creation_input_tokens);
        assert_eq!(cache_read, summary.cache_read_input_tokens);
    }

    #[test]
    fn the_two_providers_do_not_share_a_billing_convention() {
        // A single summary must bill differently under each schema. If a
        // future edit collapses them into one rule, this fails loudly rather
        // than silently moving everyone's numbers.
        let summary = ModelUsageSummary {
            input_tokens: 100,
            output_tokens: 20,
            cache_creation_input_tokens: 10,
            cache_read_input_tokens: 60,
            ..Default::default()
        };
        assert_ne!(
            ClaudeCodeSchema.billing_tokens(&summary),
            CodexSchema.billing_tokens(&summary)
        );
    }

    #[test]
    fn displayed_totals_saturate_instead_of_wrapping() {
        let summary = ModelUsageSummary {
            input_tokens: u64::MAX,
            output_tokens: 1,
            cache_creation_input_tokens: 1,
            cache_read_input_tokens: 1,
            ..Default::default()
        };
        assert_eq!(ClaudeCodeSchema.total_tokens(&summary), u64::MAX);
        assert_eq!(CodexSchema.total_tokens(&summary), u64::MAX);
    }

    #[test]
    fn test_extract_tokens_claude_code() {
        let s = ModelUsageSummary {
            model: "test".to_string(),
            input_tokens: 100,
            output_tokens: 200,
            cache_creation_input_tokens: 300,
            cache_read_input_tokens: 400,
            event_count: 1,
            cost_usd: None,
        };
        let schema = ClaudeCodeSchema;
        assert_eq!(schema.extract_tokens(&s), vec![100, 200, 300, 400]);
        assert_eq!(schema.total_tokens(&s), 1000);
    }

    #[test]
    fn test_extract_tokens_codex() {
        let s = ModelUsageSummary {
            model: "test".to_string(),
            input_tokens: 100,
            output_tokens: 200,
            cache_creation_input_tokens: 50,   // reasoning output in slot 3
            cache_read_input_tokens: 400,       // cached input in slot 4
            event_count: 1,
            cost_usd: None,
        };
        let schema = CodexSchema;
        // Codex: [input, output, cached_input (slot4), reasoning (slot3)]
        assert_eq!(schema.extract_tokens(&s), vec![100, 200, 400, 50]);
        // total = input + output only (cached_input ⊂ input, reasoning ⊂ output)
        assert_eq!(schema.total_tokens(&s), 300);
    }

    #[test]
    fn test_schema_for_provider() {
        assert_eq!(schema_for_provider("codex").provider_name(), "Codex CLI");
        assert_eq!(schema_for_provider("claude_code").provider_name(), "Claude Code");
        assert_eq!(schema_for_provider("unknown").provider_name(), "Claude Code");
    }
}
