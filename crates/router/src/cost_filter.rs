//! Four-axis chain filter for cost-aware routing. Plan 06 P04 T3.
//!
//! Spec §4 D1-D5. Takes a chain (the operator-ordered list of
//! `BackendTarget` candidates produced by the router) and filters it by:
//!
//!   1. tier   — upstream.tier < route.min_tier
//!   2. latency — recent_p95 > upstream.p95_latency_budget_ms (per upstream)
//!   3. cap    — estimated_cost > route.max_cost_usd
//!
//! Returns the survivors in their original chain order (D6), plus per-
//! axis skip reasons so the caller (`ResilientCaller`, wired in T4) can
//! emit metrics + build the 503 `NoEligibleUpstream` response body when
//! the survivor list is empty.
//!
//! This module is pure: no I/O, no metrics emission. The caller is
//! responsible for translating `Skip`s and `Note`s into
//! `agent_shim_cost_filtered_total` counter increments.

use agent_shim_config::{
    upstream_cost, upstream_latency_budget, upstream_tier, GatewayConfig, RouteEntry,
};
use agent_shim_core::{BackendTarget, CanonicalRequest};

use crate::cost_estimate::estimate_request_cost;
use crate::latency_probe::LatencyProbe;

/// One axis on which an upstream was rejected (or noted, in the case of
/// `LatencyUnknown` / `TiktokenFallback`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FilterReason {
    /// Upstream's `tier` is below the route's `min_tier`.
    Tier,
    /// Upstream's recent p95 latency exceeded its `p95_latency_budget_ms`.
    Latency,
    /// Estimated request cost exceeded the route's `max_cost_usd`.
    Cap,
    /// Latency probe returned `None` — upstream was NOT skipped, but
    /// noted so operators see warm-up periods (metric only; never a
    /// survival decision).
    LatencyUnknown,
    /// Reserved for a future stricter mode in which `tiktoken-rs` fails
    /// to encode the request and a character-count fallback is used.
    /// Today the encoder is initialised once at startup via `OnceLock`
    /// (see `cost_estimate.rs`) and `encode_ordinary` handles arbitrary
    /// bytes gracefully — so this variant is defined for API stability
    /// but never produced by `filter_chain`.
    #[allow(dead_code)]
    TiktokenFallback,
}

impl FilterReason {
    /// Stable string label used as the `reason` metric label value on
    /// `agent_shim_cost_filtered_total`.
    pub fn as_str(&self) -> &'static str {
        match self {
            FilterReason::Tier => "tier",
            FilterReason::Latency => "latency",
            FilterReason::Cap => "cap",
            FilterReason::LatencyUnknown => "latency_unknown",
            FilterReason::TiktokenFallback => "tiktoken_fallback",
        }
    }
}

/// One skipped upstream + the reason it was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    pub upstream: String,
    pub reason: FilterReason,
}

/// One observed event that did NOT cause a skip — for metrics only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub upstream: String,
    pub reason: FilterReason,
}

/// Outcome of filtering a chain. `survivors` retains original order;
/// `skipped` lists the rejections; `notes` lists the non-skip events
/// (`LatencyUnknown`, `TiktokenFallback`) so the caller can still emit
/// counters for them.
#[derive(Debug, Clone)]
pub struct FilterOutcome {
    pub survivors: Vec<BackendTarget>,
    pub skipped: Vec<Skip>,
    pub notes: Vec<Note>,
}

