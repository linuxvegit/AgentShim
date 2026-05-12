//! Combined model resolution: static route lookup + fuzzy model-name upgrade.
//!
//! [`ModelResolver`] is the deep module callers actually want. It owns the
//! `Router` trait (static config table) and the `ModelIndex` (fuzzy match
//! against the upstream's discovered model list) as **internal seams**, and
//! exposes a single [`ModelResolver::resolve`] method. Two-step composition
//! that used to live in the gateway pipeline now lives behind the interface.
//!
//! Tests for the static and fuzzy halves still live with their respective
//! types ([`crate::static_routes`], [`crate::model_index`]) — those interfaces
//! are still useful in isolation. New tests at [`ModelResolver`] level cover
//! the *combination*: static route returns model X, fuzzy upgrades it to Y.

use std::sync::Arc;

use agent_shim_config::RetryConfig;
use agent_shim_core::{BackendTarget, FrontendKind};

use crate::model_index::ModelIndex;
use crate::{RouteError, Router};

/// The single entry point for turning `(frontend, model_alias)` into a
/// `BackendTarget`. Composes the static route table with fuzzy model-name
/// matching against discovered upstream models.
pub struct ModelResolver {
    static_router: Arc<dyn Router>,
    model_index: Arc<ModelIndex>,
}

impl ModelResolver {
    pub fn new(static_router: Arc<dyn Router>, model_index: Arc<ModelIndex>) -> Self {
        Self {
            static_router,
            model_index,
        }
    }

    /// Resolve the inbound `(frontend, model_alias)` to the full fallback
    /// chain of [`BackendTarget`]s. Looks up the static route (which may
    /// already be a multi-element fallback chain for `upstreams: [...]`
    /// routes), then upgrades each `target.model` to the canonical form
    /// discovered from that target's upstream if fuzzy matching finds a hit.
    /// Logs each upgrade at info level.
    ///
    /// The chain is guaranteed non-empty on `Ok` — singular routes produce
    /// 1-element vecs; array routes are validated to be non-empty by
    /// `validate_routes`. Fuzzy upgrade is per-element because each chain
    /// element can target a different provider whose discovered model list
    /// is independent.
    pub fn resolve(
        &self,
        frontend: FrontendKind,
        model_alias: &str,
    ) -> Result<Vec<BackendTarget>, RouteError> {
        let mut chain = self.static_router.resolve(frontend, model_alias)?;
        for target in chain.iter_mut() {
            if let Some(canonical) = self.model_index.resolve(&target.provider, &target.model) {
                if canonical != target.model {
                    tracing::info!(
                        requested = %target.model,
                        resolved = %canonical,
                        provider = %target.provider,
                        "fuzzy model match"
                    );
                    target.model = canonical.to_string();
                }
            }
        }
        Ok(chain)
    }

    /// Look up the per-route retry policy. Delegates to the static router.
    /// Returns `None` if no route entry matched — callers fall back to
    /// `RetryConfig::default()` (the §4.5/D12 defaults).
    pub fn find_retry_policy(
        &self,
        frontend: FrontendKind,
        model_alias: &str,
    ) -> Option<RetryConfig> {
        self.static_router.find_retry_policy(frontend, model_alias)
    }

    /// Look up the per-route breaker policy. Delegates to the static
    /// router. Returns `None` if no route entry matched — callers fall
    /// back to `BreakerConfig::default()` (the §4.5/D6 defaults).
    pub fn find_breaker_policy(
        &self,
        frontend: FrontendKind,
        model_alias: &str,
    ) -> Option<agent_shim_config::BreakerConfig> {
        self.static_router
            .find_breaker_policy(frontend, model_alias)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashMap};

    use agent_shim_config::{BreakerConfig, GatewayConfig, RetryConfig, RouteEntry};

