//! Per-route defaults and the merged per-request snapshot.
//!
//! A [`RoutePolicy`] is configured per route in `gateway.yaml` and lives on
//! [`crate::BackendTarget`]. It carries values that should apply when the
//! inbound request didn't supply its own — today reasoning effort and the
//! `anthropic-beta` header, but the shape is intended to absorb future
//! per-route knobs without widening `BackendTarget`'s interface.
//!
//! [`RoutePolicy::resolve`] is a pure function: it merges inbound request data
//! with the route defaults and returns a [`ResolvedPolicy`] snapshot. Providers
//! read the snapshot; they never consult the policy or the inbound request
//! directly. This keeps the merge rule ("inbound wins, else route default,
//! else nothing") in a single place and makes it independently testable.

use serde::{Deserialize, Serialize};

use crate::request::{CanonicalRequest, ReasoningEffort};

/// Per-route defaults applied when the inbound request didn't specify a value.
///
/// Construct from gateway config; attach to [`crate::BackendTarget`]. Add new
/// fields here rather than on `BackendTarget` so the routing surface stays
/// small and merge logic stays co-located with the data it merges.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutePolicy {
    /// Default reasoning effort to apply when the request didn't ask for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<ReasoningEffort>,
    /// Default `anthropic-beta` header value to apply when the request didn't
    /// supply one. Used to enable beta features like the 1M context window
    /// (`context-1m-2025-08-07`) without baking them into the model name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_anthropic_beta: Option<String>,
}

/// The merged per-request view of a [`RoutePolicy`].
///
/// Built once per request by [`RoutePolicy::resolve`] and stored on
/// [`CanonicalRequest::resolved_policy`]. Providers read from this; logging
/// reads from this; no one else should be re-deriving the merge rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPolicy {
    /// Final reasoning effort to forward to the upstream, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Final `anthropic-*` headers to attach to the upstream HTTP request.
    /// Order is stable; duplicates are not collapsed (callers may pass
    /// comma-separated values verbatim).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anthropic_headers: Vec<(String, String)>,
}

impl ResolvedPolicy {
    /// Look up an `anthropic-*` header value by name (case-insensitive).
    pub fn anthropic_header(&self, name: &str) -> Option<&str> {
        self.anthropic_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl RoutePolicy {
    /// Compute the per-request snapshot.
    ///
    /// Inbound request data wins over route defaults. The resulting
    /// [`ResolvedPolicy`] is what providers actually apply.
    pub fn resolve(&self, req: &CanonicalRequest) -> ResolvedPolicy {
        let reasoning_effort = req
            .generation
            .reasoning
            .as_ref()
            .and_then(|r| r.effort)
            .or(self.default_reasoning_effort);

        let mut anthropic_headers = req.inbound_anthropic_headers.clone();
        let inbound_has_beta = anthropic_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"));
        if !inbound_has_beta {
            if let Some(beta) = &self.default_anthropic_beta {
                anthropic_headers.push(("anthropic-beta".to_string(), beta.clone()));
            }
        }

        ResolvedPolicy {
            reasoning_effort,
            anthropic_headers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{GenerationOptions, ReasoningOptions, RequestMetadata};
    use crate::target::FrontendInfo;
    use crate::{ExtensionMap, FrontendKind, FrontendModel, RequestId};

    fn req() -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: FrontendModel::from("m"),
            },
            model: FrontendModel::from("m"),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: Default::default(),
            generation: GenerationOptions::default(),
            response_format: None,
            stream: false,
            metadata: RequestMetadata::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: ResolvedPolicy::default(),
            extensions: ExtensionMap::new(),
        }
    }

    #[test]
    fn empty_policy_resolves_to_empty() {
        let policy = RoutePolicy::default();
        let resolved = policy.resolve(&req());
        assert!(resolved.reasoning_effort.is_none());
        assert!(resolved.anthropic_headers.is_empty());
    }

    #[test]
    fn route_default_reasoning_applies_when_request_silent() {
        let policy = RoutePolicy {
            default_reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        assert_eq!(
            policy.resolve(&req()).reasoning_effort,
            Some(ReasoningEffort::High)
        );
    }

    #[test]
    fn inbound_reasoning_wins_over_route_default() {
        let policy = RoutePolicy {
            default_reasoning_effort: Some(ReasoningEffort::Minimal),
            ..Default::default()
        };
        let mut r = req();
        r.generation.reasoning = Some(ReasoningOptions {
            effort: Some(ReasoningEffort::Xhigh),
            budget_tokens: None,
        });
        assert_eq!(
            policy.resolve(&r).reasoning_effort,
            Some(ReasoningEffort::Xhigh)
        );
    }

    #[test]
    fn route_default_beta_applies_when_no_inbound_beta() {
        let policy = RoutePolicy {
            default_anthropic_beta: Some("context-1m-2025-08-07".into()),
            ..Default::default()
        };
        let resolved = policy.resolve(&req());
        assert_eq!(
            resolved.anthropic_header("anthropic-beta"),
            Some("context-1m-2025-08-07")
        );
    }

    #[test]
    fn inbound_beta_wins_over_route_default() {
        let policy = RoutePolicy {
            default_anthropic_beta: Some("default-flag".into()),
            ..Default::default()
        };
        let mut r = req();
        r.inbound_anthropic_headers
            .push(("anthropic-beta".into(), "agent-flag".into()));
        let resolved = policy.resolve(&r);
        assert_eq!(
            resolved.anthropic_header("anthropic-beta"),
            Some("agent-flag")
        );
    }

    #[test]
    fn other_anthropic_headers_pass_through_alongside_route_default() {
        let policy = RoutePolicy {
            default_anthropic_beta: Some("ctx-1m".into()),
            ..Default::default()
        };
        let mut r = req();
        r.inbound_anthropic_headers
            .push(("anthropic-version".into(), "2023-06-01".into()));
        let resolved = policy.resolve(&r);
        // Both the inbound version header and the route-default beta survive.
        assert_eq!(
            resolved.anthropic_header("anthropic-version"),
            Some("2023-06-01")
        );
        assert_eq!(resolved.anthropic_header("anthropic-beta"), Some("ctx-1m"));
    }
}
