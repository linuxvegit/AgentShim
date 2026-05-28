use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;

use agent_shim_config::UpstreamConfig;
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

/// Immutable handles built once at startup. None of these fields can change
/// without restarting the process. See Phase 5 design spec §2.2 for the
/// AppCore vs AppSnapshot split.
///
/// Plan 01 P01 T2: extracted from the old monolithic `AppState` so Plan 04's
/// hot-reload path can swap the policy-bearing `AppSnapshot` while leaving
/// the immutable handles (provider registry, breaker registry, etc.) alone.
///
/// Plan 04 P04 T4: `#[derive(Clone)]` so `commands::serve::run` can fork
/// the freshly constructed core with a pinned `config_path` for
/// SIGHUP-triggered reloads. All fields are `Arc<...>`, primitives, or
/// other cheaply cloneable handles.
#[derive(Clone)]
pub struct AppCore {
    /// Path the configuration was originally loaded from. `None` when the
    /// state was constructed from an in-memory `GatewayConfig` (tests, ad-hoc
    /// callers). Plan 04 P04 T3 reads this from `POST /admin/reload` when
    /// no body is supplied, and T4's SIGHUP listener re-reads this path on
    /// every signal. Plan 04 P04 T4 wires `commands::serve::run` to clone
    /// `AppCore` with this field populated.
    pub config_path: Option<std::path::PathBuf>,
    /// Bind address / keepalive / timeout knobs cached at startup. Server
    /// rebinding mid-process is out of scope, so these never change after
    /// `AppCore` is constructed.
    pub server_config: agent_shim_config::ServerConfig,
    /// Optional admin listener config (Plan 01 P01 T1 schema; consumed
    /// by `commands::serve` in T4 to bind the admin port). `None` →
    /// admin listener disabled entirely (spec D10). The
    /// `#[allow(dead_code)]` falls off in T4.
    #[allow(dead_code)]
    pub admin_config: Option<agent_shim_config::AdminConfig>,
    /// Anthropic Messages frontend adapter (decode + encode). Built
    /// once at startup; the per-request keepalive is captured here.
    pub anthropic: Arc<AnthropicMessages>,
    /// OpenAI Chat Completions frontend adapter.
    pub openai: Arc<OpenAiChat>,
    /// OpenAI Responses frontend adapter.
    pub openai_responses: Arc<OpenAiResponses>,
    /// Backend provider registry. The chain walker (`ResilientCaller`)
    /// looks up providers through `GatewayProviderLookup`, which
    /// borrows this Arc.
    pub providers: Arc<ProviderRegistry>,
    /// Resolves `(frontend, model_alias)` → `BackendTarget`. Owns both the
    /// static route table and the fuzzy model index as internal seams.
    pub resolver: Arc<ModelResolver>,
    /// Resilience-layer entry point. Wraps the same provider registry
    /// behind a `ProviderLookup` adapter, then walks the chain from the
    /// resolver per request, applying retry+fallback per chain element.
    pub resilient_caller: Arc<ResilientCaller>,
    /// Per-`(provider, model)` circuit breakers. State, not policy — the
    /// breaker map survives reload (spec §2.2). Future admin/introspection
    /// endpoints will read through this Arc.
    #[allow(dead_code)]
    pub breaker_registry: Arc<BreakerRegistry>,
    /// Token-bucket rate limiters. The registry holds buckets across reload;
    /// individual buckets are replaced when policy changes (spec §5.4).
    /// Phase 6 P02: now hot-swappable via `ArcSwap`. The reload-applying
    /// task in `commands::serve::handle_reload` rebuilds and stores a fresh
    /// registry after committing the new snapshot (spec §6.2, atomicity).
    #[allow(dead_code)]
    pub limiter_registry: Arc<arc_swap::ArcSwap<LimiterRegistry>>,
    /// Prometheus metrics handle. Plan 02 P02 T3. Built once at startup;
    /// the /metrics admin handler renders against it.
    pub metrics: Arc<agent_shim_observability::MetricsHandle>,
    /// Sender side of the reload trigger channel. SIGHUP listener and
    /// `/admin/reload` handler push onto this; the reload-applying task
    /// drains it and performs the `ArcSwap` of `snapshot`. Plan 04 P04
    /// T2 spec §5.1. The receiver is returned from `AppState::new` and
    /// consumed by the reload-applying task that `commands::serve::run`
    /// spawns in T4. `#[allow(dead_code)]` falls off in T3 once
    /// `/admin/reload` clones this sender.
    #[allow(dead_code)]
    pub reload_tx: tokio::sync::mpsc::Sender<crate::reload_trigger::ReloadRequest>,
}

