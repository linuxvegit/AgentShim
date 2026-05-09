use std::collections::HashMap;

use agent_shim_config::{BreakerConfig, GatewayConfig, RetryConfig, RouteEntry};
use agent_shim_core::{request::ReasoningEffort, BackendTarget, FrontendKind, RoutePolicy};

use crate::{RouteError, Router};

/// Key for the route table.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RouteKey {
    frontend: FrontendKind,
    model: String,
}

/// A wildcard route entry: `model: "*"` matches any model not handled by a
/// specific route. The `upstream_model` field controls what's sent upstream:
/// - `"*"` means pass the original model name through
/// - anything else is used as a literal override
struct WildcardTarget {
    provider: String,
    upstream_model: String,
    policy: RoutePolicy,
}

/// A static router built from `GatewayConfig.routes`.
pub struct StaticRouter {
    /// Route table keyed by `(frontend, model)`. Each entry holds the FULL
    /// fallback chain — singular config produces a 1-element vec, array-form
    /// config produces N elements in configured order. `Router::resolve`
    /// returns `Vec<BackendTarget>` so `ResilientCaller` (Plan 02 T4) can
    /// walk the chain head-to-tail.
    routes: HashMap<RouteKey, Vec<BackendTarget>>,
    wildcards: HashMap<FrontendKind, WildcardTarget>,
    /// Source `RouteEntry` indexed by `(frontend, model)` so the gateway
    /// pipeline can recover per-route resilience policy (`retry`, `breaker`)
    /// after `resolve` collapses a route into a `BackendTarget`. Phase 4
    /// (Plan 04 P01 T5) — the retry loop in `pipeline.rs` reads from here
    /// via [`StaticRouter::find_retry_policy`].
    route_entries: HashMap<RouteKey, RouteEntry>,
    /// Wildcard variant of `route_entries`. Keyed by frontend only because
    /// wildcards match any model for a frontend.
    wildcard_entries: HashMap<FrontendKind, RouteEntry>,
}

impl StaticRouter {
    pub fn from_config(cfg: &GatewayConfig) -> Self {
        let mut routes = HashMap::new();
        let mut wildcards = HashMap::new();
        let mut route_entries = HashMap::new();
        let mut wildcard_entries = HashMap::new();
        for entry in &cfg.routes {
            let frontend = match entry.frontend.as_str() {
                "anthropic_messages" | "anthropic" => FrontendKind::AnthropicMessages,
                "openai_chat" | "openai" => FrontendKind::OpenAiChat,
                "openai_responses" | "responses" => FrontendKind::OpenAiResponses,
                other => {
                    tracing::warn!("unknown frontend kind in route config: {other}");
                    continue;
                }
            };
            let default_reasoning_effort = entry
                .reasoning_effort
                .as_deref()
                .and_then(ReasoningEffort::parse);
            if entry.reasoning_effort.is_some() && default_reasoning_effort.is_none() {
                tracing::warn!(
                    value = ?entry.reasoning_effort,
                    "ignoring unknown reasoning_effort in route config (expected minimal/low/medium/high/xhigh)"
                );
            }
            let policy = RoutePolicy {
                default_reasoning_effort,
                default_anthropic_beta: entry.anthropic_beta.clone(),
            };

            // Build the full fallback chain from either the singular (v0.3)
            // or array (v0.4) form. `validate_routes` enforces exactly one
            // shape per route, so an empty `upstreams` vec is the unambiguous
            // marker for the singular shape. Plan 02 T1: singular produces a
            // 1-element chain; array produces N elements in configured order.
            let targets: Vec<BackendTarget> = if !entry.upstreams.is_empty() {
                entry
                    .upstreams
                    .iter()
                    .map(|u| BackendTarget {
                        provider: u.name.clone(),
                        model: u.model.clone(),
                        policy: policy.clone(),
                    })
                    .collect()
            } else {
                // Singular form: `validate_routes` enforces these are
                // populated when `upstreams` is empty. `.expect()` (not
                // `unwrap_or_default()`) so a config that bypassed
                // validation panics with a pointer to the invariant
                // instead of silently routing to `provider = ""` and
                // failing later with a confusing `UnknownProvider("")`.
                let provider = entry
                    .upstream
                    .clone()
                    .expect("validate_routes guarantees singular routes have `upstream` set");
                let upstream_model = entry
                    .upstream_model
                    .clone()
                    .expect("validate_routes guarantees singular routes have `upstream_model` set");
                vec![BackendTarget {
                    provider,
                    model: upstream_model,
                    policy: policy.clone(),
                }]
            };

            if entry.model == "*" {
                // Wildcards stay single-target — exactly one wildcard per
                // frontend, and chain walking under wildcards isn't part of
                // Plan 02. Pick the chain head as the wildcard primary.
                let head = &targets[0];
                wildcards.insert(
                    frontend,
                    WildcardTarget {
                        provider: head.provider.clone(),
                        upstream_model: head.model.clone(),
                        policy: policy.clone(),
                    },
                );
                wildcard_entries.insert(frontend, entry.clone());
                continue;
            }
            let key = RouteKey {
                frontend,
                model: entry.model.clone(),
            };
            routes.insert(key.clone(), targets);
            route_entries.insert(key, entry.clone());
        }
        Self {
            routes,
            wildcards,
            route_entries,
            wildcard_entries,
        }
    }

