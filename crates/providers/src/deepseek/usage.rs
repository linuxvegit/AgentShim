//! DeepSeek-specific usage mapping.
//!
//! DeepSeek's `/chat/completions` `usage` block reports prompt-cache hit and
//! miss counts as separate fields:
//!
//! - `prompt_cache_hit_tokens`  — tokens served from the prompt cache.
//! - `prompt_cache_miss_tokens` — *new* prompt tokens (the not-cached portion).
//!
//! The canonical [`Usage`] struct splits cache fields differently:
//!
//! - `input_tokens` carries the new (non-cached) prompt portion.
//! - `cache_read_input_tokens` carries the hit count.
//! - `cache_creation_input_tokens` is reserved for providers that report cache
//!   creation explicitly (Anthropic). DeepSeek doesn't, so this stays `None`.
//!
//! When DeepSeek omits the cache fields (older models, error responses), we
//! fall back to `prompt_tokens` for `input_tokens` so the canonical shape is
//! still populated. The full upstream `usage` JSON is preserved verbatim in
//! `provider_raw` for downstream consumers that want the raw counts.
//!
//! Reasoning tokens are not separately reported by DeepSeek today; the field
//! stays `None`. Future work — and a future field on the upstream payload —
//! may light it up.

use agent_shim_core::Usage;
use serde_json::Value;

/// Map DeepSeek's `usage` JSON object into the canonical [`Usage`] shape.
///
/// `raw` should be the inner `usage` value from DeepSeek's response (i.e. the
/// object that contains `prompt_tokens`, `completion_tokens`, and the optional
/// `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`).
pub(crate) fn map_usage(raw: &Value) -> Usage {
    let prompt_tokens = raw
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let completion_tokens = raw
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let cache_hit = raw
        .get("prompt_cache_hit_tokens")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let cache_miss = raw
        .get("prompt_cache_miss_tokens")
        .and_then(Value::as_u64)
        .map(|n| n as u32);

    Usage {
        // input_tokens reflects the *new* (non-cached) prompt input. When the
        // miss field is absent (older payloads), fall back to prompt_tokens so
        // the canonical shape is still populated.
        input_tokens: cache_miss.or(prompt_tokens),
        output_tokens: completion_tokens,
        // DeepSeek has no concept of cache creation.
        cache_creation_input_tokens: None,
        cache_read_input_tokens: cache_hit,
        // DeepSeek doesn't separately report reasoning tokens today.
        reasoning_tokens: None,
        estimated: false,
        provider_raw: Some(raw.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn map_usage_with_cache_hit_and_miss() {
        let raw = json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "prompt_cache_hit_tokens": 80,
            "prompt_cache_miss_tokens": 20,
            "total_tokens": 150
        });

        let usage = map_usage(&raw);

        // input_tokens == miss (new prompt portion), NOT total prompt.
        assert_eq!(usage.input_tokens, Some(20));
        assert_eq!(usage.output_tokens, Some(50));
        assert_eq!(usage.cache_creation_input_tokens, None);
        assert_eq!(usage.cache_read_input_tokens, Some(80));
        assert_eq!(usage.reasoning_tokens, None);
        assert!(!usage.estimated);
        assert_eq!(usage.provider_raw, Some(raw));
    }

    #[test]
    fn map_usage_with_only_prompt_tokens() {
        // Older DeepSeek payloads (or error envelopes) may omit cache fields.
        let raw = json!({
            "prompt_tokens": 100,
            "completion_tokens": 50
        });

        let usage = map_usage(&raw);

        // Falls back to prompt_tokens when cache_miss is absent.
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(50));
        assert_eq!(usage.cache_creation_input_tokens, None);
        assert_eq!(usage.cache_read_input_tokens, None);
        assert_eq!(usage.reasoning_tokens, None);
        assert!(!usage.estimated);
        assert_eq!(usage.provider_raw, Some(raw));
    }

    #[test]
    fn map_usage_with_missing_fields_returns_none() {
        let raw = json!({});

        let usage = map_usage(&raw);

        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, None);
        assert_eq!(usage.cache_creation_input_tokens, None);
        assert_eq!(usage.cache_read_input_tokens, None);
        assert_eq!(usage.reasoning_tokens, None);
        assert!(!usage.estimated);
        // provider_raw still preserves the empty object verbatim.
        assert_eq!(usage.provider_raw, Some(raw));
    }

    #[test]
    fn map_usage_with_zero_cache_hit_preserves_zero() {
        // Edge case: cache_hit_tokens explicitly 0. Not the same as None — a
        // recorded zero means "we asked the cache and got nothing", which is
        // observable signal worth preserving for analytics.
        let raw = json!({
            "prompt_tokens": 100,
            "completion_tokens": 25,
            "prompt_cache_hit_tokens": 0,
            "prompt_cache_miss_tokens": 100
        });

        let usage = map_usage(&raw);

        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(25));
        assert_eq!(usage.cache_read_input_tokens, Some(0));
    }

    #[test]
    fn map_usage_preserves_provider_raw_verbatim() {
        // Anything in the raw blob should be preserved — including upstream
        // fields the canonical shape doesn't model (e.g. `total_tokens`).
        let raw = json!({
            "prompt_tokens": 1,
            "completion_tokens": 2,
            "total_tokens": 3,
            "experimental_field": "future-deepseek-quirk"
        });

        let usage = map_usage(&raw);

        assert_eq!(usage.provider_raw, Some(raw));
    }
}
