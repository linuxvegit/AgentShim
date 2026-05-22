//! Plan 07 P07 T4: integration test for the H5 (`on_stream_event`) hook
//! exercised end-to-end over a real socket.
//!
//! Why this file exists
//! --------------------
//! `plugins_pipeline.rs` covers H2/H3/H7 (and the empty-registry baseline)
//! using `axum::Router::oneshot`. That style works because those hooks fire
//! before or after the SSE body is produced and the body can be collected
//! in one `to_bytes()` call. H5 is different — the plugin chain wraps the
//! CanonicalStream that feeds the encoder, and we want to verify the wire
//! shape the client actually sees AFTER axum's hyper layer + SSE framing.
//!
//! To do that we follow the `pii_scrubber_integration.rs` /
//! `usage_recorder_integration.rs` pattern: bind a real `TcpListener`,
//! spawn `run_on_listener`, and use `eventsource-client` as the SSE
//! consumer. `eventsource-client` configured with
//! `.method("POST".into()).body(...)` issues a POST with our request body
//! and yields parsed `event:` / `data:` pairs.
//!
//! Scope (T4): one test — "every other event is dropped".
//!   - The H5 plugin returns `Ok(vec![event])` on even indices and
//!     `Ok(vec![])` (drop) on odd indices.
//!   - The `CapturingStubProvider` emits 6 canonical events
//!     (ResponseStart, MessageStart, ContentBlockStart, TextDelta,
//!     ContentBlockStop, MessageStop). With alternating drop the forwarded
//!     canonical events are {0: ResponseStart, 2: ContentBlockStart,
//!     4: ContentBlockStop} — three events.
//!   - Anthropic encoding maps those three canonical events to:
//!     ResponseStart → `message_start` + `ping`
//!     ContentBlockStart → `content_block_start`
//!     ContentBlockStop → `content_block_stop`
//!     i.e. 4 SSE events on the wire. (MessageStop is dropped, so the
//!     encoder never emits message_delta/message_stop — the stream simply
//!     terminates when the upstream is exhausted.)
//!   - The test asserts the forwarded event TYPES match exactly and the
//!     unforwarded ones (e.g. `content_block_delta`, `message_stop`) are
//!     absent. That uniquely pins H5 behaviour: if alternation skipped the
//!     wrong indices, or if every event flowed through, the assertion
//!     would fail.
//!
//! T5 will add the mid-stream `Aborted`/`Failed` error-frame test in a
//! follow-up commit.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_shim_config::{
    schema::{LoggingConfig, RouteEntry, ServerConfig},
    GatewayConfig,
};
use agent_shim_core::{
    BackendTarget, CanonicalRequest, CanonicalStream, ContentBlockKind, FrontendKind, MessageRole,
    ResponseId, StopReason, StreamEvent,
};
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use agent_shim_plugins::{HookSet, OnError, Plugin, PluginContext, PluginRegistry, PluginResult};
use agent_shim_providers::{
    BackendProvider, ProviderCapabilities, ProviderError, ProviderRegistry,
};
use agent_shim_router::model_index::ModelIndex;
use agent_shim_router::{
    BreakerRegistry, ModelResolver, ProviderLookup, ResilientCaller, Router as RouterTrait,
    StaticRouter,
};
use async_trait::async_trait;
use eventsource_client::{Client as _, ClientBuilder, ReconnectOptions, SSE};
use futures::StreamExt;

// ── Stub provider that streams a fixed sequence of canonical events ─────

/// Mirrors `CapturingStubProvider` from `plugins_pipeline.rs`. The captured
/// request field isn't needed here — H5 doesn't mutate the request — but
/// keeping the shape identical means future H5 tests can adopt this helper
/// without re-deriving it.
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
/// the `CapturingStubProvider`, with a single Anthropic + `claude-test`
/// route. Cribs from `plugins_pipeline.rs::make_app_state` — the upstream
/// helper is private to that test crate, but the surface it relies on
/// (`AppCore`, `AppSnapshot`) is `pub` in the gateway lib.
fn make_app_state(
    plugins: Arc<PluginRegistry>,
    captured: Arc<tokio::sync::Mutex<Option<CanonicalRequest>>>,
) -> AppState {
    use agent_shim_frontends::{
        anthropic_messages::AnthropicMessages, openai_chat::OpenAiChat,
        openai_responses::OpenAiResponses,
    };
    use agent_shim_gateway::state::{AppCore, AppSnapshot};

    // Keepalive MUST be None for this test: the Anthropic SSE encoder
    // merges a `take_while(!done)` ping stream when keepalive is `Some`,
    // and `done` is only flipped to true inside the `ResponseStop` arm —
    // which our plugin can drop. Without keepalive the encoder's output
    // stream simply ends when the upstream `CanonicalStream` is exhausted,
    // which is exactly the termination signal we want the client to see.
    let keepalive: Option<Duration> = None;
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

/// One-plugin registry bound to the Anthropic + `claude-test` route.
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

const ANTHROPIC_STREAMING_BODY: &str = r#"{
    "model": "claude-test",
    "max_tokens": 256,
    "stream": true,
    "messages": [{"role": "user", "content": "hello"}]
}"#;

// ── The plugin under test ───────────────────────────────────────────────

/// H5 plugin: forwards even-indexed events and drops odd-indexed ones.
/// `seen` is an `AtomicUsize` rather than a `Mutex<usize>` because every
/// invocation needs only a single fetch-and-increment; no read-modify-write
/// pattern that would benefit from a critical section.
struct EveryOtherEventSkipper {
    seen: Arc<AtomicUsize>,
}

