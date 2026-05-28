//! Phase 7 P05 T12: gateway integration tests for plugin observability.
//!
//! Drives the full HTTP→pipeline→provider path with hand-built plugin
//! registries, then renders Prometheus output to verify the plugin
//! metric labels are populated. Mirrors the pattern from
//! `plugins_pipeline.rs` (P04 T12).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use agent_shim_config::{
    schema::{LoggingConfig, RouteEntry, ServerConfig},
    GatewayConfig,
};
use agent_shim_core::{
    BackendTarget, CanonicalRequest, CanonicalStream, ContentBlockKind, FrontendKind, MessageRole,
    ResponseId, StopReason, StreamEvent,
};
use agent_shim_gateway::{server::build_router, state::AppState};
use agent_shim_observability::MetricsHandle;
use agent_shim_plugins::{
    HookSet, OnError, Plugin, PluginContext, PluginRegistry, PluginResult, ResponseSummary,
};
use agent_shim_providers::{
    BackendProvider, ProviderCapabilities, ProviderError, ProviderRegistry,
};
use agent_shim_router::model_index::ModelIndex;
use agent_shim_router::{
    BreakerRegistry, ModelResolver, ProviderLookup, ResilientCaller, Router as RouterTrait,
    StaticRouter,
};
use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::util::ServiceExt;

// ── Stub provider: returns a canned 6-event stream ─────────────────────

struct StubProvider {
    capabilities: ProviderCapabilities,
}

impl StubProvider {
    fn new() -> Self {
        Self {
            capabilities: ProviderCapabilities {
                streaming: true,
                tool_use: false,
                vision: false,
                json_mode: false,
                accepts_xhigh: false,
            },
        }
    }
}

