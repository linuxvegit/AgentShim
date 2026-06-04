use std::cmp::Reverse;
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
    /// Exact route table keyed by `(frontend, model)`. O(1) lookup; the hot
    /// path for well-tuned configs. Each entry holds the full fallback chain
    /// (`Vec<BackendTarget>` len 1 for singular config, len N for array).
    exact: HashMap<RouteKey, Vec<BackendTarget>>,
    /// Source `RouteEntry` for each exact route so resolution can recover
    /// per-route resilience policy (retry, breaker).
    exact_entries: HashMap<RouteKey, RouteEntry>,

    /// Prefix-wildcard route table, partitioned by frontend. Each per-frontend
    /// `Vec<PrefixRoute>` is sorted at startup by `prefix.len()` descending,
    /// so a linear scan's first hit IS the longest prefix match. Real-world
    /// per-frontend prefix counts are < 10; the linear scan is effectively
    /// O(1) at production scale.
    prefixes: HashMap<FrontendKind, Vec<PrefixRoute>>,

    /// Catch-all wildcard (`model: "*"`), at most one per frontend.
    wildcards: HashMap<FrontendKind, WildcardTarget>,
    /// `RouteEntry` source for wildcard routes, mirroring `exact_entries`.
    wildcard_entries: HashMap<FrontendKind, RouteEntry>,
}

/// A prefix-wildcard route (`model: "claude-*"`). Stored separately from both
/// exact and full-wildcard routes so:
///   - `list_routes()` ignores them structurally (catalog excludes prefix).
///   - The hot exact-lookup path is not touched.
///   - Longest-prefix-wins is a simple sorted-Vec scan.
struct PrefixRoute {
    /// `model` field with the trailing `*` stripped. Non-empty by validation.
    prefix: String,
    /// Same `Vec<BackendTarget>` shape exact routes produce.
    chain: Vec<BackendTarget>,
    /// True when the source `upstream_model` was `"*"` (singular form only);
    /// at lookup time the router overwrites `chain[0].model` with the
    /// inbound model string.
    upstream_model_passthrough: bool,
    /// Source `RouteEntry` so per-route retry/breaker propagate the same way
    /// they do for exact routes.
    entry: RouteEntry,
}

impl StaticRouter {
    pub fn from_config(cfg: &GatewayConfig) -> Self {
        let mut exact = HashMap::new();
        let mut exact_entries = HashMap::new();
        let mut prefixes: HashMap<FrontendKind, Vec<PrefixRoute>> = HashMap::new();
        let mut wildcards = HashMap::new();
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

            // Partition by `model` field shape into exactly one of three
            // buckets. `validate_routes` rule 8 guarantees `*` is either
            // absent, the sole character, or the final character; this
            // matches that grammar.
            if entry.model == "*" {
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
            } else if let Some(prefix) = entry.model.strip_suffix('*') {
                // Prefix wildcard. validate_routes ensures `prefix` is
                // non-empty and contains no other `*`.
                //
                // Pass-through is only meaningful in the singular form; the
                // array form specifies per-element model strings explicitly.
                let upstream_model_passthrough =
                    entry.upstreams.is_empty() && entry.upstream_model.as_deref() == Some("*");
                let route = PrefixRoute {
                    prefix: prefix.to_string(),
                    chain: targets,
                    upstream_model_passthrough,
                    entry: entry.clone(),
                };
                let bucket = prefixes.entry(frontend).or_default();
                // Spec §2.4: identical (frontend, model) entries silently
                // overwrite — later wins, mirroring HashMap-replacement
                // semantics for exact routes. For the Vec-based prefix
                // bucket, achieve this by removing any earlier entry with
                // the same literal prefix before pushing.
                bucket.retain(|existing| existing.prefix != route.prefix);
                bucket.push(route);
            } else {
                let key = RouteKey {
                    frontend,
                    model: entry.model.clone(),
                };
                exact.insert(key.clone(), targets);
                exact_entries.insert(key, entry.clone());
            }
        }

