//! Model catalog types — the composed view of "what aliases AgentShim accepts"
//! plus the upstream-discovered capabilities for each alias's backend target.
//!
//! Lives in core because it's read by router (which builds it from routes +
//! model index), by gateway handlers (which serve it on /v1/models and
//! /admin/catalog), and is part of the public surface (so changes here are
//! breaking changes for clients).

use crate::FrontendKind;
use serde::{Deserialize, Serialize};

/// One entry in the catalog returned to clients / operators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRecord {
    /// The route alias (what clients put in `model:`).
    pub id: String,
    /// All frontends on which this alias is accepted.
    pub frontends: Vec<FrontendKind>,
    /// Head of the fallback chain (the primary upstream).
    pub upstream_provider: String,
    pub upstream_model: String,
    /// Full chain (length 1 for singular routes, N for `upstreams: [...]`).
    pub upstreams_chain: Vec<UpstreamRef>,
    /// Upstream-discovered metadata. `None` when discovery didn't run or the
    /// provider doesn't publish a catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ModelMetadata>,
    /// Same-family alias on this gateway with a strictly larger context
    /// window, if any. Surfaces "you might want -1m" without forcing
    /// dynamic SKU selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_context_variant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamRef {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default)]
    pub supports: ModelSupports,
    /// The upstream API endpoints this model accepts, copied verbatim from
    /// the provider catalog (e.g. Copilot's per-model `supported_endpoints`:
    /// `["/responses", "ws:/responses"]` for responses-only models,
    /// `["/v1/messages", "/chat/completions"]` for chat-only). Drives the
    /// Copilot provider's endpoint selection so a responses-only model
    /// (e.g. `gpt-5.5`) is reached via `/v1/responses` regardless of the
    /// inbound frontend dialect. `None` when the provider doesn't surface
    /// the field — callers then fall back to frontend-driven selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_endpoints: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Whether the upstream's "adaptive thinking" mode is available
    /// (Anthropic family on Copilot exposes `adaptive_thinking: true`
    /// alongside a budget range; `None` for upstreams that don't
    /// surface the field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
    /// Minimum tokens the upstream accepts for a thinking budget.
    /// `None` when the model doesn't take a numeric budget at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budget_min: Option<u32>,
    /// Maximum tokens the upstream accepts for a thinking budget.
    /// `None` when the model doesn't take a numeric budget at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budget_max: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelSupports {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_outputs: Option<bool>,
    /// Bool form: whether the model accepts a reasoning-effort knob at
    /// all. Derived by upstream parsers — typically `Some(true)` when
    /// `reasoning_effort_values` is a non-empty list, `Some(false)`
    /// when the upstream explicitly says so, `None` when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<bool>,
    /// The list of effort strings the upstream advertises (e.g.
    /// `["low","medium","high","xhigh"]`). Empty / missing when the
    /// upstream doesn't surface the list; the bool above remains the
    /// authoritative "does it accept any" signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort_values: Option<Vec<String>>,
}

pub type ModelCatalog = Vec<ModelRecord>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_serialises_with_no_metadata() {
        let r = ModelRecord {
            id: "x".into(),
            frontends: vec![FrontendKind::AnthropicMessages],
            upstream_provider: "u".into(),
            upstream_model: "um".into(),
            upstreams_chain: vec![UpstreamRef {
                provider: "u".into(),
                model: "um".into(),
            }],
            metadata: None,
            long_context_variant: None,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["id"], "x");
        assert!(json.get("metadata").is_none());
        assert!(json.get("long_context_variant").is_none());
    }

    #[test]
    fn metadata_round_trips() {
        let m = ModelMetadata {
            context_window_tokens: Some(200000),
            max_output_tokens: Some(32000),
            family: Some("claude-opus-4.7".into()),
            supported_endpoints: Some(vec!["/v1/messages".into(), "/chat/completions".into()]),
            supports: ModelSupports {
                vision: Some(true),
                tool_calls: Some(true),
                streaming: Some(true),
                structured_outputs: None,
                reasoning_effort: Some(true),
                reasoning_effort_values: Some(vec!["low".into(), "medium".into(), "high".into()]),
            },
            version: None,
            adaptive_thinking: Some(true),
            thinking_budget_min: Some(1024),
            thinking_budget_max: Some(32000),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ModelMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }
}
