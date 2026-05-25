//! Plan 07 P04 T12: integration tests for the four `PluginRegistry`
//! hook anchors wired into `pipeline.rs::dispatch_inner`.
//!
//! These tests exercise the gateway HTTP surface end-to-end. We avoid
//! mockito here — the plugin hooks fire BEFORE the upstream is contacted
//! (H2/H3) or DURING the streamed response (H5/H7). A hand-built
//! `BackendProvider` stub with deterministic output is sufficient and
//! keeps each test self-contained.
//!
//! Test matrix:
//!   1. `empty_registry_request_returns_200` — the default `empty()`
//!      registry is a zero-overhead identity path (frozen-core invariant
//!      from §9: pre-P04 byte-identical parity).
//!   2. `h2_plugin_fires_and_modifies_prompt` — H2 sees the canonical
//!      request, mutates it, and the change reaches the stub provider.
//!   3. `h3_plugin_fires_with_backend_target` — H3 receives the resolved
//!      `BackendTarget`.
//!   4. `h7_plugin_fires_after_unary_response` — H7 spawns after the
//!      unary response is collected; eventually observes the captured
//!      `ResponseSummary`.
//!   5. `plugin_aborted_returns_400_anthropic_envelope` — `Aborted`
//!      surfaces as HTTP 400 with the Anthropic-shaped error envelope.
//!   6. `plugin_failed_returns_502_when_on_error_fail` — `Failed` with
//!      `OnError::Fail` surfaces as HTTP 502 (`plugin_failed` envelope).
//!   7. `plugin_protected_field_mutation_returns_502` — mutating `model`
//!      in H2 trips the protected-field guard and produces 502.
//!
//! H5 stream-event tests and mid-stream error frames are deferred to
//! P05 / P07 — they need precise SSE frame parsing infrastructure that
//! is overkill for the pipeline-integration acceptance criteria.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use agent_shim_config::{
    schema::{LoggingConfig, RouteEntry, ServerConfig},
    GatewayConfig,
};
use agent_shim_core::{
    BackendTarget, CanonicalRequest, CanonicalStream, ContentBlock, ContentBlockKind, FrontendKind,
    Message, MessageRole, ResponseId, StopReason, StreamEvent, TextBlock,
};
use agent_shim_gateway::{server::build_router, state::AppState};
use agent_shim_plugins::{
    HookSet, OnError, Plugin, PluginContext, PluginError, PluginRegistry, PluginResult,
    ResponseSummary,
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
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::util::ServiceExt;

// ── Stub provider that returns a canned, deterministic response ─────────

/// A provider that returns a single TextDelta + MessageStop, plus
/// captures the inbound `CanonicalRequest` for later inspection.
struct CapturingStubProvider {
    capabilities: ProviderCapabilities,
    last_request: Arc<tokio::sync::Mutex<Option<CanonicalRequest>>>,
}

impl CapturingStubProvider {
    fn new(captured: Arc<tokio::sync::Mutex<Option<CanonicalRequest>>>) -> Self {
        Self {
            capabilities: ProviderCapabilities {
                streaming: true,
                tool_use: false,
                vision: false,
                json_mode: false,
            },
            last_request: captured,
        }
    }
}

#[async_trait]
impl BackendProvider for CapturingStubProvider {
    fn name(&self) -> &'static str {
        "capturing-stub"
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    async fn complete(
        &self,
        req: CanonicalRequest,
        _target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError> {
        // Capture the request as observed at the provider boundary — i.e.
        // AFTER H2/H3 plugins have run.
        *self.last_request.lock().await = Some(req);
        let events: Vec<Result<StreamEvent, agent_shim_core::StreamError>> = vec![
            Ok(StreamEvent::ResponseStart {
                id: ResponseId("test-resp-1".to_string()),
                model: "claude-test".to_string(),
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
                text: "hello".to_string(),
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

// ── AppState builder ────────────────────────────────────────────────────

/// Build an AppState pointing at a hand-built provider registry containing
/// the `CapturingStubProvider`, with a single route for Anthropic →
/// `claude-test`. The `plugins` registry is the parameter; pass
/// `PluginRegistry::empty()` for the zero-overhead baseline.
fn make_app_state(
    plugins: Arc<PluginRegistry>,
    captured: Arc<tokio::sync::Mutex<Option<CanonicalRequest>>>,
) -> AppState {
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
    let stub: Arc<dyn BackendProvider> = Arc::new(CapturingStubProvider::new(captured));
    registry.register("capturing-stub".into(), stub);

    let cfg = GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams: BTreeMap::new(),
        routes: vec![RouteEntry::singular(
            "anthropic_messages",
            "claude-test",
            "capturing-stub",
            "claude-test",
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
            reload_tx: tokio::sync::mpsc::channel(1).0,
        }),
        snapshot: Arc::new(arc_swap::ArcSwap::new(Arc::new(AppSnapshot {
            config: Arc::new(cfg),
            auth_enabled: false,
            auth_required: false,
            configured_key_hashes: Arc::new(std::collections::HashSet::new()),
            plugins,
        }))),
    }
}

/// Compact helper: build a registry with one plugin attached to one hook
/// on the Anthropic + `claude-test` route. Used in every test below.
fn registry_with_one_plugin(
    name: &str,
    plugin: Arc<dyn Plugin>,
    on_error: OnError,
    hook: agent_shim_plugins::Hook,
) -> Arc<PluginRegistry> {
    Arc::new(PluginRegistry::for_testing_single_plugin(
        name,
        plugin,
        on_error,
        hook,
        FrontendKind::AnthropicMessages,
        "claude-test",
    ))
}

const ANTHROPIC_BODY: &str = r#"{
    "model": "claude-test",
    "max_tokens": 256,
    "messages": [{"role": "user", "content": "hello"}]
}"#;

// ── Tests ──────────────────────────────────────────────────────────────

/// T12 §1: empty registry = identity. The default `PluginRegistry::empty()`
/// path must not perturb the request or response — this verifies the
/// frozen-core invariant from spec §9.
#[tokio::test]
async fn empty_registry_request_returns_200() {
    let captured = Arc::new(tokio::sync::Mutex::new(None));
    let state = make_app_state(Arc::new(PluginRegistry::empty()), Arc::clone(&captured));
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_BODY))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "empty registry must yield 200"
    );
    // Capturing provider was invoked — the empty path did NOT intercept.
    assert!(
        captured.lock().await.is_some(),
        "provider was hit (no plugin short-circuited the request)"
    );
}

/// T12 §2: H2 plugin mutates messages, change visible to provider.
#[tokio::test]
async fn h2_plugin_fires_and_modifies_prompt() {
    struct AppendMarker;
    #[async_trait]
    impl Plugin for AppendMarker {
        fn kind_name(&self) -> &'static str {
            "append_marker"
        }
        fn hooks(&self) -> HookSet {
            HookSet::DECODED_REQUEST
        }
        async fn on_decoded_request(
            &self,
            _ctx: &PluginContext,
            mut req: CanonicalRequest,
        ) -> PluginResult<CanonicalRequest> {
            req.messages
                .push(Message::user(vec![ContentBlock::Text(TextBlock {
                    text: "INJECTED_BY_H2".to_string(),
                    extensions: Default::default(),
                })]));
            Ok(req)
        }
    }

    let captured = Arc::new(tokio::sync::Mutex::new(None));
    let registry = registry_with_one_plugin(
        "append_marker",
        Arc::new(AppendMarker),
        OnError::Skip,
        agent_shim_plugins::Hook::DecodedRequest,
    );
    let state = make_app_state(registry, Arc::clone(&captured));
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_BODY))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let captured_req = captured.lock().await.clone().expect("provider was hit");
    // Original body has 1 message; H2 appended 1 → expect 2.
    assert_eq!(captured_req.messages.len(), 2, "H2 appended one message");
    let last = &captured_req.messages[1];
    let has_marker = last
        .content
        .iter()
        .any(|c| matches!(c, ContentBlock::Text(t) if t.text == "INJECTED_BY_H2"));
    assert!(has_marker, "H2's marker reached the provider");
}