    /// Look up the per-route retry policy for this `(frontend, model)`.
    ///
    /// Returns `Some(retry)` if a static route (specific or wildcard) was
    /// configured for the pair, `None` if no route matches. Callers can fall
    /// back to `RetryConfig::default()` on `None` — the default policy is the
    /// §4.5/D12 contract from the Phase 4 design.
    ///
    /// Specific `(frontend, model)` routes win over wildcards, mirroring the
    /// resolution order in `Router::resolve`.
    pub fn find_retry_policy(&self, frontend: FrontendKind, model: &str) -> Option<RetryConfig> {
        let key = RouteKey {
            frontend,
            model: model.to_string(),
        };
        if let Some(entry) = self.route_entries.get(&key) {
            return Some(entry.retry.clone());
        }
        self.wildcard_entries
            .get(&frontend)
            .map(|entry| entry.retry.clone())
    }

    /// Look up the per-route breaker policy for this `(frontend, model)`.
    ///
    /// Mirrors `find_retry_policy`: specific routes win over wildcards.
    /// Returns `None` if no route matches; callers fall back to
    /// `BreakerConfig::default()` (the §4.5/D6 defaults).
    pub fn find_breaker_policy(
        &self,
        frontend: FrontendKind,
        model: &str,
    ) -> Option<BreakerConfig> {
        let key = RouteKey {
            frontend,
            model: model.to_string(),
        };
        if let Some(entry) = self.route_entries.get(&key) {
            return Some(entry.breaker.clone());
        }
        self.wildcard_entries
            .get(&frontend)
            .map(|entry| entry.breaker.clone())
    }
}

impl Router for StaticRouter {
    fn resolve(
        &self,
        frontend: FrontendKind,
        model: &str,
    ) -> Result<Vec<BackendTarget>, RouteError> {
        let key = RouteKey {
            frontend,
            model: model.to_string(),
        };
        if let Some(targets) = self.routes.get(&key) {
            return Ok(targets.clone());
        }
        if let Some(wc) = self.wildcards.get(&frontend) {
            let upstream_model = if wc.upstream_model == "*" {
                model.to_string()
            } else {
                wc.upstream_model.clone()
            };
            return Ok(vec![BackendTarget {
                provider: wc.provider.clone(),
                model: upstream_model,
                policy: wc.policy.clone(),
            }]);
        }
        Err(RouteError::NoRoute {
            frontend,
            model: model.to_string(),
        })
    }

    fn find_retry_policy(&self, frontend: FrontendKind, model: &str) -> Option<RetryConfig> {
        StaticRouter::find_retry_policy(self, frontend, model)
    }

    fn find_breaker_policy(&self, frontend: FrontendKind, model: &str) -> Option<BreakerConfig> {
        StaticRouter::find_breaker_policy(self, frontend, model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_config::{BreakerConfig, GatewayConfig, RetryConfig, RouteEntry};

    fn cfg_with_route(
        frontend: &str,
        model: &str,
        upstream: &str,
        upstream_model: &str,
    ) -> GatewayConfig {
        GatewayConfig {
            server: Default::default(),
            logging: Default::default(),
            upstreams: Default::default(),
            routes: vec![RouteEntry::singular(
                frontend,
                model,
                upstream,
                upstream_model,
            )],
            auth: Default::default(),
            rate_limit: Default::default(),
            copilot: None,
            admin: None,
        }
    }

    #[test]
    fn resolves_known_route() {
        let cfg = cfg_with_route("openai_chat", "gpt-4o", "my-upstream", "gpt-4o-2024-11-20");
        let router = StaticRouter::from_config(&cfg);
        let chain = router.resolve(FrontendKind::OpenAiChat, "gpt-4o").unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].provider, "my-upstream");
        assert_eq!(chain[0].model, "gpt-4o-2024-11-20");
    }

    #[test]
    fn unknown_model_returns_no_route() {
        let cfg = cfg_with_route("openai_chat", "gpt-4o", "my-upstream", "gpt-4o-2024-11-20");
        let router = StaticRouter::from_config(&cfg);
        let err = router
            .resolve(FrontendKind::OpenAiChat, "unknown-model")
            .unwrap_err();
        assert!(matches!(err, RouteError::NoRoute { .. }));
    }

