//! Combined model resolution: static route lookup + fuzzy model-name upgrade.
//!
//! [`ModelResolver`] is the deep module callers actually want. It owns the
//! `Router` trait (static config table) and the `ModelIndex` (fuzzy match
//! against the upstream's discovered model list) as **internal seams**, and
//! exposes [`ModelResolver::resolve`] / [`ModelResolver::resolve_route`].
//! Two-step composition that used to live in the gateway pipeline now lives
//! behind the interface.
//!
//! Tests for the static and fuzzy halves still live with their respective
//! types ([`crate::static_routes`], [`crate::model_index`]) — those interfaces
//! are still useful in isolation. New tests at [`ModelResolver`] level cover
//! the *combination*: static route returns model X, fuzzy upgrades it to Y.

use std::sync::Arc;

use agent_shim_core::{BackendTarget, FrontendKind};

use crate::model_index::ModelIndex;
use crate::{ResolvedRoute, RouteError, Router, StaticRouter};

/// The single entry point for turning `(frontend, model_alias)` into a
/// `BackendTarget`. Composes the static route table with fuzzy model-name
/// matching against discovered upstream models.
pub struct ModelResolver {
    static_router: Arc<StaticRouter>,
    model_index: Arc<ModelIndex>,
}

impl ModelResolver {
    pub fn new(static_router: Arc<StaticRouter>, model_index: Arc<ModelIndex>) -> Self {
        Self {
            static_router,
            model_index,
        }
    }

    /// Direct accessor for the embedded model index. Most callers should
    /// use `resolve` / `list_catalog`; this exists for the gateway's
    /// strict-upstream-models startup check which needs to enumerate the
    /// full discovered catalog without re-fetching it.
    pub fn model_index(&self) -> &ModelIndex {
        &self.model_index
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
        self.resolve_route(frontend, model_alias)
            .map(|route| route.chain)
    }

    pub fn resolve_route(
        &self,
        frontend: FrontendKind,
        model_alias: &str,
    ) -> Result<ResolvedRoute, RouteError> {
        let mut resolved = self.static_router.resolve_route(frontend, model_alias)?;
        self.apply_fuzzy_upgrades(&mut resolved.chain);
        Ok(resolved)
    }

    fn apply_fuzzy_upgrades(&self, chain: &mut [BackendTarget]) {
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
    }