/// T12 §3: H3 plugin observes the resolved `BackendTarget`.
#[tokio::test]
async fn h3_plugin_fires_with_backend_target() {
    struct RecordProvider {
        seen: Arc<tokio::sync::Mutex<Option<String>>>,
    }
    #[async_trait]
    impl Plugin for RecordProvider {
        fn kind_name(&self) -> &'static str {
            "record_provider"
        }
        fn hooks(&self) -> HookSet {
            HookSet::RESOLVED
        }
        async fn on_resolved(
            &self,
            _ctx: &PluginContext,
            req: CanonicalRequest,
            target: &BackendTarget,
        ) -> PluginResult<CanonicalRequest> {
            *self.seen.lock().await = Some(target.provider.clone());
            Ok(req)
        }
    }

    let seen = Arc::new(tokio::sync::Mutex::new(None));
    let captured = Arc::new(tokio::sync::Mutex::new(None));
    let registry = registry_with_one_plugin(
        "record_provider",
        Arc::new(RecordProvider {
            seen: Arc::clone(&seen),
        }),
        OnError::Skip,
        agent_shim_plugins::Hook::Resolved,
    );
    let state = make_app_state(registry, captured);
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_BODY))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let recorded = seen.lock().await.clone();
    assert_eq!(
        recorded,
        Some("capturing-stub".to_string()),
        "H3 received the configured upstream provider name"
    );
}