/// Hot-swappable policy-bearing snapshot. Plan 04 will swap this on
/// SIGHUP / `POST /admin/reload`; in P01 it is built once and never replaced.
/// See Phase 5 design spec §2.2.
pub struct AppSnapshot {
    /// The policy-bearing config. Hot-reloadable fields (auth, rate_limit,
    /// routes, upstream-policy knobs) live here; immutable startup-time
    /// fields (server bind, admin listener, provider construction) live on
    /// `AppCore`.
    #[allow(dead_code)]
    pub config: Arc<agent_shim_config::GatewayConfig>,
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
    /// Plan 07 P07: plugins now hot-swappable. In-flight requests
    /// capture the snapshot at the top of `pipeline::dispatch`;
    /// subsequent reloads do not perturb their plugin view.
    pub plugins: Arc<agent_shim_plugins::PluginRegistry>,
}

/// The application-wide handler state cloned into every axum handler.
///
/// Phase 5 P01 T2: split into a fixed `AppCore` (provider registry, frontend
/// adapters, resilience layer) and a hot-swappable `AppSnapshot` (config,
/// auth, etc.). In-flight requests capture the snapshot once at the top of
/// `pipeline::dispatch` so a mid-request reload does not perturb their
/// policy view (spec §2.2).
#[derive(Clone)]
pub struct AppState {
    pub core: Arc<AppCore>,
    pub snapshot: Arc<ArcSwap<AppSnapshot>>,
}

impl AppState {
    pub async fn new(
        config: agent_shim_config::GatewayConfig,
    ) -> anyhow::Result<(
        Self,
        tokio::sync::mpsc::Receiver<crate::reload_trigger::ReloadRequest>,
    )> {
        Self::build(config, Arc::new(SystemClock), None).await
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
    pub async fn new_with_clock(
        config: agent_shim_config::GatewayConfig,
        clock: Arc<dyn Clock>,
    ) -> anyhow::Result<(
        Self,
        tokio::sync::mpsc::Receiver<crate::reload_trigger::ReloadRequest>,
    )> {
        Self::build(config, clock, None).await
    }

    /// Plan 07 P04 T12: integration-test-only constructor that lets tests
    /// inject a custom `PluginRegistry` (instead of the default
    /// `empty()`) (in P07, the injected plugins end up on `AppSnapshot`
    /// rather than `AppCore`). Same shape as `new_with_clock`.
    /// Production callers MUST use `AppState::new`.
    ///
    /// `#[allow(dead_code)]` because integration tests in
    /// `crates/gateway/tests/` are a separate compilation unit; the
    /// library build proper does not call this constructor and would
    /// otherwise warn under `-D warnings`.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub async fn new_with_plugins(
        config: agent_shim_config::GatewayConfig,
        plugins: Arc<agent_shim_plugins::PluginRegistry>,
    ) -> anyhow::Result<(
        Self,
        tokio::sync::mpsc::Receiver<crate::reload_trigger::ReloadRequest>,
    )> {
        Self::build(config, Arc::new(SystemClock), Some(plugins)).await
    }

