//! Plan 04 T9: end-to-end test that the vision capability gate rejects
//! image-bearing requests against text-only providers BEFORE any network
//! call.
//!
//! Acceptance criteria from the plan:
//!   * Gateway never makes a network call to the upstream (exercised here
//!     by registering a stub provider whose `complete()` panics — if it
//!     fires, the gate failed open and the test crashes loudly).
//!   * Response is HTTP 400.
//!   * Response body matches the inbound frontend's error envelope shape
//!     (Anthropic-style for `/v1/messages`, OpenAI-style for
//!     `/v1/chat/completions`).
//!
//! The two endpoints are tested in parallel sub-tests rather than a
//! shared one because the assertion shape genuinely differs — pulling
//! both into one helper would just hide the dialect-specific JSON keys
//! we actually need to verify.

use std::sync::Arc;
use std::time::Duration;

use agent_shim_config::{schema::UpstreamRef, BreakerConfig, GatewayConfig, RetryConfig};
use agent_shim_core::{
    BackendTarget, CanonicalRequest, CanonicalStream, FrontendKind, RoutePolicy,
};
use agent_shim_frontends::{
    anthropic_messages::AnthropicMessages, openai_chat::OpenAiChat,
    openai_responses::OpenAiResponses,
};
use agent_shim_gateway::server::build_router;
use agent_shim_gateway::state::{AppCore, AppSnapshot, AppState};
use agent_shim_providers::{
    BackendProvider, ProviderCapabilities, ProviderError, ProviderRegistry,
};
use agent_shim_router::model_index::ModelIndex;
use agent_shim_router::{
    BreakerDecision, BreakerPolicy, BreakerRegistry, ModelResolver, ProviderLookup,
    ResilientCaller, StaticRouter,
};
use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::util::ServiceExt;

// ── Stub provider ─────────────────────────────────────────────────────

/// A provider with `vision = false` that explicitly panics if `complete`
/// is reached. The whole point of T1's gate is that we never get this
/// far when an inbound image is routed at a text-only backend; the
/// panic guarantees the test fails loudly the moment that contract is
/// broken.
struct TextOnlyStubProvider {
    capabilities: ProviderCapabilities,
}