#[async_trait]
impl Plugin for EveryOtherEventSkipper {
    fn kind_name(&self) -> &'static str {
        "every_other_event_skipper"
    }

    fn hooks(&self) -> HookSet {
        HookSet::STREAM_EVENT
    }

    async fn on_stream_event(
        &self,
        _ctx: &PluginContext,
        event: StreamEvent,
    ) -> PluginResult<Vec<StreamEvent>> {
        // Snapshot then increment — index 0 is the first event.
        let idx = self.seen.fetch_add(1, Ordering::SeqCst);
        if idx % 2 == 0 {
            Ok(vec![event])
        } else {
            // Drop the event entirely — see trait_def.rs: "empty Vec = drop".
            Ok(Vec::new())
        }
    }
}

// ── Test ────────────────────────────────────────────────────────────────

/// T4 §1: an H5 plugin that drops alternate events MUST produce a wire
/// stream containing only the SSE events derived from the forwarded
/// canonical events.
///
/// Forwarded canonical events (indices 0, 2, 4):
///   ResponseStart → `message_start` + `ping`
///   ContentBlockStart → `content_block_start`
///   ContentBlockStop → `content_block_stop`
///
/// Dropped canonical events (indices 1, 3, 5) include MessageStop, so the
/// encoder's `message_delta` + `message_stop` are NEVER emitted. The
/// stream ends when the upstream is exhausted — `eventsource-client` is
/// configured with `reconnect(false)` so it surfaces the disconnect as
/// the end of the stream rather than re-connecting and looping forever.
#[tokio::test]
async fn h5_drops_alternate_stream_events() {
    let seen = Arc::new(AtomicUsize::new(0));
    let plugin = Arc::new(EveryOtherEventSkipper {
        seen: Arc::clone(&seen),
    });
    let registry = registry_with_one_plugin(
        "every_other_event_skipper",
        plugin,
        OnError::Skip,
        agent_shim_plugins::Hook::StreamEvent,
    );
    let captured = Arc::new(tokio::sync::Mutex::new(None));
    let state = make_app_state(registry, captured);

    // Bind an ephemeral socket, spawn the gateway against it, and capture
    // the bound port so the SSE client can reach it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        let _ = run_on_listener(listener, state, async {
            let _ = shutdown_rx.await;
        })
        .await;
    });
    // Tiny yield so axum::serve has actually started accepting before we
    // dial. 50 ms is what the pii_scrubber test uses for the same reason.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Disable auto-reconnect: when the gateway closes the connection after
    // the (short) stream ends, we want the client to surface that as EOF
    // instead of looping forever. retry_initial(false) covers the edge
    // case where the very first connect fails — we'd rather see a hard
    // error than a silent retry.
    let reconnect = ReconnectOptions::reconnect(false)
        .retry_initial(false)
        .build();
    let url = format!("http://{}/v1/messages", addr);
    let client = ClientBuilder::for_url(&url)
        .expect("valid URL")
        .method("POST".to_string())
        .body(ANTHROPIC_STREAMING_BODY.to_string())
        .header("content-type", "application/json")
        .expect("set content-type")
        .header("accept", "text/event-stream")
        .expect("set accept")
        .reconnect(reconnect)
        .build_http();

    // Collect every `event:` name (we don't need the `data:` payload for
    // this assertion — the event-name sequence alone uniquely identifies
    // which canonical events made it through the H5 chain).
    let mut event_names: Vec<String> = Vec::new();
    let mut stream = client.stream();
    // 2 s ceiling: each event arrives well under 100 ms in practice; the
    // ceiling exists only so a hung test fails noisily instead of stalling
    // CI for 60+ s. Lifted from the pii_scrubber timing convention.
    let collection_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = collection_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(SSE::Event(ev)))) => event_names.push(ev.event_type),
            Ok(Some(Ok(SSE::Comment(_)))) => {
                // Ping comments fall under this arm.
            }
            Ok(Some(Ok(SSE::Connected(_)))) => {
                // Initial connection — not a wire event.
            }
            Ok(Some(Err(_))) | Ok(None) => break, // Stream closed.
            Err(_) => break,                      // Per-iteration deadline.
        }
    }

    // Tear down the gateway BEFORE asserting so a failed assertion doesn't
    // leak the server task into subsequent tests.
    let _ = shutdown_tx.send(());
    let _ = server_handle.await;

    // The plugin must have been invoked exactly 6 times — once per
    // upstream-emitted canonical event.
    assert_eq!(
        seen.load(Ordering::SeqCst),
        6,
        "H5 plugin should have seen all 6 upstream events"
    );

    // Forwarded canonical events 0,2,4 expand to these 4 SSE events.
    // (`message_start` comes from ResponseStart, plus a sibling `ping`.)
    let expected = vec![
        "message_start".to_string(),
        "ping".to_string(),
        "content_block_start".to_string(),
        "content_block_stop".to_string(),
    ];
    assert_eq!(
        event_names, expected,
        "only events derived from forwarded canonical events should appear on the wire"
    );

    // Negative assertions: the dropped canonical events MUST NOT produce
    // wire frames. These are subsumed by the equality check above, but
    // making them explicit pins exactly which behaviour is being verified
    // and gives clearer diagnostics when the test regresses.
    assert!(
        !event_names.iter().any(|n| n == "content_block_delta"),
        "TextDelta was dropped (index 3) → no content_block_delta on wire"
    );
    assert!(
        !event_names.iter().any(|n| n == "message_stop"),
        "MessageStop was dropped (index 5) → no message_stop on wire"
    );
    assert!(
        !event_names.iter().any(|n| n == "message_delta"),
        "MessageStop being dropped also suppresses the trailing message_delta"
    );
}
