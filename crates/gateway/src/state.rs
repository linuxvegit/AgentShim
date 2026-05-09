use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_shim_config::{GatewayConfig, UpstreamConfig};
use agent_shim_frontends::{
    anthropic_messages::AnthropicMessages, openai_chat::OpenAiChat,
    openai_responses::OpenAiResponses,
};
use agent_shim_providers::{
    anthropic, deepseek, gemini,
    github_copilot::{self, credential_store},
    openai_compatible::{self},
    BackendProvider, ProviderRegistry,
};
use agent_shim_router::model_index::ModelIndex;
use agent_shim_router::{
    BreakerRegistry, Clock, LimiterRegistry, ModelResolver, ProviderLookup, ResilientCaller,
    Router, StaticRouter, SystemClock,
};

/// Adapter that lets `ResilientCaller` (in the router crate) look up
/// providers via the gateway's `ProviderRegistry` (in the providers crate).
///
/// The orphan rule forbids implementing the foreign `ProviderLookup` trait
/// directly on the foreign `ProviderRegistry` type, so this thin wrapper
/// lives here in the gateway — the only crate that depends on both. It
/// holds an `Arc<ProviderRegistry>` so the wrapper itself is cheap to
/// share between the resilience layer and the regular request pipeline.
struct GatewayProviderLookup {
    registry: Arc<ProviderRegistry>,
}

impl ProviderLookup for GatewayProviderLookup {
    fn get(&self, name: &str) -> Option<Arc<dyn BackendProvider>> {
        self.registry.get(name)
    }
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<GatewayConfig>,
    pub anthropic: Arc<AnthropicMessages>,
    pub openai: Arc<OpenAiChat>,
    pub openai_responses: Arc<OpenAiResponses>,
    pub providers: Arc<ProviderRegistry>,
    /// Resolves `(frontend, model_alias)` → `BackendTarget`. Owns both the
    /// static route table and the fuzzy model index as internal seams.
    pub resolver: Arc<ModelResolver>,
    /// Resilience-layer entry point. Wraps the same provider registry
    /// behind a `ProviderLookup` adapter, then walks the chain from the
    /// resolver per request, applying retry+fallback per chain element.
    /// Plan 04 P02 T6 wires this into `pipeline::dispatch`.
    pub resilient_caller: Arc<ResilientCaller>,
    /// Per-`(provider, model)` circuit breakers. Built once at startup and
    /// shared with `ResilientCaller`. Plan 04 P03 T4: the gateway holds
    /// the canonical handle so future surfaces (admin endpoints, metrics)
    /// can introspect the same registry the chain walker is using.
    ///
    /// `#[allow(dead_code)]` because the request hot path reads through
    /// `resilient_caller`'s clone of this Arc; this field is the
    /// future-facing introspection seam.
    #[allow(dead_code)]
    pub breaker_registry: Arc<BreakerRegistry>,
    /// Token-bucket rate limiters owned at the gateway boundary. Plan 04
    /// P04 T4 wires this into `ResilientCaller` (which actually consults
    /// the buckets) and keeps the canonical handle here for the same
    /// reason as `breaker_registry` — future admin/introspection
    /// endpoints will read through this Arc.
    ///
    /// `#[allow(dead_code)]` because the request hot path reads through
    /// `resilient_caller`'s clone of this Arc.
    #[allow(dead_code)]
    pub limiter_registry: Arc<LimiterRegistry>,
    /// Pre-computed `auth.enabled` so `pipeline::dispatch` doesn't re-walk
    /// the config on every request. Plan 04 P04 T4.
    pub auth_enabled: bool,
    /// Pre-computed `auth.required`. When true the pipeline rejects
    /// missing/unknown keys with HTTP 401 before the chain walk.
    pub auth_required: bool,
    /// Set of `sha256:<hex>` strings derived from `auth.keys`. The
    /// `auth.required` check rejects requests whose presented key hash
    /// is not in this set. Pre-computed once so the per-request lookup
    /// is a single HashSet contains() call.
    pub configured_key_hashes: Arc<HashSet<String>>,
}

impl AppState {
    pub async fn new(config: GatewayConfig) -> Self {
        Self::build(config, Arc::new(SystemClock)).await
    }

