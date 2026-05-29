//! Regression coverage for passthrough `AdmissionTicket` first-byte
//! semantics on raw byte streams.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use agent_shim_config::{
    schema::{LoggingConfig, RouteEntry, ServerConfig},
    BreakerConfig, GatewayConfig,
};
use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream, FrontendKind};
use agent_shim_frontends::{
    anthropic_messages::AnthropicMessages, openai_chat::OpenAiChat,
    openai_responses::OpenAiResponses,
};
use agent_shim_gateway::server::build_router;
use agent_shim_gateway::state::{AppCore, AppSnapshot, AppState};
use agent_shim_providers::{
    BackendProvider, ProviderCapabilities, ProviderError, ProviderRegistry, RawByteStream,
};
use agent_shim_router::model_index::ModelIndex;
use agent_shim_router::{
    BreakerDecision, BreakerPolicy, BreakerRegistry, ModelResolver, ProviderLookup,
    ResilientCaller, StaticRouter,
};
use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::util::ServiceExt;

struct EmptyPassthroughProvider {
    capabilities: ProviderCapabilities,
}

impl EmptyPassthroughProvider {
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
impl BackendProvider for EmptyPassthroughProvider {
    fn name(&self) -> &'static str {
        "empty-passthrough"
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    async fn complete(
        &self,
        _req: CanonicalRequest,
        _target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError> {
        panic!("canonical complete must not run when proxy_raw returns a passthrough stream")
    }

    async fn proxy_raw(
        &self,
        _body: bytes::Bytes,
        _target: BackendTarget,
        frontend_kind: FrontendKind,
    ) -> Result<Option<(String, RawByteStream)>, ProviderError> {
        assert_eq!(frontend_kind, FrontendKind::OpenAiResponses);
        let stream: RawByteStream =
            Box::pin(futures::stream::empty::<Result<bytes::Bytes, reqwest::Error>>());
        Ok(Some(("text/event-stream".to_string(), stream)))
    }
}

fn open_on_single_failure_breaker() -> BreakerConfig {
    BreakerConfig {
        enabled: true,
        failure_threshold_pct: 100,
        min_requests: 1,
        window_secs: 60,
        open_cooldown_secs: 30,
    }
}

fn make_app_state() -> (AppState, BreakerPolicy) {
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
    registry.register(
        "empty-passthrough".into(),
        Arc::new(EmptyPassthroughProvider::new()) as Arc<dyn BackendProvider>,
    );

    let breaker = open_on_single_failure_breaker();
    let policy = BreakerPolicy::from(&breaker);
    let mut route = RouteEntry::singular(
        "openai_responses",
        "gpt-4o",
        "empty-passthrough",
        "gpt-4o-up",
    );
    route.breaker = breaker;

    let cfg = GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams: BTreeMap::new(),
        routes: vec![route],
        plugins: BTreeMap::new(),
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
        shutdown: Default::default(),
        validation: Default::default(),
    };

    let static_router = Arc::new(StaticRouter::from_config(&cfg));
    let model_index = Arc::new(ModelIndex::new(Default::default()));
    let resolver = Arc::new(ModelResolver::new(static_router, model_index));
    let chain = resolver
        .resolve(FrontendKind::OpenAiResponses, "gpt-4o")
        .expect("test setup: responses route must resolve");
    assert_eq!(chain[0].provider, "empty-passthrough");
    assert_eq!(chain[0].model, "gpt-4o-up");

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
    let latency_probe = Arc::new(agent_shim_router::DisabledLatencyProbe)
        as Arc<dyn agent_shim_router::LatencyProbe>;
    let admission = Arc::new(agent_shim_router::Admission::new(
        Arc::clone(&limiter_registry),
        Arc::clone(&breaker_registry),
        Arc::clone(&provider_lookup),
        Arc::clone(&latency_probe),
    ));
    let resilient_caller = Arc::new(ResilientCaller::new(
        provider_lookup,
        Arc::clone(&breaker_registry),
        Arc::clone(&limiter_registry),
        Arc::clone(&latency_probe),
    ));

    (
        AppState {
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
                admission,
                breaker_registry,
                limiter_registry,
                metrics: agent_shim_observability::install_metrics(&Default::default()),
                reload_tx: tokio::sync::mpsc::channel(1).0,
            }),
            snapshot: Arc::new(arc_swap::ArcSwap::new(Arc::new(AppSnapshot {
                config: Arc::new(cfg),
                auth_enabled: false,
                auth_required: false,
                configured_key_hashes: Arc::new(std::collections::HashSet::new()),
                plugins: Arc::new(agent_shim_plugins::PluginRegistry::empty()),
            }))),
        },
        policy,
    )
}

#[tokio::test]
async fn admission_ticket_byte_stream_records_failure_on_empty_passthrough_stream() {
    let (state, policy) = make_app_state();
    let breaker_registry = Arc::clone(&state.core.breaker_registry);
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"model":"gpt-4o","input":"hi","stream":true}"#,
        ))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);

    assert_eq!(
        breaker_registry.decision("empty-passthrough", "gpt-4o-up", &policy),
        BreakerDecision::Skip,
        "empty passthrough stream must record as abandoned-probe failure"
    );
}
