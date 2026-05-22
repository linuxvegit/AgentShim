#![cfg(feature = "prompt_compressor")]

use serde::Deserialize;

/// Top-level configuration for the `prompt_compressor` plugin.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptCompressorConfig {
    pub strategy: Strategy,
}

/// Selects which compression algorithm to apply.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Strategy {
    DropOldTurns(DropOldTurnsConfig),
    TruncateToTokens(TruncateToTokensConfig),
    SummarizeOldTurns(SummarizeOldTurnsConfig),
}

/// Drop the oldest conversation turns, keeping the last `keep_last_n`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DropOldTurnsConfig {
    pub keep_last_n: usize,
}

/// Truncate conversation to fit within a token budget, always keeping the
/// last `keep_last_n` turns intact.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TruncateToTokensConfig {
    pub target_tokens: u32,
    pub keep_last_n: usize,
}

/// Replace old turns with an LLM-generated summary.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummarizeOldTurnsConfig {
    pub keep_last_n: usize,
    pub summarizer: SummarizerConfig,
}

/// Configuration for the upstream summarizer model.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummarizerConfig {
    /// Name of an upstream registered in the gateway config.
    pub upstream: String,
    /// Model identifier to pass to that upstream.
    pub model: String,
    /// Maximum tokens the summary may use (default 300).
    #[serde(default = "default_max_summary_tokens")]
    pub max_summary_tokens: u32,
    /// Request timeout in milliseconds (default 5000).
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_max_summary_tokens() -> u32 {
    300
}

fn default_timeout_ms() -> u64 {
    5_000
}

// ─── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_drop_old_turns() {
        let yaml = r#"
strategy:
  type: drop_old_turns
  keep_last_n: 10
"#;
        let cfg: PromptCompressorConfig = serde_yaml::from_str(yaml).unwrap();
        match cfg.strategy {
            Strategy::DropOldTurns(c) => assert_eq!(c.keep_last_n, 10),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn round_trip_truncate_to_tokens() {
        let yaml = r#"
strategy:
  type: truncate_to_tokens
  target_tokens: 4096
  keep_last_n: 4
"#;
        let cfg: PromptCompressorConfig = serde_yaml::from_str(yaml).unwrap();
        match cfg.strategy {
            Strategy::TruncateToTokens(c) => {
                assert_eq!(c.target_tokens, 4096);
                assert_eq!(c.keep_last_n, 4);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn round_trip_summarize_old_turns() {
        let yaml = r#"
strategy:
  type: summarize_old_turns
  keep_last_n: 6
  summarizer:
    upstream: openai
    model: gpt-4o-mini
"#;
        let cfg: PromptCompressorConfig = serde_yaml::from_str(yaml).unwrap();
        match cfg.strategy {
            Strategy::SummarizeOldTurns(c) => {
                assert_eq!(c.keep_last_n, 6);
                assert_eq!(c.summarizer.upstream, "openai");
                assert_eq!(c.summarizer.model, "gpt-4o-mini");
                assert_eq!(c.summarizer.max_summary_tokens, 300);
                assert_eq!(c.summarizer.timeout_ms, 5_000);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn reject_unknown_strategy_type() {
        let yaml = r#"
strategy:
  type: magic_compress
  keep_last_n: 5
"#;
        assert!(serde_yaml::from_str::<PromptCompressorConfig>(yaml).is_err());
    }

    #[test]
    fn reject_unknown_field_in_summarizer() {
        let yaml = r#"
strategy:
  type: summarize_old_turns
  keep_last_n: 3
  summarizer:
    upstream: openai
    model: gpt-4o-mini
    unknown_field: oops
"#;
        assert!(serde_yaml::from_str::<PromptCompressorConfig>(yaml).is_err());
    }
}
