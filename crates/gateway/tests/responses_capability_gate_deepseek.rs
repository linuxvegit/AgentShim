//! Plan 04 T5: end-to-end test that the vision capability gate rejects
//! Responses requests carrying `input_image` parts when the routed
//! upstream is text-only (DeepSeek-style: `vision = false`).
//!
//! The Responses dialect joins the matrix already covered by
//! `vision_capability_mismatch.rs` (Anthropic + OpenAI Chat). The gate
//! itself is shared (`pipeline::check_capabilities`); this test pins the
//! HTTP-level contract for the third frontend dialect:
//!   * Provider's `complete()` is never reached (panicking stub proves
//!     the gate fires before upstream dispatch).
//!   * Response is HTTP 400.
//!   * Body matches the OpenAI-style error envelope:
//!     `{"error":{"message":...,"type":"invalid_request_error","code":"capability_mismatch"}}`.
//!     Per `handlers::mod::HandlerError::CapabilityMismatch`, the
//!     Responses dialect deliberately shares the OpenAI Chat envelope,
//!     so the assertion shape mirrors the OAI Chat case.
//!
//! "DeepSeek-style" here means a stub whose capabilities mirror DeepSeek
//! (streaming: true, tool_use: false, vision: false, json_mode: false).
//! The same `TextOnlyStubProvider` shape is reused from
//! `vision_capability_mismatch.rs`; it lives here as a copy because the
//! struct is private to that test crate target.

use std::sync::Arc;
use std::time::Duration;

use agent_shim_config::GatewayConfig;
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
    BreakerRegistry, ModelResolver, ProviderLookup, ResilientCaller, Router as RouterTrait,
    StaticRouter,
};
use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::util::ServiceExt;

// ── Stub provider ─────────────────────────────────────────────────────

/// DeepSeek-style provider stub: `vision = false`, panics in `complete`
/// so the test fails loudly if the gate ever fails open. Mirror of the
/// stub in `vision_capability_mismatch.rs` — duplicated, not shared,
/// because integration tests are separate crate targets and the original
/// type is private to its file.
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
        // Responses request through to the upstream — exactly the
        // regression Plan 04 T5 must prevent.
        panic!(
            "TextOnlyStubProvider::complete called — the capability gate must reject \
             input_image-bearing Responses requests BEFORE provider dispatch (Plan 04 T5)"
        );
    }
}

// ── AppState builder ──────────────────────────────────────────────────

/// Build a minimal AppState wired only with the text-only stub provider,
/// with a single Responses-frontend route. Mirrors `make_app_state` in
/// `vision_capability_mismatch.rs` but the route table contains only the
/// `openai_responses` entry — that's the dialect under test, and keeping
/// the table minimal makes a stray dispatch easier to diagnose.
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

    use agent_shim_config::RouteEntry;
    let cfg = GatewayConfig {
        server: Default::default(),
        logging: Default::default(),
        upstreams: Default::default(),
        routes: vec![RouteEntry::singular(
            "openai_responses",
            "text-only-model",
            "text-only-stub",
            "text-only-model",
        )],
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
    };
    let static_router: Arc<dyn RouterTrait> = Arc::new(StaticRouter::from_config(&cfg));
    let model_index = Arc::new(ModelIndex::new(Default::default()));
    let resolver = Arc::new(ModelResolver::new(static_router, model_index));

    // Sanity check: route lookup hits the stub. Avoids a cryptic 404 if
    // the route table ever drifts out from under the test.
    let chain = resolver
        .resolve(FrontendKind::OpenAiResponses, "text-only-model")
        .expect("test setup: openai_responses route must resolve");
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
    let limiter_registry = Arc::new(agent_shim_router::LimiterRegistry::disabled());
    let resilient_caller = Arc::new(ResilientCaller::new(
        provider_lookup,
        Arc::clone(&breaker_registry),
        Arc::clone(&limiter_registry),
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
        }))),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

/// OpenAI Responses-style request body carrying an `input_image` part.
/// Wire shape per `frontends::openai_responses::wire`:
///   * `input` is a message array (`InputField::Messages`).
///   * Content parts use `input_text` / `input_image` (snake_case).
///   * `input_image` carries `image_url` as a STRING (not an object) —
///     this is the Responses dialect's specific divergence from Chat.
const RESPONSES_IMAGE_BODY: &str = r#"{
    "model": "text-only-model",
    "input": [
        {
            "role": "user",
            "content": [
                {"type": "input_text", "text": "describe this"},
                {"type": "input_image", "image_url": "https://example.com/cat.png"}
            ]
        }
    ]
}"#;

#[tokio::test]
async fn responses_image_against_text_only_provider_rejected_before_upstream() {
    let app = build_router(make_app_state());

    let req = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .body(Body::from(RESPONSES_IMAGE_BODY))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();

    // Status: HTTP 400 — capability gate must surface as a client error.
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "capability mismatch must surface as HTTP 400 for Responses dialect"
    );

    let body_bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();

    // Body shape: OpenAI Responses inherits the OpenAI error envelope:
    //   {"error":{"message":...,"type":"invalid_request_error","code":"capability_mismatch"}}
    // (See `HandlerError::CapabilityMismatch` in `gateway::handlers::mod`.)
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "capability_mismatch");
    let msg = body["error"]["message"]
        .as_str()
        .expect("error.message must be a string");
    assert!(
        msg.contains("vision"),
        "expected message to mention vision, got: {msg}"
    );
}

#[tokio::test]
async fn responses_text_only_request_passes_gate() {
    // Negative sanity: a text-only Responses request must NOT be
    // rejected by the gate. The stub panics in `complete()`, so the
    // ONLY way this test passes is if the gate let the request through
    // to dispatch. This guards against a regression that would
    // accidentally reject every Responses request to a text-only
    // provider — strictly worse than letting an image slip through.
    let app = build_router(make_app_state());

    let req = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"model":"text-only-model","input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
        ))
        .unwrap();

    // tokio::task::spawn lets us observe the stub's panic without
    // aborting the test process.
    let join = tokio::task::spawn(async move { app.oneshot(req).await });
    let result = join.await;

    assert!(
        result.is_err() && result.unwrap_err().is_panic(),
        "expected stub provider to be reached (panic) — gate must not reject text-only Responses request"
    );
}