/// Filter a chain. Pure function: no I/O, no metrics emission. Caller
/// emits metrics from the returned `skipped` + `notes` lists.
///
/// Order of axes: tier → latency → cap. The first axis to reject an
/// upstream is the one recorded; subsequent axes are not evaluated for
/// that upstream. This matches the spec's "first reason wins" semantics
/// for the per-upstream skip envelope.
pub fn filter_chain(
    chain: Vec<BackendTarget>,
    route: &RouteEntry,
    request: &CanonicalRequest,
    config: &GatewayConfig,
    probe: &dyn LatencyProbe,
) -> FilterOutcome {
    let mut survivors = Vec::with_capacity(chain.len());
    let mut skipped = Vec::new();
    let mut notes = Vec::new();

    for target in chain {
        // The `BackendTarget::provider` field holds the upstream name
        // — the same key used in `GatewayConfig.upstreams`. No
        // `upstream_name()` accessor exists on `BackendTarget`; we
        // read the field directly.
        let upstream_name = target.provider.clone();
        let Some(upstream_cfg) = config.upstreams.get(&upstream_name) else {
            // Upstream config missing — earlier validation should have
            // caught this. Keep as a survivor; `ResilientCaller` will
            // surface the error consistently with v0.4 semantics.
            survivors.push(target);
            continue;
        };

        // Axis 1: tier
        if let Some(min_tier) = route.min_tier {
            let upstream_tier = upstream_tier(upstream_cfg);
            if upstream_tier < min_tier {
                skipped.push(Skip {
                    upstream: upstream_name,
                    reason: FilterReason::Tier,
                });
                continue;
            }
        }

        // Axis 2: latency
        let p95_budget = upstream_latency_budget(upstream_cfg);
        if let Some(budget) = p95_budget {
            match probe.recent_p95_ms(&upstream_name) {
                Some(measured) if measured > budget => {
                    skipped.push(Skip {
                        upstream: upstream_name,
                        reason: FilterReason::Latency,
                    });
                    continue;
                }
                None => {
                    // Probe has no data — let through, but note it.
                    notes.push(Note {
                        upstream: upstream_name.clone(),
                        reason: FilterReason::LatencyUnknown,
                    });
                }
                Some(_) => {}
            }
        }

        // Axis 3: cost cap
        if let Some(cap) = route.max_cost_usd {
            let upstream_cost = upstream_cost(upstream_cfg);
            if let Some(est) = estimate_request_cost(request, upstream_cost) {
                if est.cost_usd > cap {
                    skipped.push(Skip {
                        upstream: upstream_name,
                        reason: FilterReason::Cap,
                    });
                    continue;
                }
            }
            // No cost schedule on this upstream — cap effectively
            // doesn't apply. Pass through (consistent with the
            // "Copilot has no per-token cost" case).
        }

        survivors.push(target);
    }

    FilterOutcome {
        survivors,
        skipped,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_config::{
        AnthropicUpstream, GatewayConfig, OpenAiCompatibleUpstream, RouteEntry, Secret, Tier,
        UpstreamConfig, UpstreamCost,
    };
    use agent_shim_core::{
        content::TextBlock,
        extensions::ExtensionMap,
        ids::RequestId,
        message::{Message, MessageRole},
        request::{CanonicalRequest, GenerationOptions, RequestMetadata},
        target::{FrontendInfo, FrontendKind, FrontendModel},
        BackendTarget, ContentBlock,
    };

    use crate::latency_probe::{DisabledLatencyProbe, MockLatencyProbe};

    // ---- fixture helpers ----

    fn target(provider: &str) -> BackendTarget {
        BackendTarget {
            provider: provider.into(),
            model: "m".into(),
            policy: Default::default(),
        }
    }

    fn openai_upstream(
        tier: Tier,
        cost: Option<UpstreamCost>,
        p95_latency_budget_ms: Option<u64>,
    ) -> UpstreamConfig {
        UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: "http://localhost".into(),
            api_key: Secret::new("dummy"),
            default_headers: Default::default(),
            request_timeout_secs: 30,
            tier,
            cost,
            p95_latency_budget_ms,
        })
    }

    fn anthropic_upstream(
        tier: Tier,
        cost: Option<UpstreamCost>,
        p95_latency_budget_ms: Option<u64>,
    ) -> UpstreamConfig {
        UpstreamConfig::Anthropic(AnthropicUpstream {
            base_url: "http://localhost".into(),
            api_key: Secret::new("dummy"),
            anthropic_version: "2023-06-01".into(),
            default_headers: Default::default(),
            request_timeout_secs: 30,
            tier,
            cost,
            p95_latency_budget_ms,
        })
    }

    fn cfg_with_upstreams(
        entries: impl IntoIterator<Item = (&'static str, UpstreamConfig)>,
    ) -> GatewayConfig {
        let mut cfg = GatewayConfig {
            server: Default::default(),
            logging: Default::default(),
            upstreams: Default::default(),
            routes: vec![],
            auth: Default::default(),
            rate_limit: Default::default(),
            copilot: None,
            admin: None,
            metrics: Default::default(),
            otel: None,
        };
        for (name, u) in entries {
            cfg.upstreams.insert(name.into(), u);
        }
        cfg
    }

    fn route_with(min_tier: Option<Tier>, max_cost_usd: Option<f64>) -> RouteEntry {
        let mut r = RouteEntry::singular("openai_chat", "x", "ignored", "ignored");
        r.min_tier = min_tier;
        r.max_cost_usd = max_cost_usd;
        r
    }

    fn request_with_text(text: &str, max_tokens: Option<u32>) -> CanonicalRequest {
        let model = FrontendModel("m".into());
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::OpenAiChat,
                requested_model: model.clone(),
            },
            model,
            system: vec![],
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text(TextBlock {
                    text: text.into(),
                    extensions: ExtensionMap::new(),
                })],
                name: None,
                extensions: ExtensionMap::new(),
            }],
            tools: vec![],
            tool_choice: Default::default(),
            generation: GenerationOptions {
                max_tokens,
                ..Default::default()
            },
            response_format: None,
            stream: false,
            metadata: RequestMetadata::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: Default::default(),
            extensions: ExtensionMap::new(),
        }
    }

    // ---- tests ----

    #[test]
    fn tier_filters_below_min() {
        // chain [economy, premium]; route min_tier = standard
        let cfg = cfg_with_upstreams([
            ("economy", openai_upstream(Tier::Economy, None, None)),
            ("premium", anthropic_upstream(Tier::Premium, None, None)),
        ]);
        let route = route_with(Some(Tier::Standard), None);
        let req = request_with_text("hi", None);

        let outcome = filter_chain(
            vec![target("economy"), target("premium")],
            &route,
            &req,
            &cfg,
            &DisabledLatencyProbe,
        );

        assert_eq!(outcome.survivors.len(), 1);
        assert_eq!(outcome.survivors[0].provider, "premium");
        assert_eq!(outcome.skipped.len(), 1);
        assert_eq!(outcome.skipped[0].upstream, "economy");
        assert_eq!(outcome.skipped[0].reason, FilterReason::Tier);
    }

    #[test]
    fn latency_filter_uses_probe() {
        // "slow" has measured p95 = 500ms, budget 300 → skipped
        // "unknown" has budget 300, probe returns None → survivor + note
        let cfg = cfg_with_upstreams([
            ("slow", openai_upstream(Tier::Standard, None, Some(300))),
            (
                "unknown",
                anthropic_upstream(Tier::Standard, None, Some(300)),
            ),
        ]);
        let route = route_with(None, None);
        let req = request_with_text("hi", None);
        let probe = MockLatencyProbe::with([("slow", 500u64)]);

        let outcome = filter_chain(
            vec![target("slow"), target("unknown")],
            &route,
            &req,
            &cfg,
            &probe,
        );

        assert_eq!(outcome.survivors.len(), 1);
        assert_eq!(outcome.survivors[0].provider, "unknown");
        assert_eq!(
            outcome.skipped,
            vec![Skip {
                upstream: "slow".into(),
                reason: FilterReason::Latency,
            }]
        );
        assert_eq!(
            outcome.notes,
            vec![Note {
                upstream: "unknown".into(),
                reason: FilterReason::LatencyUnknown,
            }]
        );
    }

    #[test]
    fn cost_cap_filter_uses_estimate() {
        // Extremely expensive upstream + tight cap → must be skipped
        let cost = UpstreamCost {
            input_per_million_usd: 1_000_000.0,
            output_per_million_usd: 1_000_000.0,
        };
        let cfg = cfg_with_upstreams([(
            "expensive",
            openai_upstream(Tier::Standard, Some(cost), None),
        )]);
        let route = route_with(None, Some(0.0001));
        let req = request_with_text("hello world this is a non-trivial request", Some(100));

        let outcome = filter_chain(
            vec![target("expensive")],
            &route,
            &req,
            &cfg,
            &DisabledLatencyProbe,
        );

        assert!(outcome.survivors.is_empty());
        assert_eq!(outcome.skipped.len(), 1);
        assert_eq!(outcome.skipped[0].upstream, "expensive");
        assert_eq!(outcome.skipped[0].reason, FilterReason::Cap);
    }

    #[test]
    fn all_filtered_returns_empty() {
        // Both upstreams fail tier; survivors empty, every upstream listed.
        let cfg = cfg_with_upstreams([
            ("a", openai_upstream(Tier::Economy, None, None)),
            ("b", anthropic_upstream(Tier::Economy, None, None)),
        ]);
        let route = route_with(Some(Tier::Premium), None);
        let req = request_with_text("hi", None);

        let outcome = filter_chain(
            vec![target("a"), target("b")],
            &route,
            &req,
            &cfg,
            &DisabledLatencyProbe,
        );

        assert!(outcome.survivors.is_empty());
        assert_eq!(outcome.skipped.len(), 2);
        assert!(outcome
            .skipped
            .iter()
            .all(|s| s.reason == FilterReason::Tier));
        let names: Vec<&str> = outcome
            .skipped
            .iter()
            .map(|s| s.upstream.as_str())
            .collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn partial_filter_preserves_chain_order() {
        // chain [a, b, c, d]; only "b" fails (economy tier).
        // Survivors must be [a, c, d] in that exact order.
        let cfg = cfg_with_upstreams([
            ("a", openai_upstream(Tier::Standard, None, None)),
            ("b", openai_upstream(Tier::Economy, None, None)),
            ("c", openai_upstream(Tier::Standard, None, None)),
            ("d", openai_upstream(Tier::Premium, None, None)),
        ]);
        let route = route_with(Some(Tier::Standard), None);
        let req = request_with_text("hi", None);

        let outcome = filter_chain(
            vec![target("a"), target("b"), target("c"), target("d")],
            &route,
            &req,
            &cfg,
            &DisabledLatencyProbe,
        );

        let survivor_names: Vec<&str> = outcome
            .survivors
            .iter()
            .map(|t| t.provider.as_str())
            .collect();
        assert_eq!(survivor_names, vec!["a", "c", "d"]);
        assert_eq!(outcome.skipped.len(), 1);
        assert_eq!(outcome.skipped[0].upstream, "b");
        assert_eq!(outcome.skipped[0].reason, FilterReason::Tier);
    }
}