impl TextOnlyStubProvider {
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
impl BackendProvider for TextOnlyStubProvider {
    fn name(&self) -> &'static str {
        "text-only-stub"
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    async fn complete(
        &self,
        _req: CanonicalRequest,
        _target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError> {
        // Reaching this means the capability gate let an image-bearing
        // request through to the upstream — exactly the regression the
        // gate must prevent. Panic so the test fails hard rather than
        // silently passing on a broken response shape.
        panic!(
            "TextOnlyStubProvider::complete called — the capability gate must reject \
             image-bearing requests BEFORE provider dispatch (Plan 04 T1)"
        );
    }
}

/// Vision-capable chain head used by the fallback regression. Its breaker is
/// pre-opened, so dispatch must never reach this provider during the test.
struct VisionStubProvider {
    capabilities: ProviderCapabilities,
}

impl VisionStubProvider {
    fn new() -> Self {
        Self {
            capabilities: ProviderCapabilities {
                streaming: true,
                tool_use: false,
                vision: true,
                json_mode: false,
                accepts_xhigh: false,
            },
        }
    }
}

#[async_trait]
impl BackendProvider for VisionStubProvider {
    fn name(&self) -> &'static str {
        "vision-stub"
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    async fn complete(
        &self,
        _req: CanonicalRequest,
        _target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError> {
        panic!("VisionStubProvider::complete called — its breaker should be open")
    }
}

// ── AppState builder ──────────────────────────────────────────────────

/// Build a minimal AppState wired only with the text-only stub provider.
/// We bypass `AppState::new` (which builds providers from a YAML config)
/// because the stub isn't a real upstream type. The route table maps
/// both `/v1/messages` (Anthropic) and `/v1/chat/completions` (OpenAI
/// Chat) to the same stub, since the gate fires on either inbound
/// dialect.
fn make_app_state() -> AppState {
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
    let stub: Arc<dyn BackendProvider> = Arc::new(TextOnlyStubProvider::new());
    registry.register("text-only-stub".into(), stub);

    // Hand-built routes for both inbound dialects → stub provider.
    // Using StaticRouter::from_config keeps the test exercising the
    // real lookup path; `static_routes::tests` shows the same shape.
    use agent_shim_config::RouteEntry;
    let cfg = GatewayConfig {
        server: Default::default(),
        logging: Default::default(),
        upstreams: Default::default(),
        routes: vec![
            RouteEntry::singular(
                "anthropic_messages",
                "text-only-model",
                "text-only-stub",
                "text-only-model",
            ),
            RouteEntry::singular(
                "openai_chat",
                "text-only-model",
                "text-only-stub",
                "text-only-model",
            ),
        ],
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
    let static_router = Arc::new(StaticRouter::from_config(&cfg));
    let model_index = Arc::new(ModelIndex::new(Default::default()));
    let resolver = Arc::new(ModelResolver::new(static_router, model_index));

    // Sanity check: verify our hand-built route resolves to the stub.
    // This avoids debugging a cryptic 404 if the route table ever drifts
    // out from under the test.
    let chain = resolver
        .resolve(FrontendKind::AnthropicMessages, "text-only-model")
        .expect("test setup: anthropic route must resolve");
    assert_eq!(chain[0].provider, "text-only-stub");
    let _ = RoutePolicy::default(); // silence unused-import lint when policy isn't touched

    // Build the resilient caller against the same provider registry the
    // gateway exposes to the pipeline. The stub is wrapped in an
    // `Arc<ProviderRegistry>` so the lookup adapter shares its instance.
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
            // The test bypasses AppState::new so the channel sender has
            // no real consumer; that's fine because this test never
            // triggers a reload. Plan 04 P04 T2.
            reload_tx: tokio::sync::mpsc::channel(1).0,
        }),
        snapshot: Arc::new(arc_swap::ArcSwap::new(Arc::new(AppSnapshot {
            config: Arc::new(cfg),
            auth_enabled: false,
            auth_required: false,
            configured_key_hashes: Arc::new(std::collections::HashSet::new()),
            // Plan 07 P04 T7: tests inherit the empty-registry fast path.
            plugins: Arc::new(agent_shim_plugins::PluginRegistry::empty()),
        }))),
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

fn make_fallback_app_state_with_open_head_breaker() -> AppState {
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
        "vision-head".into(),
        Arc::new(VisionStubProvider::new()) as Arc<dyn BackendProvider>,
    );
    registry.register(
        "text-fallback".into(),
        Arc::new(TextOnlyStubProvider::new()) as Arc<dyn BackendProvider>,
    );

    let breaker = open_on_single_failure_breaker();
    let cfg = GatewayConfig {
        server: Default::default(),
        logging: Default::default(),
        upstreams: Default::default(),
        routes: vec![agent_shim_config::RouteEntry {
            frontend: "openai_chat".to_string(),
            model: "fallback-vision-model".to_string(),
            upstream: None,
            upstream_model: None,
            upstreams: vec![
                UpstreamRef {
                    name: "vision-head".to_string(),
                    model: "gpt-4o".to_string(),
                },
                UpstreamRef {
                    name: "text-fallback".to_string(),
                    model: "gpt-4o".to_string(),
                },
            ],
            reasoning_effort: None,
            anthropic_beta: None,
            retry: RetryConfig {
                max_attempts: 1,
                initial_backoff_ms: 1,
                multiplier: 2.0,
                jitter_pct: 0.0,
                total_budget_ms: 100,
                retry_on: vec![
                    "network".into(),
                    "upstream_5xx".into(),
                    "upstream_429".into(),
                ],
            },
            breaker: breaker.clone(),
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
    let static_router = Arc::new(StaticRouter::from_config(&cfg));
    let model_index = Arc::new(ModelIndex::new(Default::default()));
    let resolver = Arc::new(ModelResolver::new(static_router, model_index));
    let chain = resolver
        .resolve(FrontendKind::OpenAiChat, "fallback-vision-model")
        .expect("test setup: fallback route must resolve");
    assert_eq!(chain[0].provider, "vision-head");
    assert_eq!(chain[1].provider, "text-fallback");

    let providers = Arc::new(registry);
    struct Lookup(Arc<ProviderRegistry>);
    impl ProviderLookup for Lookup {
        fn get(&self, name: &str) -> Option<Arc<dyn BackendProvider>> {
            self.0.get(name)
        }
    }
    let provider_lookup: Arc<dyn ProviderLookup> = Arc::new(Lookup(Arc::clone(&providers)));
    let breaker_registry = Arc::new(BreakerRegistry::with_system_clock());
    let breaker_policy = BreakerPolicy::from(&breaker);
    breaker_registry.record("vision-head", "gpt-4o", false, &breaker_policy);
    assert_eq!(
        breaker_registry.decision("vision-head", "gpt-4o", &breaker_policy),
        BreakerDecision::Skip
    );
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
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

/// Anthropic-style request body carrying a single image block. Uses the
/// Anthropic wire shape (decoded by `frontends::anthropic_messages`).
const ANTHROPIC_IMAGE_BODY: &str = r#"{
    "model": "text-only-model",
    "max_tokens": 256,
    "messages": [{
        "role": "user",
        "content": [
            {"type": "text", "text": "describe this"},
            {"type": "image", "source": {"type": "url", "url": "https://example.com/cat.png"}}
        ]
    }]
}"#;

/// OpenAI Chat-style request body carrying an image_url part.
const OPENAI_IMAGE_BODY: &str = r#"{
    "model": "text-only-model",
    "messages": [{
        "role": "user",
        "content": [
            {"type": "text", "text": "describe this"},
            {"type": "image_url", "image_url": {"url": "https://example.com/cat.png"}}
        ]
    }]
}"#;

#[tokio::test]
async fn anthropic_image_against_text_only_provider_rejected_before_upstream() {
    let app = build_router(make_app_state());

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_IMAGE_BODY))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();