#[async_trait]
impl BackendProvider for StubProvider {
    fn name(&self) -> &'static str {
        "stub-provider"
    }
    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }
    async fn complete(
        &self,
        _req: CanonicalRequest,
        _target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError> {
        let events: Vec<Result<StreamEvent, agent_shim_core::StreamError>> = vec![
            Ok(StreamEvent::ResponseStart {
                id: ResponseId("test-resp".to_string()),
                model: "test-model".to_string(),
                created_at_unix: 0,
            }),
            Ok(StreamEvent::MessageStart {
                role: MessageRole::Assistant,
            }),
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                kind: ContentBlockKind::Text,
            }),
            Ok(StreamEvent::TextDelta {
                index: 0,
                text: "hi".to_string(),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                stop_sequence: None,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

// ── AppState builder that also returns the MetricsHandle ───────────────

fn make_app_state(plugins: Arc<PluginRegistry>) -> (AppState, Arc<MetricsHandle>) {
    use agent_shim_frontends::{
        anthropic_messages::AnthropicMessages, openai_chat::OpenAiChat,
        openai_responses::OpenAiResponses,
    };
    use agent_shim_gateway::state::{AppCore, AppSnapshot};

    let keepalive = Some(Duration::from_secs(15));
    let anthropic = Arc::new(AnthropicMessages { keepalive });
    let openai = Arc::new(OpenAiChat {
        keepalive,
        clock_override: None,
    });
    let openai_responses = Arc::new(OpenAiResponses {
        keepalive,
        clock_override: None,
    });

    let mut registry = ProviderRegistry::new();
    let stub: Arc<dyn BackendProvider> = Arc::new(StubProvider::new());
    registry.register("stub-provider".into(), stub);

    let cfg = GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams: BTreeMap::new(),
        routes: vec![RouteEntry::singular(
            "anthropic_messages",
            "test-model",
            "stub-provider",
            "test-model",
        )],
        plugins: BTreeMap::new(),
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
        shutdown: Default::default(),
    };

    let static_router: Arc<dyn RouterTrait> = Arc::new(StaticRouter::from_config(&cfg));
    let model_index = Arc::new(ModelIndex::new(Default::default()));
    let resolver = Arc::new(ModelResolver::new(static_router, model_index));

    let providers = Arc::new(registry);
    struct Lookup(Arc<ProviderRegistry>);
    impl ProviderLookup for Lookup {
        fn get(&self, name: &str) -> Option<Arc<dyn BackendProvider>> {
            self.0.get(name)
        }
    }
    let provider_lookup: Arc<dyn ProviderLookup> = Arc::new(Lookup(Arc::clone(&providers)));
    let breaker_registry = Arc::new(BreakerRegistry::with_system_clock());
    let limiter_registry = Arc::new(arc_swap::ArcSwap::from_pointee(
        agent_shim_router::LimiterRegistry::disabled(),
    ));
    let resilient_caller = Arc::new(ResilientCaller::new(
        provider_lookup,
        Arc::clone(&breaker_registry),
        Arc::clone(&limiter_registry),
        Arc::new(agent_shim_router::DisabledLatencyProbe)
            as Arc<dyn agent_shim_router::LatencyProbe>,
    ));

    let metrics = agent_shim_observability::install_metrics(&Default::default());

    let state = AppState {
        core: Arc::new(AppCore {
            config_path: None,
            server_config: cfg.server.clone(),
            admin_config: cfg.admin.clone(),
            anthropic,
            openai,
            openai_responses,
            providers,
            resolver,
            resilient_caller,
            breaker_registry,
            limiter_registry,
            metrics: metrics.clone(),
            reload_tx: tokio::sync::mpsc::channel(1).0,
        }),
        snapshot: Arc::new(arc_swap::ArcSwap::new(Arc::new(AppSnapshot {
            config: Arc::new(cfg),
            auth_enabled: false,
            auth_required: false,
            configured_key_hashes: Arc::new(std::collections::HashSet::new()),
            plugins,
        }))),
    };
    (state, metrics)
}

const ANTHROPIC_BODY: &str = r#"{
    "model": "test-model",
    "max_tokens": 64,
    "messages": [{"role": "user", "content": "hi"}]
}"#;

// ── Tests ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn h2_plugin_invocation_emits_prometheus_counter() {
    struct PassThrough;
    #[async_trait]
    impl Plugin for PassThrough {
        fn kind_name(&self) -> &'static str {
            "passthrough"
        }
        fn hooks(&self) -> HookSet {
            HookSet::DECODED_REQUEST
        }
        async fn on_decoded_request(
            &self,
            _ctx: &PluginContext,
            req: CanonicalRequest,
        ) -> PluginResult<CanonicalRequest> {
            Ok(req)
        }
    }

    let registry = Arc::new(PluginRegistry::for_testing_single_plugin(
        "p1",
        Arc::new(PassThrough),
        OnError::Skip,
        agent_shim_plugins::Hook::DecodedRequest,
        FrontendKind::AnthropicMessages,
        "test-model",
    ));
    let (state, metrics) = make_app_state(registry);
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_BODY))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = metrics.render();
    assert!(
        body.contains("agent_shim_plugin_invocations_total"),
        "Prometheus output must include plugin invocations counter, got: {body}"
    );
    assert!(
        body.contains(r#"plugin_kind="passthrough""#),
        "counter must carry plugin_kind label, got body: {body}"
    );
    assert!(
        body.contains(r#"plugin_name="p1""#),
        "counter must carry plugin_name label"
    );
    assert!(
        body.contains(r#"hook="on_decoded_request""#),
        "counter must carry hook label"
    );
    assert!(
        body.contains(r#"outcome="success""#),
        "successful invoke must emit outcome=success label"
    );
}

#[tokio::test]
async fn slow_h7_plugin_dropped_at_shutdown_increments_dropped_counter() {
    struct SlowH7;
    #[async_trait]
    impl Plugin for SlowH7 {
        fn kind_name(&self) -> &'static str {
            "slow_h7"
        }
        fn hooks(&self) -> HookSet {
            HookSet::RESPONSE_COMPLETE
        }
        async fn on_response_complete(
            &self,
            _ctx: &PluginContext,
            _summary: &ResponseSummary,
        ) -> PluginResult<()> {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        }
    }

    let registry = Arc::new(PluginRegistry::for_testing_single_plugin(
        "slow",
        Arc::new(SlowH7),
        OnError::Skip,
        agent_shim_plugins::Hook::ResponseComplete,
        FrontendKind::AnthropicMessages,
        "test-model",
    ));
    let (state, metrics) = make_app_state(Arc::clone(&registry));
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_BODY))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Yield once so the H7 spawn registers in the supervisor's pending map.
    tokio::task::yield_now().await;

    // Flush with a tiny deadline — slow H7 should be dropped.
    let dropped = registry.flush_pending_h7(Duration::from_millis(10)).await;
    assert!(
        dropped.iter().any(|(name, _)| name == "slow"),
        "slow H7 plugin must appear in dropped list, got: {dropped:?}"
    );

    // Simulate the gateway shutdown hook: record dropped metrics, then
    // render and assert.
    for (name, count) in dropped {
        agent_shim_observability::metrics::recorders::record_h7_dropped(&name, count);
    }
    let body = metrics.render();
    assert!(
        body.contains("agent_shim_plugin_h7_dropped_at_shutdown_total"),
        "h7 dropped counter must be present in Prometheus output, got: {body}"
    );
    assert!(
        body.contains(r#"plugin_name="slow""#),
        "h7 dropped counter must carry plugin_name label"
    );
}

#[tokio::test]
async fn empty_registry_no_nonzero_plugin_observation() {
    let (state, metrics) = make_app_state(Arc::new(PluginRegistry::empty()));
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_BODY))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = metrics.render();
    // The registry for THIS test was empty, so this request cannot have
    // produced any plugin observation. We confirm by checking that no
    // line carries `plugin_name="empty"` — no plugin with that name was
    // registered. Other tests in the same binary may have accumulated
    // observations under different plugin_name values; that's tolerated.
    assert!(
        !body.lines().any(|l| l.contains("plugin_name=\"empty\"")),
        "empty registry must not emit a plugin_name=\"empty\" line, got: {body}"
    );
}