/// T12 §4: H7 plugin fires after the unary response is collected.
#[tokio::test]
async fn h7_plugin_fires_after_unary_response() {
    struct RecordElapsed {
        captured: Arc<tokio::sync::Mutex<Option<u64>>>,
    }
    #[async_trait]
    impl Plugin for RecordElapsed {
        fn kind_name(&self) -> &'static str {
            "record_elapsed"
        }
        fn hooks(&self) -> HookSet {
            HookSet::RESPONSE_COMPLETE
        }
        async fn on_response_complete(
            &self,
            _ctx: &PluginContext,
            summary: &ResponseSummary,
        ) -> PluginResult<()> {
            *self.captured.lock().await = Some(summary.elapsed_ms);
            Ok(())
        }
    }

    let captured = Arc::new(tokio::sync::Mutex::new(None));
    let provider_captured = Arc::new(tokio::sync::Mutex::new(None));
    let registry = registry_with_one_plugin(
        "record_elapsed",
        Arc::new(RecordElapsed {
            captured: Arc::clone(&captured),
        }),
        OnError::Skip,
        agent_shim_plugins::Hook::ResponseComplete,
    );
    // Keep a strong Arc alive for the duration of the test. P05 routes H7
    // tasks through the registry's supervisor JoinSet, so the registry must
    // outlive the spawned H7 task or it gets aborted on drop.
    let registry_keepalive = Arc::clone(&registry);
    let state = make_app_state(registry, provider_captured);
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_BODY))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // H7 is fire-and-forget via tokio::spawn. Yield a few times until the
    // captured value lands or 1 s elapses.
    for _ in 0..100 {
        if captured.lock().await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        captured.lock().await.is_some(),
        "H7 plugin fired and recorded elapsed_ms"
    );
    drop(registry_keepalive);
}

/// T12 §5: an H2 plugin returning `Aborted` produces HTTP 400 with the
/// Anthropic-shaped error envelope.
#[tokio::test]
async fn plugin_aborted_returns_400_anthropic_envelope() {
    struct AbortRequest;
    #[async_trait]
    impl Plugin for AbortRequest {
        fn kind_name(&self) -> &'static str {
            "abort_request"
        }
        fn hooks(&self) -> HookSet {
            HookSet::DECODED_REQUEST
        }
        async fn on_decoded_request(
            &self,
            _ctx: &PluginContext,
            _req: CanonicalRequest,
        ) -> PluginResult<CanonicalRequest> {
            Err(PluginError::Aborted {
                plugin: "abort_request".to_string(),
                reason: "test abort".to_string(),
            })
        }
    }

    let captured = Arc::new(tokio::sync::Mutex::new(None));
    let registry = registry_with_one_plugin(
        "abort_request",
        Arc::new(AbortRequest),
        OnError::Skip,
        agent_shim_plugins::Hook::DecodedRequest,
    );
    let state = make_app_state(registry, captured);
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_BODY))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "Aborted → 400");

    let body_bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    // Anthropic envelope shape.
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    let msg = body["error"]["message"].as_str().expect("message present");
    assert!(
        msg.contains("abort_request"),
        "envelope names the plugin, got: {msg}"
    );
}