    // Status: HTTP 400 (Plan 04 T1).
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "capability mismatch must surface as HTTP 400"
    );

    let body_bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();

    // Body shape: Anthropic envelope.
    //   {"type":"error","error":{"type":"invalid_request_error","message":"..."}}
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    let msg = body["error"]["message"].as_str().expect("message present");
    assert!(
        msg.contains("vision"),
        "expected message to mention vision, got: {msg}"
    );
}

#[tokio::test]
async fn openai_image_against_text_only_provider_rejected_before_upstream() {
    let app = build_router(make_app_state());

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(OPENAI_IMAGE_BODY))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body_bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();

    // Body shape: OpenAI envelope.
    //   {"error":{"message":...,"type":"invalid_request_error","code":"capability_mismatch"}}
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "capability_mismatch");
    let msg = body["error"]["message"].as_str().expect("message present");
    assert!(
        msg.contains("vision"),
        "expected message to mention vision, got: {msg}"
    );
}

#[tokio::test]
async fn vision_capability_mismatch_rejects_text_only_fallback_even_when_head_breaker_open() {
    let app = build_router(make_fallback_app_state_with_open_head_breaker());

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{
                "model": "fallback-vision-model",
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "describe this"},
                        {"type": "image_url", "image_url": {"url": "https://example.com/cat.png"}}
                    ]
                }]
            }"#,
        ))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "capability mismatch must reject before breaker fallback reaches text-only provider"
    );

    let body_bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["error"]["code"], "capability_mismatch");
    let msg = body["error"]["message"].as_str().expect("message present");
    assert!(
        msg.contains("vision"),
        "expected message to mention vision, got: {msg}"
    );
}

#[tokio::test]
async fn anthropic_text_only_request_passes_gate() {
    // Sanity check: a text-only request against the same text-only
    // provider should NOT be rejected by the gate. (It will still
    // panic in the stub's `complete`, since the stub is wired to fail
    // on any actual dispatch — but the panic happens AFTER the gate
    // would have returned. We catch the panic to assert the gate
    // didn't fire.)
    //
    // Why this matters: the gate is an "image AND no-vision" gate, NOT
    // a "no-vision provider can't be hit" gate. A regression that
    // accidentally rejected every text request to a no-vision
    // provider would be much worse than a bug that let images through.
    let app = build_router(make_app_state());

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"model":"text-only-model","max_tokens":256,"messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .unwrap();

    // The stub panics inside `complete()` to prove the gate is the only
    // thing that ever rejects. For the negative test (gate must NOT
    // fire) we'd need to catch that panic. tokio::task::spawn lets us
    // observe a panic without aborting the test process.
    let join = tokio::task::spawn(async move { app.oneshot(req).await });
    let result = join.await;

    // The task panicked inside the stub — that's the SUCCESS condition
    // for this negative test, because the only way to reach the stub
    // is to pass the capability gate.
    assert!(
        result.is_err() && result.unwrap_err().is_panic(),
        "expected stub provider to be reached (panic) — gate must not reject text-only request"
    );
}