    use crate::StaticRouter;

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
            routes: vec![RouteEntry {
                frontend: frontend.to_string(),
                model: model.to_string(),
                upstream: Some(upstream.to_string()),
                upstream_model: Some(upstream_model.to_string()),
                upstreams: vec![],
                reasoning_effort: None,
                anthropic_beta: None,
                retry: RetryConfig::default(),
                breaker: BreakerConfig::default(),
            }],
            auth: Default::default(),
            rate_limit: Default::default(),
            copilot: None,
            admin: None,
            metrics: Default::default(),
            otel: None,
        }
    }

    fn resolver_with(cfg: GatewayConfig, provider: &str, discovered: &[&str]) -> ModelResolver {
        let router: Arc<dyn Router> = Arc::new(StaticRouter::from_config(&cfg));
        let mut map = HashMap::new();
        let set: BTreeSet<String> = discovered.iter().map(|s| s.to_string()).collect();
        map.insert(provider.to_string(), set);
        let index = Arc::new(ModelIndex::new(map));
        ModelResolver::new(router, index)
    }

    #[test]
    fn static_route_only_passes_target_through() {
        let cfg = cfg_with_route("openai_chat", "gpt-4o", "openai", "gpt-4o-2024-11-20");
        let resolver = resolver_with(cfg, "openai", &[]);
        let chain = resolver
            .resolve(FrontendKind::OpenAiChat, "gpt-4o")
            .unwrap();
        assert_eq!(chain.len(), 1);
        let target = &chain[0];
        assert_eq!(target.provider, "openai");
        assert_eq!(target.model, "gpt-4o-2024-11-20");
    }

    #[test]
    fn fuzzy_upgrade_replaces_target_model_with_canonical() {
        let cfg = cfg_with_route(
            "anthropic_messages",
            "claude-sonnet-4-5",
            "copilot",
            "claude-sonnet-4-5",
        );
        let resolver = resolver_with(cfg, "copilot", &["claude-sonnet-4-5-20250514"]);
        let chain = resolver
            .resolve(FrontendKind::AnthropicMessages, "claude-sonnet-4-5")
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].model, "claude-sonnet-4-5-20250514");
    }

    #[test]
    fn fuzzy_match_on_unknown_provider_leaves_target_alone() {
        let cfg = cfg_with_route("openai_chat", "gpt-4o", "openai", "gpt-4o");
        // ModelIndex has data only for a different provider — no upgrade.
        let resolver = resolver_with(cfg, "copilot", &["gpt-4o-2024-11-20"]);
        let chain = resolver
            .resolve(FrontendKind::OpenAiChat, "gpt-4o")
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].model, "gpt-4o");
    }

    #[test]
    fn no_route_propagates_route_error() {
        let cfg = cfg_with_route("openai_chat", "gpt-4o", "openai", "gpt-4o");
        let resolver = resolver_with(cfg, "openai", &["gpt-4o"]);
        let err = resolver
            .resolve(FrontendKind::OpenAiChat, "no-such-model")
            .unwrap_err();
        assert!(matches!(err, RouteError::NoRoute { .. }));
    }

    #[test]
    fn fuzzy_upgrade_applies_to_each_chain_element() {
        // Multi-upstream route: each element targets a different provider, and
        // each provider has its own canonical model name. Fuzzy upgrade must
        // run per-element so chain[0] resolves against `openai`'s discovered
        // list and chain[1] resolves against `copilot`'s.
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
                        model: "gpt-4o".into(),
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
            metrics: Default::default(),
            otel: None,
        };
        let router: Arc<dyn Router> = Arc::new(StaticRouter::from_config(&cfg));

        let mut map = HashMap::new();
        map.insert("openai".to_string(), {
            let mut s = BTreeSet::new();
            s.insert("gpt-4o-2024-11-20".to_string());
            s
        });
        map.insert("copilot".to_string(), {
            let mut s = BTreeSet::new();
            s.insert("gpt-4o-2024-08-06".to_string());
            s
        });
        let index = Arc::new(ModelIndex::new(map));
        let resolver = ModelResolver::new(router, index);

        let chain = resolver
            .resolve(FrontendKind::OpenAiChat, "gpt-4o")
            .unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].provider, "openai");
        assert_eq!(chain[0].model, "gpt-4o-2024-11-20"); // openai upgrade
        assert_eq!(chain[1].provider, "copilot");
        assert_eq!(chain[1].model, "gpt-4o-2024-08-06"); // copilot upgrade
    }
}
