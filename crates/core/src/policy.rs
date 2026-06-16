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
    /// Per-route ordered rewrite table for canonical reasoning effort. Each
    /// rule maps an inbound canonical effort (post route-default fallback) to
    /// an outbound canonical effort. First rule whose `match` equals the
    /// post-default effort wins; unmatched passes through. Both `match` and
    /// `set` are canonical-vocabulary effort values, NOT dialect strings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_mapping: Vec<MappingRule>,
}

/// One entry in a [`RoutePolicy::reasoning_mapping`] table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingRule {
    /// Inbound canonical effort the rule fires on.
    pub r#match: ReasoningEffort,
    /// Outbound canonical effort the rule rewrites to.
    pub set: ReasoningEffort,
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
    /// Final reasoning token budget to forward, if any. This is the
    /// quantitative axis still used by Gemini and by the legacy Anthropic
    /// `thinking.budget_tokens` shape. It is independent of `reasoning_effort`
    /// — both may be `Some` (effort drives the effort dialect; budget drives
    /// any provider that quantizes from a number).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_budget_tokens: Option<u32>,
    /// The target model's advertised `reasoning_effort` vocabulary, copied
    /// from discovered catalog metadata at route-resolution time (e.g.
    /// `["low","medium","high","xhigh","max"]`). Encoders that emit an
    /// effort string clamp `reasoning_effort` against this list via
    /// [`ReasoningEffort::clamp_to_advertised`], so per-model differences
    /// (opus-4.8 takes `max`; opus-4.6 takes `max` but not `xhigh`;
    /// gpt-5.5 takes `xhigh` but not `max`) are honoured instead of squashed
    /// to a single provider-wide ceiling. `None` when the catalog had no
    /// metadata for this model — encoders then fall back to their static
    /// per-provider compression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_efforts: Option<Vec<String>>,
    /// The target model's advertised upstream API endpoints, copied from
    /// discovered catalog metadata at route-resolution time (e.g. Copilot's
    /// `["/responses", "ws:/responses"]` for a responses-only model). The
    /// Copilot provider reads this to pick `/v1/responses` vs
    /// `/chat/completions` by capability rather than by inbound frontend
    /// dialect, so a responses-only model (e.g. `gpt-5.5`) works from any
    /// frontend. `None` when the catalog had no metadata for this model —
    /// providers then fall back to frontend-driven selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_endpoints: Option<Vec<String>>,
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
        // Step 1: gather inbound effort, falling back to route default.
        let inbound_effort = req.generation.reasoning.as_ref().and_then(|r| r.effort);
        let inbound_budget = req
            .generation
            .reasoning
            .as_ref()
            .and_then(|r| r.budget_tokens);
        let post_default = inbound_effort.or(self.default_reasoning_effort);

        // Step 2: apply mapping table. First match wins; unmatched passes through.
        let final_effort = match post_default {
            Some(e) => self
                .reasoning_mapping
                .iter()
                .find(|rule| rule.r#match == e)
                .map(|rule| rule.set)
                .or(Some(e)),
            None => None,
        };

        // Step 3: anthropic headers (unchanged).
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
            reasoning_effort: final_effort,
            reasoning_budget_tokens: inbound_budget,
            // Populated by the gateway pipeline after route resolution, where
            // the discovered model catalog is in scope. `resolve` is a pure
            // policy+request merge and has no catalog access.
            supported_efforts: None,
            supported_endpoints: None,
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

    #[test]
    fn resolved_policy_carries_budget_tokens() {
        let rp = ResolvedPolicy {
            reasoning_effort: Some(ReasoningEffort::High),
            reasoning_budget_tokens: Some(8192),
            supported_efforts: None,
            supported_endpoints: None,
            anthropic_headers: vec![],
        };
        assert_eq!(rp.reasoning_budget_tokens, Some(8192));
    }

    #[test]
    fn resolve_leaves_supported_efforts_for_pipeline() {
        // `resolve` is pure: it never fills supported_efforts (no catalog
        // access). The gateway populates it post-resolution.
        let policy = RoutePolicy {
            default_reasoning_effort: Some(ReasoningEffort::Max),
            ..Default::default()
        };
        assert_eq!(policy.resolve(&req()).supported_efforts, None);
    }

    #[test]
    fn mapping_rule_max_to_xhigh() {
        let policy = RoutePolicy {
            reasoning_mapping: vec![MappingRule {
                r#match: ReasoningEffort::Max,
                set: ReasoningEffort::Xhigh,
            }],
            ..Default::default()
        };
        let mut r = req();
        r.generation.reasoning = Some(ReasoningOptions {
            effort: Some(ReasoningEffort::Max),
            budget_tokens: None,
        });
        let resolved = policy.resolve(&r);
        assert_eq!(resolved.reasoning_effort, Some(ReasoningEffort::Xhigh));
    }

    #[test]
    fn mapping_first_rule_wins() {
        let policy = RoutePolicy {
            reasoning_mapping: vec![
                MappingRule {
                    r#match: ReasoningEffort::High,
                    set: ReasoningEffort::Xhigh,
                },
                MappingRule {
                    r#match: ReasoningEffort::High,
                    set: ReasoningEffort::Max,
                },
            ],
            ..Default::default()
        };
        let mut r = req();
        r.generation.reasoning = Some(ReasoningOptions {
            effort: Some(ReasoningEffort::High),
            budget_tokens: None,
        });
        assert_eq!(
            policy.resolve(&r).reasoning_effort,
            Some(ReasoningEffort::Xhigh)
        );
    }

    #[test]
    fn mapping_no_match_passthrough() {
        let policy = RoutePolicy {
            reasoning_mapping: vec![MappingRule {
                r#match: ReasoningEffort::Max,
                set: ReasoningEffort::Xhigh,
            }],
            ..Default::default()
        };
        let mut r = req();
        r.generation.reasoning = Some(ReasoningOptions {
            effort: Some(ReasoningEffort::Low),
            budget_tokens: None,
        });
        assert_eq!(
            policy.resolve(&r).reasoning_effort,
            Some(ReasoningEffort::Low)
        );
    }

    #[test]
    fn mapping_runs_after_default_fallback() {
        let policy = RoutePolicy {
            default_reasoning_effort: Some(ReasoningEffort::Medium),
            reasoning_mapping: vec![MappingRule {
                r#match: ReasoningEffort::Medium,
                set: ReasoningEffort::High,
            }],
            ..Default::default()
        };
        let resolved = policy.resolve(&req()); // inbound has no effort
        assert_eq!(resolved.reasoning_effort, Some(ReasoningEffort::High));
    }
}