/// T12 §6: an H2 plugin returning `Failed` with `OnError::Fail` produces
/// HTTP 502.
#[tokio::test]
async fn plugin_failed_returns_502_when_on_error_fail() {
    struct AlwaysFail;
    #[async_trait]
    impl Plugin for AlwaysFail {
        fn kind_name(&self) -> &'static str {
            "always_fail"
        }
        fn hooks(&self) -> HookSet {
            HookSet::DECODED_REQUEST
        }
        async fn on_decoded_request(
            &self,
            _ctx: &PluginContext,
            _req: CanonicalRequest,
        ) -> PluginResult<CanonicalRequest> {
            Err(PluginError::Failed {
                plugin: "always_fail".to_string(),
                hook: "on_decoded_request",
                message: "synthetic failure".to_string(),
            })
        }
    }

    let captured = Arc::new(tokio::sync::Mutex::new(None));
    let registry = registry_with_one_plugin(
        "always_fail",
        Arc::new(AlwaysFail),
        OnError::Fail,
        agent_shim_plugins::Hook::DecodedRequest,
    );
    let state = make_app_state(registry, captured);
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_BODY))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_GATEWAY,
        "Failed with on_error=Fail → 502"
    );
}

/// T12 §7: an H2 plugin that mutates `model` (a protected field) trips
/// the protected-field guard and produces HTTP 502 when `on_error: fail`.
#[tokio::test]
async fn plugin_protected_field_mutation_returns_502() {
    struct MutateModel;
    #[async_trait]
    impl Plugin for MutateModel {
        fn kind_name(&self) -> &'static str {
            "mutate_model"
        }
        fn hooks(&self) -> HookSet {
            HookSet::DECODED_REQUEST
        }
        async fn on_decoded_request(
            &self,
            _ctx: &PluginContext,
            mut req: CanonicalRequest,
        ) -> PluginResult<CanonicalRequest> {
            req.model = agent_shim_core::FrontendModel::from("hijacked-model");
            Ok(req)
        }
    }

    let captured = Arc::new(tokio::sync::Mutex::new(None));
    let registry = registry_with_one_plugin(
        "mutate_model",
        Arc::new(MutateModel),
        OnError::Fail,
        agent_shim_plugins::Hook::DecodedRequest,
    );
    let state = make_app_state(registry, captured);
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_BODY))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_GATEWAY,
        "ProtectedFieldMutated → 502"
    );
}

// All helpers live above. The plugins crate exposes
// `PluginRegistry::for_testing_single_plugin` so we don't need to
// reach into the private `RouteHookPlan` / `FrontendRoutePlans` types.

/// H2 plugin that always returns `PluginError::Failed`. Used together with
/// `OnError::Skip` to verify the error is silently swallowed: the chain
/// continues, the upstream provider sees the un-modified canonical request,
/// and the gateway returns 200.
struct AlwaysErrorPlugin;

#[async_trait]
impl Plugin for AlwaysErrorPlugin {
    fn kind_name(&self) -> &'static str {
        "always_error"
    }
    fn hooks(&self) -> HookSet {
        HookSet::DECODED_REQUEST
    }
    async fn on_decoded_request(
        &self,
        _ctx: &PluginContext,
        _req: CanonicalRequest,
    ) -> PluginResult<CanonicalRequest> {
        Err(PluginError::Failed {
            plugin: "always_error".to_string(),
            hook: "on_decoded_request",
            message: "synthetic skip test".to_string(),
        })
    }
}

/// §9 T6 coverage: `on_error: skip` swallows non-aborted plugin errors.
/// The plugin's `Err(PluginError::Failed{..})` is converted to a no-op by
/// the registry; the upstream provider sees the original request and the
/// HTTP response is 200.
#[tokio::test]
async fn plugin_skip_on_error_returns_200() {
    let captured = Arc::new(tokio::sync::Mutex::new(None));
    let registry = registry_with_one_plugin(
        "always_error",
        Arc::new(AlwaysErrorPlugin),
        OnError::Skip,
        agent_shim_plugins::Hook::DecodedRequest,
    );
    let state = make_app_state(registry, Arc::clone(&captured));
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_BODY))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "Skip should swallow the plugin error → 200, got {:?}",
        response.status(),
    );

    // The chain continued past the failing plugin, so the upstream was hit
    // and saw the un-modified request (the plugin attempted no mutation
    // before erroring).
    let captured_req = captured
        .lock()
        .await
        .clone()
        .expect("upstream provider was reached");
    assert_eq!(
        captured_req.messages.len(),
        1,
        "request reached upstream with its single original message intact"
    );
    let first = &captured_req.messages[0];
    let has_original_text = first
        .content
        .iter()
        .any(|c| matches!(c, ContentBlock::Text(t) if t.text.contains("hello")));
    assert!(
        has_original_text,
        "original prompt text reached upstream un-modified"
    );
}
