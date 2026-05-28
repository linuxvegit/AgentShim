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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<bool>,
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
            supports: ModelSupports {
                vision: Some(true),
                tool_calls: Some(true),
                streaming: Some(true),
                structured_outputs: None,
                reasoning_effort: Some(true),
            },
            version: None,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ModelMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }
}
