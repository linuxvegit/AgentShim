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

    /// Resolve the inbound `(frontend, model_alias)` to a final
    /// [`BackendTarget`]. Looks up the static route, then upgrades
    /// `target.model` to the canonical form discovered from the upstream if
    /// fuzzy matching finds a hit. Logs the upgrade at info level.
    pub fn resolve(
        &self,
        frontend: FrontendKind,
        model_alias: &str,
    ) -> Result<BackendTarget, RouteError> {
        let mut target = self.static_router.resolve(frontend, model_alias)?;

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

        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashMap};

    use agent_shim_config::{GatewayConfig, RouteEntry};

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
                upstream: upstream.to_string(),
                upstream_model: upstream_model.to_string(),
                reasoning_effort: None,
                anthropic_beta: None,
            }],
            copilot: None,
        }
    }

    fn resolver_with(
        cfg: GatewayConfig,
        provider: &str,
        discovered: &[&str],
    ) -> ModelResolver {
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
        let target = resolver
            .resolve(FrontendKind::OpenAiChat, "gpt-4o")
            .unwrap();
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
        let target = resolver
            .resolve(FrontendKind::AnthropicMessages, "claude-sonnet-4-5")
            .unwrap();
        assert_eq!(target.model, "claude-sonnet-4-5-20250514");
    }

    #[test]
    fn fuzzy_match_on_unknown_provider_leaves_target_alone() {
        let cfg = cfg_with_route("openai_chat", "gpt-4o", "openai", "gpt-4o");
        // ModelIndex has data only for a different provider — no upgrade.
        let resolver = resolver_with(cfg, "copilot", &["gpt-4o-2024-11-20"]);
        let target = resolver
            .resolve(FrontendKind::OpenAiChat, "gpt-4o")
            .unwrap();
        assert_eq!(target.model, "gpt-4o");
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
}