        // Sort each frontend's prefix bucket by literal-prefix length
        // descending. `sort_by_key` is stable (MUST stay stable — tie-break
        // preserves YAML appearance order when two distinct prefixes have
        // the same length; do NOT switch to `sort_unstable_by_key`).
        for routes in prefixes.values_mut() {
            routes.sort_by_key(|r| Reverse(r.prefix.len()));
        }

        Self {
            exact,
            exact_entries,
            prefixes,
            wildcards,
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

        // Tier 1: exact lookup. O(1) hot path; unchanged from pre-prefix.
        if let Some(targets) = self.exact.get(&key) {
            return Ok(ResolvedRoute {
                chain: targets.clone(),
                retry: self.retry_config_for_exact(&key, model),
                breaker: self.breaker_config_for_exact(&key, model),
                route_label: route_label(frontend, model),
            });
        }

        // Tier 2: prefix lookup. Sorted longest-first at startup, so the
        // first `starts_with` hit IS the longest-prefix match. Linear scan
        // over a per-frontend bucket (real-world size < 10).
        if let Some(bucket) = self.prefixes.get(&frontend) {
            for pr in bucket {
                if model.starts_with(&pr.prefix) {
                    tracing::debug!(
                        frontend = ?frontend,
                        inbound_model = %model,
                        matched_prefix = %pr.prefix,
                        provider = %pr.chain[0].provider,
                        "prefix route hit"
                    );
                    let mut chain = pr.chain.clone();
                    if pr.upstream_model_passthrough {
                        // Singular-form pass-through: rewrite chain head to
                        // the inbound model. Fuzzy upgrade (in ModelResolver)
                        // canonicalizes this against the upstream's
                        // discovered catalog.
                        chain[0].model = model.to_string();
                    }
                    return Ok(ResolvedRoute {
                        chain,
                        retry: pr.entry.retry.clone(),
                        breaker: pr.entry.breaker.clone(),
                        route_label: route_label(frontend, model),
                    });
                }
            }
        }

        // Tier 3: catch-all wildcard. Unchanged from pre-prefix.
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
                retry: self.retry_config_for_wildcard(frontend, model),
                breaker: self.breaker_config_for_wildcard(frontend, model),
                route_label: route_label(frontend, model),
            });
        }

        Err(RouteError::NoRoute {
            frontend,
            model: model.to_string(),
        })
    }

    fn retry_config_for_exact(&self, key: &RouteKey, model: &str) -> RetryConfig {
        match self.exact_entries.get(key) {
            Some(entry) => entry.retry.clone(),
            None => {
                tracing::debug!(
                    model = %model,
                    "no exact-route retry policy; using RetryConfig defaults"
                );
                RetryConfig::default()
            }
        }
    }

    fn breaker_config_for_exact(&self, key: &RouteKey, model: &str) -> BreakerConfig {
        match self.exact_entries.get(key) {
            Some(entry) => entry.breaker.clone(),
            None => {
                tracing::debug!(
                    model = %model,
                    "no exact-route breaker policy; using BreakerConfig defaults"
                );
                BreakerConfig::default()
            }
        }
    }

    fn retry_config_for_wildcard(&self, frontend: FrontendKind, model: &str) -> RetryConfig {
        match self.wildcard_entries.get(&frontend) {
            Some(entry) => entry.retry.clone(),
            None => {
                tracing::debug!(
                    model = %model,
                    "no wildcard-route retry policy; using RetryConfig defaults"
                );
                RetryConfig::default()
            }
        }
    }

    fn breaker_config_for_wildcard(&self, frontend: FrontendKind, model: &str) -> BreakerConfig {
        match self.wildcard_entries.get(&frontend) {
            Some(entry) => entry.breaker.clone(),
            None => {
                tracing::debug!(
                    model = %model,
                    "no wildcard-route breaker policy; using BreakerConfig defaults"
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
        // Both wildcards (`*`) and prefix wildcards (`claude-*`) are
        // intentionally excluded: catalog surfaces enumerate concrete
        // aliases only. Prefix routes live in `self.prefixes` and not in
        // `self.exact_entries`, so this iteration excludes them
        // structurally — no `if pattern { skip }` needed.
        self.exact_entries
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