    /// Shared construction path for `new`, `new_with_clock`, and
    /// `new_with_plugins`. The callers parameterize the `Clock` handed to
    /// `BreakerRegistry::new` and the `PluginRegistry` placed on
    /// `AppSnapshot` (so reload-time hot-swap honors §2.2 atomicity);
    /// everything else (provider registry, resolver, frontend
    /// handlers) is identical, so extracting this helper keeps the
    /// public constructors as one-liners.
    async fn build(
        config: agent_shim_config::GatewayConfig,
        clock: Arc<dyn Clock>,
        plugin_override: Option<Arc<agent_shim_plugins::PluginRegistry>>,
    ) -> anyhow::Result<(
        Self,
        tokio::sync::mpsc::Receiver<crate::reload_trigger::ReloadRequest>,
    )> {
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
                UpstreamConfig::GithubCopilot(_cfg) => {
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

        let plugins = match plugin_override {
            Some(p) => p,
            None => {
                let factories = agent_shim_plugins::builtin::builtin_plugins();
                let plugin_specs: Vec<(String, agent_shim_config::plugins::PluginEntry)> = config
                    .plugins
                    .iter()
                    .map(|(name, entry)| (name.clone(), entry.clone()))
                    .collect();
                let deps = agent_shim_plugins::FactoryDependencies {
                    providers: registry.inner_map(),
                };
                Arc::new(
                    agent_shim_plugins::PluginRegistry::build(
                        factories,
                        &plugin_specs,
                        &config.routes,
                        deps,
                    )
                    .map_err(|e| anyhow::anyhow!("plugin registry build failed: {e}"))?,
                )
            }
        };

        let static_router: Arc<dyn Router> = Arc::new(StaticRouter::from_config(&config));

        let mut discovered = std::collections::HashMap::new();
        for (name, provider) in registry.iter() {
            match provider.list_models().await {
                Ok(Some(models)) => {
                    tracing::info!(provider = %name, count = models.len(), "discovered models");
                    // ModelIndex currently takes BTreeSet<String>; convert
                    // until Plan B Task 3 migrates ModelIndex to BTreeMap.
                    let names: std::collections::BTreeSet<String> =
                        models.into_keys().collect();
                    discovered.insert(name.clone(), names);
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
        //
        // Phase 6 P02 T2: wrap in `ArcSwap` so `handle_reload` can swap
        // the entire registry atomically when `rate_limit.*` changes.
        let limiter_registry = Arc::new(arc_swap::ArcSwap::from_pointee(
            if config.rate_limit.enabled {
                LimiterRegistry::from_config(&config.rate_limit)
            } else {
                LimiterRegistry::disabled()
            },
        ));

        // Plan 02 P02 T3: install the global metrics recorder. The
        // exporter handle lives in AppCore so the /metrics admin handler
        // can render against it.
        //
        // Plan 06 P04 T5: install must happen BEFORE `ResilientCaller::new`
        // so the same handle can back `PrometheusLatencyProbe`.
        let metrics = agent_shim_observability::install_metrics(&config.metrics);

        // Plan 07 P01 T5: pre-warm the cl100k tokenizer so the first request
        // doesn't pay the ~50-100ms init cost on the hot path. The result is
        // dropped — we only care about the side-effect of populating the
        // `OnceLock` cell inside `agent-shim-tokens`.
        let _ = agent_shim_tokens::cl100k_encoder();

        // Plan 06 P04 T5: Prometheus-backed latency probe for the
        // cost filter's latency axis. Renders `metrics` on demand to
        // pull the recent p95 of `agent_shim_upstream_duration_seconds`.
        let latency_probe: Arc<dyn agent_shim_router::LatencyProbe> = Arc::new(
            crate::latency_probe::PrometheusLatencyProbe::new(Arc::clone(&metrics)),
        );

        let resilient_caller = Arc::new(ResilientCaller::new(
            provider_lookup,
            Arc::clone(&breaker_registry),
            Arc::clone(&limiter_registry),
            Arc::clone(&latency_probe),
        ));

        // Plan 04 P04 T4: pre-compute the auth fields so per-request
        // header parsing and key lookup are single Bool/HashSet checks
        // instead of walking the config map.
        let auth_enabled = config.auth.enabled;
        let auth_required = config.auth.required;
        let configured_key_hashes: Arc<HashSet<String>> =
            Arc::new(config.auth.keys.keys().cloned().collect());

        let server_config = config.server.clone();
        let admin_config = config.admin.clone();

        // Plan 04 P04 T2: reload trigger channel. SIGHUP and
        // `/admin/reload` push onto the sender stored in `AppCore`; the
        // receiver is returned to the caller (`commands::serve::run`),
        // which moves it into the reload-applying task in T4. Bounded
        // at 8 so a stuck applier rejects rather than ballooning memory.
        let (reload_tx, reload_rx) = tokio::sync::mpsc::channel(8);

        let snapshot = AppSnapshot {
            config: Arc::new(config),
            auth_enabled,
            auth_required,
            configured_key_hashes,
            plugins,
        };

        let core = AppCore {
            config_path: None,
            server_config,
            admin_config,
            anthropic,
            openai,
            openai_responses,
            providers,
            resolver,
            resilient_caller,
            breaker_registry,
            limiter_registry,
            metrics,
            reload_tx,
        };

        Ok((
            Self {
                core: Arc::new(core),
                snapshot: Arc::new(ArcSwap::new(Arc::new(snapshot))),
            },
            reload_rx,
        ))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_yaml() -> &'static str {
        r#"
server: {bind: 127.0.0.1, port: 8787}
upstreams:
  noop:
    type: open_ai_compatible
    base_url: http://localhost:9999/v1
    api_key: dummy
    tier: standard
routes:
  - frontend: openai_chat
    model: x
    upstream: noop
    upstream_model: x
"#
    }

    #[tokio::test]
    async fn appstate_split_into_core_and_snapshot() {
        let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        let (state, _reload_rx) = AppState::new(cfg).await.unwrap();

        // Compile-time assertions: each half holds the right type. Pinning
        // the types here ensures a future refactor can't quietly move a
        // field from one half to the other without breaking this test.
        let _: &Arc<agent_shim_providers::ProviderRegistry> = &state.core.providers;
        let _: &Arc<agent_shim_router::BreakerRegistry> = &state.core.breaker_registry;
        let _: &Arc<agent_shim_router::ModelResolver> = &state.core.resolver;
        let _: &Arc<arc_swap::ArcSwap<agent_shim_router::LimiterRegistry>> =
            &state.core.limiter_registry;
        let _: &Arc<agent_shim_router::ResilientCaller> = &state.core.resilient_caller;
        let _: &Option<agent_shim_config::AdminConfig> = &state.core.admin_config;

        let snap = state.snapshot.load_full();
        let _: &Arc<agent_shim_config::GatewayConfig> = &snap.config;
        let _: &Arc<HashSet<String>> = &snap.configured_key_hashes;
        // Plan 07 P07 T1: plugins live on the hot-swappable snapshot.
        let _: &Arc<agent_shim_plugins::PluginRegistry> = &snap.plugins;

        // Behavioural assertion: the YAML's absent auth block produces the
        // expected defaults on the snapshot side.
        assert!(!snap.auth_required);
        assert!(snap.configured_key_hashes.is_empty());
        assert!(
            state.core.admin_config.is_none(),
            "absent admin block → None"
        );
        assert!(state.core.providers.get("noop").is_some());
    }

    #[tokio::test]
    async fn captured_snapshot_survives_swap() {
        // The §2.2 invariant: in-flight requests use the snapshot at the
        // time of their start. Mid-request swap doesn't reach them.
        let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        let (state, _reload_rx) = AppState::new(cfg).await.unwrap();

        // Simulate request capture (what pipeline::dispatch does at the top).
        let captured = state.snapshot.load_full();
        assert!(!captured.auth_required);

        // Build a replacement snapshot with auth_required = true and store.
        let new_snap = AppSnapshot {
            config: Arc::clone(&captured.config),
            auth_enabled: true,
            auth_required: true,
            configured_key_hashes: Arc::clone(&captured.configured_key_hashes),
            plugins: Arc::clone(&captured.plugins),
        };
        state.snapshot.store(Arc::new(new_snap));

        // The captured Arc still sees the OLD value — in-flight requests
        // are unaffected by the swap.
        assert!(
            !captured.auth_required,
            "captured snapshot stays valid post-swap"
        );

        // A fresh load sees the NEW value — new requests pick up the swap.
        assert!(
            state.snapshot.load_full().auth_required,
            "fresh load sees new snapshot"
        );
    }
}