    #[test]
    fn resolves_anthropic_route() {
        let cfg = cfg_with_route(
            "anthropic_messages",
            "claude-3-5-sonnet",
            "upstream-a",
            "claude-3-5-sonnet-20241022",
        );
        let router = StaticRouter::from_config(&cfg);
        let chain = router
            .resolve(FrontendKind::AnthropicMessages, "claude-3-5-sonnet")
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].provider, "upstream-a");
    }

    #[test]
    fn wildcard_route_passes_model_through() {
        let mut cfg = cfg_with_route("anthropic_messages", "*", "copilot", "*");
        cfg.routes.push(RouteEntry::singular(
            "anthropic_messages",
            "override",
            "other",
            "other-model",
        ));
        let router = StaticRouter::from_config(&cfg);
        // Specific route wins
        let chain = router
            .resolve(FrontendKind::AnthropicMessages, "override")
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].provider, "other");
        // Wildcard catches anything else, passes model name through (still
        // a 1-element chain — wildcards don't fan out under Plan 02).
        let chain = router
            .resolve(FrontendKind::AnthropicMessages, "claude-opus-4-7")
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].provider, "copilot");
        assert_eq!(chain[0].model, "claude-opus-4-7");
    }

    #[test]
    fn array_route_produces_multi_element_chain() {
        // Plan 02 T1: array-form route preserves all upstreams, in
        // configured order, as a fallback chain. Replaces the P01 T5 contract
        // (which kept only the head) now that `Router::resolve` returns
        // `Vec<BackendTarget>` for `ResilientCaller` to walk.
        use agent_shim_config::UpstreamRef;
        let cfg = GatewayConfig {
            server: Default::default(),
            logging: Default::default(),
            upstreams: Default::default(),
            routes: vec![RouteEntry {
                frontend: "openai_chat".into(),
                model: "gpt-4o".into(),
                upstream: None,
                upstream_model: None,
                upstreams: vec![
                    UpstreamRef {
                        name: "openai".into(),
                        model: "gpt-4o-2024-11-20".into(),
                    },
                    UpstreamRef {
                        name: "copilot".into(),
                        model: "gpt-4o".into(),
                    },
                ],
                reasoning_effort: None,
                anthropic_beta: None,
                retry: RetryConfig::default(),
                breaker: BreakerConfig::default(),
            }],
            auth: Default::default(),
            rate_limit: Default::default(),
            copilot: None,
            admin: None,
        };
        let router = StaticRouter::from_config(&cfg);
        let chain = router.resolve(FrontendKind::OpenAiChat, "gpt-4o").unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].provider, "openai");
        assert_eq!(chain[0].model, "gpt-4o-2024-11-20");
        assert_eq!(chain[1].provider, "copilot");
        assert_eq!(chain[1].model, "gpt-4o");
    }

    #[test]
    fn find_retry_policy_returns_route_specific_config() {
        let mut cfg = cfg_with_route("openai_chat", "gpt-4o", "u", "gpt-4o");
        cfg.routes[0].retry.max_attempts = 7;
        let router = StaticRouter::from_config(&cfg);
        let retry = router
            .find_retry_policy(FrontendKind::OpenAiChat, "gpt-4o")
            .expect("specific route must yield its retry config");
        assert_eq!(retry.max_attempts, 7);
    }

    #[test]
    fn find_retry_policy_falls_back_to_wildcard() {
        let mut cfg = cfg_with_route("anthropic_messages", "*", "copilot", "*");
        cfg.routes[0].retry.max_attempts = 5;
        let router = StaticRouter::from_config(&cfg);
        let retry = router
            .find_retry_policy(FrontendKind::AnthropicMessages, "any-model")
            .expect("wildcard route must yield its retry config for any model");
        assert_eq!(retry.max_attempts, 5);
    }

    #[test]
    fn find_retry_policy_returns_none_when_no_match() {
        let cfg = cfg_with_route("openai_chat", "gpt-4o", "u", "gpt-4o");
        let router = StaticRouter::from_config(&cfg);
        // Different frontend, no wildcard for it → no policy.
        assert!(router
            .find_retry_policy(FrontendKind::AnthropicMessages, "claude-foo")
            .is_none());
    }

    #[test]
    fn find_breaker_policy_returns_route_specific_config() {
        let mut cfg = cfg_with_route("openai_chat", "gpt-4o", "u", "gpt-4o");
        cfg.routes[0].breaker.failure_threshold_pct = 99;
        let router = StaticRouter::from_config(&cfg);
        let breaker = router
            .find_breaker_policy(FrontendKind::OpenAiChat, "gpt-4o")
            .expect("specific route must yield its breaker config");
        assert_eq!(breaker.failure_threshold_pct, 99);
    }

    #[test]
    fn find_breaker_policy_falls_back_to_wildcard() {
        let mut cfg = cfg_with_route("anthropic_messages", "*", "copilot", "*");
        cfg.routes[0].breaker.failure_threshold_pct = 77;
        let router = StaticRouter::from_config(&cfg);
        let breaker = router
            .find_breaker_policy(FrontendKind::AnthropicMessages, "any-model")
            .expect("wildcard route must yield its breaker config for any model");
        assert_eq!(breaker.failure_threshold_pct, 77);
    }

    #[test]
    fn find_breaker_policy_returns_none_when_no_match() {
        let cfg = cfg_with_route("openai_chat", "gpt-4o", "u", "gpt-4o");
        let router = StaticRouter::from_config(&cfg);
        // Different frontend, no wildcard for it → no policy.
        assert!(router
            .find_breaker_policy(FrontendKind::AnthropicMessages, "claude-foo")
            .is_none());
    }
}