    /// Test-only constructor that lets tests inject a custom `Clock` into
    /// the `BreakerRegistry`. Production callers MUST use `AppState::new`.
    ///
    /// Plan 04 P03 T6: the half-open recovery smoke test needs deterministic
    /// time control to advance past `open_cooldown_secs` without sleeping. A
    /// `FakeClock` injected here flows through `BreakerRegistry::new(clock)`
    /// into every breaker decision/record call.
    ///
    /// `#[allow(dead_code)]` because integration tests in
    /// `crates/gateway/tests/` are a separate compilation unit; the
    /// library build proper does not call this constructor and would
    /// otherwise warn under `-D warnings`.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub async fn new_with_clock(config: GatewayConfig, clock: Arc<dyn Clock>) -> Self {
        Self::build(config, clock).await
    }

    /// Shared construction path for `new` and `new_with_clock`. The only
    /// thing the two callers parameterize is the `Clock` handed to
    /// `BreakerRegistry::new` — everything else (provider registry,
    /// resolver, frontend handlers) is identical, so extracting this
    /// helper keeps the two public constructors as one-liners.
    async fn build(config: GatewayConfig, clock: Arc<dyn Clock>) -> Self {
        let keepalive = Duration::from_secs(config.server.keepalive_secs);
        let anthropic = Arc::new(AnthropicMessages {
            keepalive: Some(keepalive),
        });
        let openai = Arc::new(OpenAiChat {
            keepalive: Some(keepalive),
            clock_override: None,
        });
        let openai_responses = Arc::new(OpenAiResponses {
            keepalive: Some(keepalive),
            clock_override: None,
        });

        let mut registry = ProviderRegistry::new();
        for (name, upstream) in &config.upstreams {
            match upstream {
                UpstreamConfig::OpenAiCompatible(cfg) => {
                    match openai_compatible::from_config(name, cfg) {
                        Ok(p) => registry.register(name.clone(), Arc::new(p)),
                        Err(e) => tracing::error!("failed to build provider {name}: {e}"),
                    }
                }
                UpstreamConfig::GithubCopilot => {
                    let credential_path = config
                        .copilot
                        .as_ref()
                        .map(|c| expand_tilde(&c.credential_path))
                        .unwrap_or_else(|| {
                            credential_store::default_path()
                                .unwrap_or_else(|_| PathBuf::from("./copilot.json"))
                        });
                    tracing::info!(path = %credential_path.display(), "copilot credential path");
                    match github_copilot::CopilotProvider::spawn(credential_path) {
                        Ok(p) => registry.register(name.clone(), Arc::new(p)),
                        Err(e) => tracing::error!("failed to build Copilot provider {name}: {e}"),
                    }
                }
                UpstreamConfig::Anthropic(cfg) => match anthropic::from_config(name, cfg) {
                    Ok(p) => registry.register(name.clone(), Arc::new(p)),
                    Err(e) => tracing::error!("failed to build Anthropic provider {name}: {e}"),
                },
                UpstreamConfig::Deepseek(cfg) => match deepseek::from_config(name, cfg) {
                    Ok(p) => registry.register(name.clone(), Arc::new(p)),
                    Err(e) => tracing::error!("failed to build Deepseek provider {name}: {e}"),
                },
                UpstreamConfig::Gemini(cfg) => match gemini::from_config(name, cfg) {
                    Ok(p) => registry.register(name.clone(), Arc::new(p)),
                    Err(e) => tracing::error!("failed to build Gemini provider {name}: {e}"),
                },
            }
        }

        let static_router: Arc<dyn Router> = Arc::new(StaticRouter::from_config(&config));

        let mut discovered = std::collections::HashMap::new();
        for (name, provider) in registry.iter() {
            match provider.list_models().await {
                Ok(Some(models)) => {
                    tracing::info!(provider = %name, count = models.len(), "discovered models");
                    discovered.insert(name.clone(), models);
                }
                Ok(None) => {
                    tracing::debug!(provider = %name, "provider does not support model discovery");
                }
                Err(e) => {
                    tracing::warn!(provider = %name, error = %e, "model discovery failed, skipping");
                }
            }
        }
        let model_index = Arc::new(ModelIndex::new(discovered));
        let resolver = Arc::new(ModelResolver::new(static_router, model_index));

        let providers = Arc::new(registry);
        let provider_lookup: Arc<dyn ProviderLookup> = Arc::new(GatewayProviderLookup {
            registry: Arc::clone(&providers),
        });
        let breaker_registry = Arc::new(BreakerRegistry::new(clock));

        // Plan 04 P04 T4: rate-limit registry. When `rate_limit.enabled
        // = false` (the default for v0.3 configs that don't carry the
        // block at all, plus explicit opt-out) we use the disabled
        // shortcut so the per-request gate calls become no-ops without
        // even touching the bucket map.
        let limiter_registry = Arc::new(if config.rate_limit.enabled {
            LimiterRegistry::from_config(&config.rate_limit)
        } else {
            LimiterRegistry::disabled()
        });

        let resilient_caller = Arc::new(ResilientCaller::new(
            provider_lookup,
            Arc::clone(&breaker_registry),
            Arc::clone(&limiter_registry),
        ));

        // Plan 04 P04 T4: pre-compute the auth fields so per-request
        // header parsing and key lookup are single Bool/HashSet checks
        // instead of walking the config map.
        let auth_enabled = config.auth.enabled;
        let auth_required = config.auth.required;
        let configured_key_hashes: Arc<HashSet<String>> =
            Arc::new(config.auth.keys.keys().cloned().collect());

        Self {
            config: Arc::new(config),
            anthropic,
            openai,
            openai_responses,
            providers,
            resolver,
            resilient_caller,
            breaker_registry,
            limiter_registry,
            auth_enabled,
            auth_required,
            configured_key_hashes,
        }
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}
