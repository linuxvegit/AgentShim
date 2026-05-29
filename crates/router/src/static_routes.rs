use std::{collections::HashMap, sync::Arc};

use agent_shim_config::{BreakerConfig, GatewayConfig, RetryConfig, RouteEntry};
use agent_shim_core::{
    policy::MappingRule, request::ReasoningEffort, BackendTarget, FrontendKind, RoutePolicy,
};

use crate::{ResolvedRoute, RouteError, Router};

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
    /// Source `RouteEntry` indexed by `(frontend, model)` so route resolution
    /// can recover the startup-frozen per-route resilience policy (`retry`,
    /// `breaker`) alongside the fallback chain.
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
            // Translate config-shape `MappingRuleConfig { match: String, set: String }`
            // into core-shape `MappingRule { match: ReasoningEffort, set: ReasoningEffort }`.
            // Unknown effort strings are silently dropped here as a belt-and-braces
            // — `validate_routes` rejects them at config-load (Task 14), so any
            // bad row reaching this point came from a hand-built `GatewayConfig`
            // fixture that bypassed validation.
            let reasoning_mapping: Vec<MappingRule> = entry
                .reasoning_mapping
                .iter()
                .filter_map(|rule| {
                    let m = ReasoningEffort::parse(&rule.r#match)?;
                    let s = ReasoningEffort::parse(&rule.set)?;
                    Some(MappingRule { r#match: m, set: s })
                })
                .collect();
            let policy = RoutePolicy {
                default_reasoning_effort,
                default_anthropic_beta: entry.anthropic_beta.clone(),
                reasoning_mapping,
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

    pub fn resolve_route(
        &self,
        frontend: FrontendKind,
        model: &str,
    ) -> Result<ResolvedRoute, RouteError> {
        let key = RouteKey {
            frontend,
            model: model.to_string(),
        };
        if let Some(targets) = self.routes.get(&key) {
            return Ok(ResolvedRoute {
                chain: targets.clone(),
                retry: self.retry_config_for(frontend, &key, model),
                breaker: self.breaker_config_for(frontend, &key, model),
                route_label: route_label(frontend, model),
            });
        }
        if let Some(wc) = self.wildcards.get(&frontend) {
            let upstream_model = if wc.upstream_model == "*" {
                model.to_string()
            } else {
                wc.upstream_model.clone()
            };
            return Ok(ResolvedRoute {
                chain: vec![BackendTarget {
                    provider: wc.provider.clone(),
                    model: upstream_model,
                    policy: wc.policy.clone(),
                }],
                retry: self.retry_config_for(frontend, &key, model),
                breaker: self.breaker_config_for(frontend, &key, model),
                route_label: route_label(frontend, model),
            });
        }
        Err(RouteError::NoRoute {
            frontend,
            model: model.to_string(),
        })
    }

    fn retry_config_for(&self, frontend: FrontendKind, key: &RouteKey, model: &str) -> RetryConfig {
        match self
            .route_entries
            .get(key)
            .or_else(|| self.wildcard_entries.get(&frontend))
        {
            Some(entry) => entry.retry.clone(),
            None => {
                tracing::debug!(
                    model = %model,
                    "no route-specific retry policy; using RetryConfig defaults"
                );
                RetryConfig::default()
            }
        }
    }

    fn breaker_config_for(
        &self,
        frontend: FrontendKind,
        key: &RouteKey,
        model: &str,
    ) -> BreakerConfig {
        match self
            .route_entries
            .get(key)
            .or_else(|| self.wildcard_entries.get(&frontend))
        {
            Some(entry) => entry.breaker.clone(),
            None => {
                tracing::debug!(
                    model = %model,
                    "no route-specific breaker policy; using BreakerConfig defaults"
                );
                BreakerConfig::default()
            }
        }
    }
}

fn frontend_kind_str(frontend: FrontendKind) -> &'static str {
    match frontend {
        FrontendKind::AnthropicMessages => "anthropic_messages",
        FrontendKind::OpenAiChat => "openai_chat",
        FrontendKind::OpenAiResponses => "openai_responses",
    }
}

fn route_label(frontend: FrontendKind, model: &str) -> Arc<str> {
    Arc::from(format!("{}/{}", frontend_kind_str(frontend), model))
}

impl Router for StaticRouter {
    fn resolve(
        &self,
        frontend: FrontendKind,
        model: &str,
    ) -> Result<Vec<BackendTarget>, RouteError> {
        self.resolve_route(frontend, model).map(|route| route.chain)
    }

    fn list_routes(&self) -> Vec<(FrontendKind, String)> {
        // Wildcards are intentionally excluded: catalog surfaces enumerate
        // concrete aliases, not "any model goes" entries.
        self.route_entries
            .keys()
            .map(|k| (k.frontend, k.model.clone()))
            .collect()
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
            plugins: ::std::collections::BTreeMap::new(),
            auth: Default::default(),
            rate_limit: Default::default(),
            copilot: None,
            admin: None,
            metrics: Default::default(),
            otel: None,
            shutdown: Default::default(),
            validation: Default::default(),
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
                min_tier: None,
                max_cost_usd: None,
                plugins: None,
                reasoning_mapping: vec![],
            }],
            plugins: ::std::collections::BTreeMap::new(),
            auth: Default::default(),
            rate_limit: Default::default(),
            copilot: None,
            admin: None,
            metrics: Default::default(),
            otel: None,
            shutdown: Default::default(),
            validation: Default::default(),
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
    fn resolve_route_returns_specific_chain_retry_and_breaker() {
        let mut cfg = cfg_with_route("openai_chat", "gpt-4o", "u", "gpt-4o");
        cfg.routes[0].retry.max_attempts = 7;
        cfg.routes[0].breaker.failure_threshold_pct = 99;
        let router = StaticRouter::from_config(&cfg);
        let resolved = router
            .resolve_route(FrontendKind::OpenAiChat, "gpt-4o")
            .expect("specific route must resolve");
        assert_eq!(resolved.chain.len(), 1);
        assert_eq!(resolved.chain[0].provider, "u");
        assert_eq!(resolved.chain[0].model, "gpt-4o");
        assert_eq!(resolved.retry.max_attempts, 7);
        assert_eq!(resolved.breaker.failure_threshold_pct, 99);
    }

    #[test]
    fn resolve_route_uses_wildcard_chain_retry_and_breaker() {
        let mut cfg = cfg_with_route("anthropic_messages", "*", "copilot", "*");
        cfg.routes[0].retry.max_attempts = 5;
        cfg.routes[0].breaker.failure_threshold_pct = 77;
        let router = StaticRouter::from_config(&cfg);
        let resolved = router
            .resolve_route(FrontendKind::AnthropicMessages, "any-model")
            .expect("wildcard route must resolve any model");
        assert_eq!(resolved.chain.len(), 1);
        assert_eq!(resolved.chain[0].provider, "copilot");
        assert_eq!(resolved.chain[0].model, "any-model");
        assert_eq!(resolved.retry.max_attempts, 5);
        assert_eq!(resolved.breaker.failure_threshold_pct, 77);
    }

    #[test]
    fn resolve_route_returns_no_route_when_no_match() {
        let cfg = cfg_with_route("openai_chat", "gpt-4o", "u", "gpt-4o");
        let router = StaticRouter::from_config(&cfg);
        let err = router
            .resolve_route(FrontendKind::AnthropicMessages, "claude-foo")
            .unwrap_err();
        assert!(matches!(
            err,
            RouteError::NoRoute {
                frontend: FrontendKind::AnthropicMessages,
                model
            } if model == "claude-foo"
        ));
    }

    #[test]
    fn resolve_route_label_uses_frontend_kind_string_and_alias() {
        let mut cfg = cfg_with_route("anthropic_messages", "claude", "a", "claude-upstream");
        cfg.routes.push(RouteEntry::singular(
            "openai_chat",
            "gpt-chat",
            "b",
            "gpt-chat-upstream",
        ));
        cfg.routes.push(RouteEntry::singular(
            "openai_responses",
            "gpt-responses",
            "c",
            "gpt-responses-upstream",
        ));
        let router = StaticRouter::from_config(&cfg);
        let anthropic = router
            .resolve_route(FrontendKind::AnthropicMessages, "claude")
            .unwrap();
        let chat = router
            .resolve_route(FrontendKind::OpenAiChat, "gpt-chat")
            .unwrap();
        let responses = router
            .resolve_route(FrontendKind::OpenAiResponses, "gpt-responses")
            .unwrap();
        assert_eq!(anthropic.route_label.as_ref(), "anthropic_messages/claude");
        assert_eq!(chat.route_label.as_ref(), "openai_chat/gpt-chat");
        assert_eq!(
            responses.route_label.as_ref(),
            "openai_responses/gpt-responses"
        );
    }

    #[test]
    fn mapping_config_propagates_to_policy() {
        use agent_shim_config::MappingRuleConfig;
        let mut cfg = cfg_with_route(
            "anthropic_messages",
            "claude-opus",
            "copilot",
            "claude-opus-4.7",
        );
        cfg.routes[0].reasoning_mapping = vec![
            MappingRuleConfig {
                r#match: "max".into(),
                set: "xhigh".into(),
            },
            MappingRuleConfig {
                r#match: "high".into(),
                set: "xhigh".into(),
            },
        ];
        let router = StaticRouter::from_config(&cfg);
        let chain = router
            .resolve(FrontendKind::AnthropicMessages, "claude-opus")
            .unwrap();
        let policy = &chain[0].policy;
        assert_eq!(policy.reasoning_mapping.len(), 2);
        assert_eq!(policy.reasoning_mapping[0].r#match, ReasoningEffort::Max,);
        assert_eq!(policy.reasoning_mapping[0].set, ReasoningEffort::Xhigh,);
        assert_eq!(policy.reasoning_mapping[1].r#match, ReasoningEffort::High,);
    }

    #[test]
    fn mapping_config_skips_unknown_effort_strings() {
        // Validation should have rejected these before reaching the router,
        // but the router applies a belt-and-braces filter_map so a hand-built
        // fixture with a typo doesn't panic — it just drops the bad row.
        use agent_shim_config::MappingRuleConfig;
        let mut cfg = cfg_with_route(
            "anthropic_messages",
            "claude-opus",
            "copilot",
            "claude-opus-4.7",
        );
        cfg.routes[0].reasoning_mapping = vec![
            MappingRuleConfig {
                r#match: "bogus".into(),
                set: "high".into(),
            },
            MappingRuleConfig {
                r#match: "max".into(),
                set: "xhigh".into(),
            },
        ];
        let router = StaticRouter::from_config(&cfg);
        let chain = router
            .resolve(FrontendKind::AnthropicMessages, "claude-opus")
            .unwrap();
        let policy = &chain[0].policy;
        assert_eq!(policy.reasoning_mapping.len(), 1);
        assert_eq!(policy.reasoning_mapping[0].r#match, ReasoningEffort::Max,);
    }
}
