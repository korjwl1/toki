use crate::common::types::ModelUsageSummary;

/// Describes a single token column in table/JSON output.
pub struct TokenColumn {
    pub header: &'static str,
    pub json_key: &'static str,
}

/// Provider-specific schema for token columns and extraction.
pub trait ProviderSchema: Send + Sync {
    fn columns(&self) -> &[TokenColumn];
    fn provider_name(&self) -> &str;
    /// Extract token values from ModelUsageSummary in column order.
    fn extract_tokens(&self, s: &ModelUsageSummary) -> Vec<u64>;
    /// Compute total tokens from a ModelUsageSummary.
    fn total_tokens(&self, s: &ModelUsageSummary) -> u64;
}

// ── Claude Code schema ──────────────────────────────────────────────────────

static CLAUDE_CODE_COLUMNS: &[TokenColumn] = &[
    TokenColumn { header: "Input",          json_key: "input_tokens" },
    TokenColumn { header: "Output",         json_key: "output_tokens" },
    TokenColumn { header: "Cache\nCreate",  json_key: "cache_creation_input_tokens" },
    TokenColumn { header: "Cache\nRead",    json_key: "cache_read_input_tokens" },
];

pub struct ClaudeCodeSchema;

impl ProviderSchema for ClaudeCodeSchema {
    fn columns(&self) -> &[TokenColumn] { CLAUDE_CODE_COLUMNS }
    fn provider_name(&self) -> &str { "claude_code" }

    fn extract_tokens(&self, s: &ModelUsageSummary) -> Vec<u64> {
        vec![
            s.input_tokens,
            s.output_tokens,
            s.cache_creation_input_tokens,
            s.cache_read_input_tokens,
        ]
    }

    fn total_tokens(&self, s: &ModelUsageSummary) -> u64 {
        s.input_tokens + s.output_tokens + s.cache_creation_input_tokens + s.cache_read_input_tokens
    }
}

// ── Codex schema ────────────────────────────────────────────────────────────

static CODEX_COLUMNS: &[TokenColumn] = &[
    TokenColumn { header: "Input",            json_key: "input_tokens" },
    TokenColumn { header: "Output",           json_key: "output_tokens" },
    TokenColumn { header: "Cached\nInput",    json_key: "cached_input_tokens" },
    TokenColumn { header: "Reasoning\nOutput", json_key: "reasoning_output_tokens" },
];

pub struct CodexSchema;

impl ProviderSchema for CodexSchema {
    fn columns(&self) -> &[TokenColumn] { CODEX_COLUMNS }
    fn provider_name(&self) -> &str { "codex" }

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
        s.input_tokens + s.output_tokens + s.cache_read_input_tokens + s.cache_creation_input_tokens
    }
}

// ── Combined (cross-provider merge) schema ──────────────────────────────────

static COMBINED_COLUMNS: &[TokenColumn] = &[
    TokenColumn { header: "Input",  json_key: "input_tokens" },
    TokenColumn { header: "Output", json_key: "output_tokens" },
];

pub struct CombinedSchema;

impl ProviderSchema for CombinedSchema {
    fn columns(&self) -> &[TokenColumn] { COMBINED_COLUMNS }
    fn provider_name(&self) -> &str { "combined" }

    fn extract_tokens(&self, s: &ModelUsageSummary) -> Vec<u64> {
        vec![s.input_tokens, s.output_tokens]
    }

    fn total_tokens(&self, s: &ModelUsageSummary) -> u64 {
        s.input_tokens + s.output_tokens
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
    fn test_combined_schema_columns() {
        let schema = CombinedSchema;
        assert_eq!(schema.columns().len(), 2);
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
        assert_eq!(schema.total_tokens(&s), 750);
    }

    #[test]
    fn test_schema_for_provider() {
        assert_eq!(schema_for_provider("codex").provider_name(), "codex");
        assert_eq!(schema_for_provider("claude_code").provider_name(), "claude_code");
        assert_eq!(schema_for_provider("unknown").provider_name(), "claude_code");
    }
}