    /// Build the public model catalog from the static route table plus the
    /// discovered upstream metadata.
    ///
    /// The result has one [`ModelRecord`] per explicit alias (`routes[].model`
    /// where `model != "*"`). When the same alias appears on multiple
    /// frontends, those frontends collapse into the record's `frontends`
    /// list. Wildcards are excluded because they don't enumerate a concrete
    /// alias.
    ///
    /// `long_context_variant` is computed by scanning the rest of the
    /// catalog for entries on the same `metadata.family` with a strictly
    /// larger `context_window_tokens`; the largest match wins. Records
    /// whose metadata lacks a family (or that have no larger sibling) leave
    /// the field `None`.
    pub fn list_catalog(&self) -> agent_shim_core::ModelCatalog {
        use std::collections::BTreeMap;

        use agent_shim_core::{ModelRecord, UpstreamRef};

        // Pass 1: walk routes, build a map keyed by alias-id. Multiple
        // frontends for the same alias collapse into one record.
        let mut by_alias: BTreeMap<String, ModelRecord> = BTreeMap::new();
        for (frontend, alias) in self.static_router.list_routes() {
            // Defensive: list_routes already excludes wildcards, but skip
            // here too in case a future Router impl returns them.
            if alias == "*" {
                continue;
            }
            let Ok(resolved) = self.resolve_route(frontend, &alias) else {
                continue;
            };
            let chain = resolved.chain;
            // For metadata lookup, use the chain head's (provider, model).
            let head = match chain.first() {
                Some(h) => h,
                None => continue,
            };
            let metadata = self
                .model_index
                .metadata(&head.provider, &head.model)
                .cloned();
            let upstreams_chain: Vec<UpstreamRef> = chain
                .iter()
                .map(|t| UpstreamRef {
                    provider: t.provider.clone(),
                    model: t.model.clone(),
                })
                .collect();
            let entry = by_alias
                .entry(alias.clone())
                .or_insert_with(|| ModelRecord {
                    id: alias.clone(),
                    frontends: Vec::new(),
                    upstream_provider: head.provider.clone(),
                    upstream_model: head.model.clone(),
                    upstreams_chain,
                    metadata,
                    long_context_variant: None,
                });
            if !entry.frontends.contains(&frontend) {
                entry.frontends.push(frontend);
            }
        }

        // Pass 2: long-context sibling lookup. For each record, scan all
        // other records on the same family and pick the one with the
        // strictly larger context window; the largest such sibling wins.
        let records: Vec<ModelRecord> = by_alias.into_values().collect();
        let mut out: Vec<ModelRecord> = Vec::with_capacity(records.len());
        for r in &records {
            let my_family = r.metadata.as_ref().and_then(|m| m.family.clone());
            let my_ctx = r
                .metadata
                .as_ref()
                .and_then(|m| m.context_window_tokens)
                .unwrap_or(0);
            let mut variant: Option<&ModelRecord> = None;
            if let Some(fam) = my_family.as_deref() {
                for other in &records {
                    if other.id == r.id {
                        continue;
                    }
                    let same_family = other
                        .metadata
                        .as_ref()
                        .and_then(|m| m.family.as_deref())
                        .is_some_and(|f| f == fam);
                    let bigger_ctx = other
                        .metadata
                        .as_ref()
                        .and_then(|m| m.context_window_tokens)
                        .is_some_and(|c| c > my_ctx);
                    if same_family && bigger_ctx {
                        let other_ctx = other
                            .metadata
                            .as_ref()
                            .and_then(|m| m.context_window_tokens)
                            .unwrap_or(0);
                        let cur_ctx = variant
                            .and_then(|v| v.metadata.as_ref().and_then(|m| m.context_window_tokens))
                            .unwrap_or(0);
                        if other_ctx > cur_ctx {
                            variant = Some(other);
                        }
                    }
                }
            }
            let mut rec = r.clone();
            rec.long_context_variant = variant.map(|v| v.id.clone());
            out.push(rec);
        }
        out
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
        }
    }

    fn resolver_with(cfg: GatewayConfig, provider: &str, discovered: &[&str]) -> ModelResolver {
        let router = Arc::new(StaticRouter::from_config(&cfg));
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
        let router = Arc::new(StaticRouter::from_config(&cfg));

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

    #[test]
    fn list_catalog_returns_one_record_per_explicit_alias() {
        let cfg = cfg_with_route(
            "anthropic_messages",
            "claude-opus-4-7",
            "copilot",
            "claude-opus-4.7",
        );
        let resolver = resolver_with(cfg, "copilot", &["claude-opus-4.7"]);
        let catalog = resolver.list_catalog();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].id, "claude-opus-4-7");
        assert_eq!(catalog[0].frontends, vec![FrontendKind::AnthropicMessages]);
        assert_eq!(catalog[0].upstream_provider, "copilot");
        // Fuzzy resolution converts the configured upstream model to its
        // canonical discovered form (here, the same string after lowercase
        // round-trip).
        assert_eq!(catalog[0].upstream_model, "claude-opus-4.7");
        assert_eq!(catalog[0].upstreams_chain.len(), 1);
    }

    #[test]
    fn list_catalog_collapses_same_alias_on_multiple_frontends() {
        let mut cfg = cfg_with_route("anthropic_messages", "shared", "copilot", "claude-opus-4.7");
        cfg.routes.push(RouteEntry::singular(
            "openai_chat",
            "shared",
            "copilot",
            "claude-opus-4.7",
        ));
        let resolver = resolver_with(cfg, "copilot", &["claude-opus-4.7"]);
        let catalog = resolver.list_catalog();
        let shared = catalog
            .iter()
            .find(|r| r.id == "shared")
            .expect("shared alias present");
        assert_eq!(shared.frontends.len(), 2);
        assert!(shared.frontends.contains(&FrontendKind::AnthropicMessages));
        assert!(shared.frontends.contains(&FrontendKind::OpenAiChat));
    }

    #[test]
    fn list_catalog_populates_metadata_when_index_has_it() {
        use std::collections::BTreeMap;

        use agent_shim_core::{ModelMetadata, ModelSupports};

        let cfg = cfg_with_route("openai_chat", "gpt-5.5", "copilot", "gpt-5.5");
        let router = Arc::new(StaticRouter::from_config(&cfg));
        let mut all = HashMap::new();
        let mut map = BTreeMap::new();
        map.insert(
            "gpt-5.5".to_string(),
            ModelMetadata {
                context_window_tokens: Some(1_050_000),
                family: Some("gpt-5.5".into()),
                supports: ModelSupports {
                    vision: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        all.insert("copilot".to_string(), map);
        let index = Arc::new(ModelIndex::with_metadata(all));
        let resolver = ModelResolver::new(router, index);
        let cat = resolver.list_catalog();
        let r = &cat[0];
        let m = r.metadata.as_ref().expect("metadata present");
        assert_eq!(m.context_window_tokens, Some(1_050_000));
        assert_eq!(m.supports.vision, Some(true));
    }

    #[test]
    fn list_catalog_omits_metadata_when_index_is_empty() {
        let cfg = cfg_with_route("openai_chat", "gpt-4o", "openai", "gpt-4o");
        let router = Arc::new(StaticRouter::from_config(&cfg));
        let resolver = ModelResolver::new(router, Arc::new(ModelIndex::empty()));
        let cat = resolver.list_catalog();
        assert!(cat[0].metadata.is_none());
        assert!(cat[0].long_context_variant.is_none());
    }

    #[test]
    fn list_catalog_finds_long_context_sibling_on_same_family() {
        use std::collections::BTreeMap;

        use agent_shim_core::ModelMetadata;

        let mut cfg = cfg_with_route(
            "anthropic_messages",
            "claude-opus-4-7",
            "copilot",
            "claude-opus-4.7",
        );
        cfg.routes.push(RouteEntry::singular(
            "anthropic_messages",
            "claude-opus-4-7-1m",
            "copilot",
            "claude-opus-4.7-1m-internal",
        ));
        let router = Arc::new(StaticRouter::from_config(&cfg));
        let mut all = HashMap::new();
        let mut map = BTreeMap::new();
        map.insert(
            "claude-opus-4.7".to_string(),
            ModelMetadata {
                context_window_tokens: Some(200_000),
                family: Some("claude-opus-4.7".into()),
                ..Default::default()
            },
        );
        map.insert(
            "claude-opus-4.7-1m-internal".to_string(),
            ModelMetadata {
                context_window_tokens: Some(1_000_000),
                // Same family marker is what tells the catalog these are
                // siblings; production data also follows this convention.
                family: Some("claude-opus-4.7".into()),
                ..Default::default()
            },
        );
        all.insert("copilot".to_string(), map);
        let index = Arc::new(ModelIndex::with_metadata(all));
        let resolver = ModelResolver::new(router, index);
        let cat = resolver.list_catalog();
        let small = cat
            .iter()
            .find(|r| r.id == "claude-opus-4-7")
            .expect("small variant present");
        assert_eq!(
            small.long_context_variant.as_deref(),
            Some("claude-opus-4-7-1m")
        );
        let big = cat
            .iter()
            .find(|r| r.id == "claude-opus-4-7-1m")
            .expect("big variant present");
        assert!(big.long_context_variant.is_none());
    }
}
