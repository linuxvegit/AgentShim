//! Phase 7 P07: Plugin hot-reload integration tests.
//!
//! Scenarios:
//!   1. `reload_swaps_plugins_atomically` (T7): happy path — reload
//!      replaces a plugin's behavior; subsequent requests see new behavior.
//!   2. `reload_with_bad_plugin_config_rejected` (T8, follow-up): Layer-B
//!      failure rejects the entire reload; old plugins remain active.
//!   3. `reload_swap_isolates_in_flight_requests` (T9, follow-up): an
//!      in-flight request bound to the old snapshot sees the old plugin;
//!      a sibling new request sees the new.

use std::sync::Arc;

use agent_shim_config::GatewayConfig;
use agent_shim_core::ContentBlock;
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use async_trait::async_trait;
use tokio::sync::Barrier;

const REQUEST_BODY: &str = r#"{
    "model": "test-model",
    "max_tokens": 16,
    "messages": [{"role": "user", "content": "ping"}]
}"#;

const UPSTREAM_OAI_RESPONSE: &str = r#"{
    "id": "chatcmpl-test",
    "object": "chat.completion",
    "created": 1700000000,
    "model": "test-model",
    "choices": [{
        "index": 0,
        "message": {"role": "assistant", "content": "ok"},
        "finish_reason": "stop"
    }],
    "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
}"#;

/// Build a YAML config string that routes `test-model` to the given
/// mockito upstream, with a single `pii_scrubber` plugin on H2 that
/// replaces the string "ping" with the given marker.
///
/// Using `pii_scrubber` (rather than a custom plugin) keeps the test
/// independent of any test-only plugin registration — `pii_scrubber` is
/// a built-in factory that the live `AppState::new` path will find.
fn yaml_v1(upstream_url: &str, marker: &str) -> String {
    format!(
        r#"
server:
  bind: 127.0.0.1
  port: 18080
  keepalive_secs: 15
upstreams:
  mocked:
    type: open_ai_compatible
    base_url: "{upstream_url}"
    api_key: "k"
    tier: standard
routes:
  - frontend: anthropic_messages
    model: test-model
    upstream: mocked
    upstream_model: test-model
    plugins:
      on_decoded_request:
        - marker_inj
plugins:
  marker_inj:
    type: pii_scrubber
    on_error: fail
    config:
      inbound:
        - name: marker
          pattern: "ping"
          replacement: "{marker}"
"#
    )
}

#[tokio::test]
async fn reload_swaps_plugins_atomically() {
    let mut upstream = mockito::Server::new_async().await;

    // First request: upstream should see "MARKER-A".
    let mock_a = upstream
        .mock("POST", "/v1/chat/completions")
        .match_body(mockito::Matcher::Regex(r"MARKER-A".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(UPSTREAM_OAI_RESPONSE)
        .expect(1)
        .create_async()
        .await;
    // Second request (after reload): upstream should see "MARKER-B".
    let mock_b = upstream
        .mock("POST", "/v1/chat/completions")
        .match_body(mockito::Matcher::Regex(r"MARKER-B".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(UPSTREAM_OAI_RESPONSE)
        .expect(1)
        .create_async()
        .await;

    let cfg_a: GatewayConfig =
        serde_yaml::from_str(&yaml_v1(&upstream.url(), "MARKER-A")).expect("yaml parses");
    let (state, _rx) = AppState::new(cfg_a).await.expect("AppState::new");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server_state = state.clone();
    let _server = tokio::spawn(async move {
        let _ = run_on_listener(listener, server_state, async {
            let _ = rx.await;
        })
        .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Request 1 → should hit the MARKER-A mock.
    let url = format!("http://{}/v1/messages", addr);
    let resp_a = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .body(REQUEST_BODY)
        .send()
        .await
        .expect("send 1");
    assert_eq!(resp_a.status(), 200);

    // Now reload with MARKER-B config.
    let yaml_b = yaml_v1(&upstream.url(), "MARKER-B");
    let outcome = agent_shim_gateway::commands::serve::handle_reload(
        &state,
        agent_shim_gateway::reload_trigger::ReloadSource::Yaml(yaml_b),
    )
    .await;
    assert!(
        matches!(
            outcome,
            agent_shim_gateway::reload_trigger::ReloadOutcome::Ok(_)
        ),
        "expected Ok reload, got {outcome:?}",
    );

    // Request 2 → should hit the MARKER-B mock.
    let resp_b = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .body(REQUEST_BODY)
        .send()
        .await
        .expect("send 2");
    assert_eq!(resp_b.status(), 200);

    let _ = tx.send(());

    mock_a.assert_async().await;
    mock_b.assert_async().await;
}

#[tokio::test]
async fn reload_with_bad_plugin_config_rejected() {
    let mut upstream = mockito::Server::new_async().await;

    // Both requests (before AND after the bad reload) must show MARKER-A,
    // proving the reload was atomically rejected and the OLD plugin is
    // still active.
    let mock_a = upstream
        .mock("POST", "/v1/chat/completions")
        .match_body(mockito::Matcher::Regex(r"MARKER-A".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(UPSTREAM_OAI_RESPONSE)
        .expect(2)
        .create_async()
        .await;

    let cfg_a: GatewayConfig =
        serde_yaml::from_str(&yaml_v1(&upstream.url(), "MARKER-A")).expect("yaml parses");
    let (state, _rx) = AppState::new(cfg_a).await.expect("AppState::new");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server_state = state.clone();
    let _server = tokio::spawn(async move {
        let _ = run_on_listener(listener, server_state, async {
            let _ = rx.await;
        })
        .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url = format!("http://{}/v1/messages", addr);

    // Request 1: hits MARKER-A mock.
    let resp1 = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .body(REQUEST_BODY)
        .send()
        .await
        .expect("send 1");
    assert_eq!(resp1.status(), 200);

    // Build a bad YAML: same server/upstream as v1, but the route points
    // at a plugin whose kind doesn't exist. PluginRegistry::build will
    // fail with RegistryBuildError::UnknownKind.
    //
    // Must mirror v1's server.{bind,port} and upstream set (those are
    // immutable across reload — see T7 notes).
    let bad_yaml = format!(
        r#"
server:
  bind: 127.0.0.1
  port: 18080
  keepalive_secs: 15
upstreams:
  mocked:
    type: open_ai_compatible
    base_url: "{}"
    api_key: "k"
    tier: standard
routes:
  - frontend: anthropic_messages
    model: test-model
    upstream: mocked
    upstream_model: test-model
    plugins:
      on_decoded_request:
        - bad
plugins:
  bad:
    type: this_kind_does_not_exist
    on_error: fail
    config: {{}}
"#,
        upstream.url()
    );

    let outcome = agent_shim_gateway::commands::serve::handle_reload(
        &state,
        agent_shim_gateway::reload_trigger::ReloadSource::Yaml(bad_yaml),
    )
    .await;
    match &outcome {
        agent_shim_gateway::reload_trigger::ReloadOutcome::PluginValidation(msg) => {
            assert!(
                msg.contains("this_kind_does_not_exist"),
                "expected error to mention the unknown kind, got: {msg}"
            );
        }
        other => panic!("expected PluginValidation, got {other:?}"),
    }

    // Request 2: after the rejected reload, still uses the OLD plugin
    // (MARKER-A injection still active).
    let resp2 = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .body(REQUEST_BODY)
        .send()
        .await
        .expect("send 2");
    assert_eq!(resp2.status(), 200);

    let _ = tx.send(());

    // mock_a.expect(2) requires both requests reach this mock with MARKER-A —
    // i.e. NEITHER ever saw a MARKER-B (which doesn't exist) AND the bad
    // reload didn't partially install some other behavior.
    mock_a.assert_async().await;
}

// ── T9: in-flight isolation ────────────────────────────────────────────

/// Test-only plugin used by `reload_swap_isolates_in_flight_requests`.
/// On `on_decoded_request`, it prepends `marker` to the first user message's
/// first text block, then awaits a barrier so the test can drive the timing.
struct BarrierMarkerPlugin {
    marker: &'static str,
    barrier: Arc<Barrier>,
}

#[async_trait]
impl agent_shim_plugins::Plugin for BarrierMarkerPlugin {
    fn kind_name(&self) -> &'static str {
        "barrier_marker"
    }
    fn hooks(&self) -> agent_shim_plugins::HookSet {
        agent_shim_plugins::HookSet::DECODED_REQUEST
    }
    async fn on_decoded_request(
        &self,
        _ctx: &agent_shim_plugins::PluginContext,
        mut req: agent_shim_core::CanonicalRequest,
    ) -> agent_shim_plugins::PluginResult<agent_shim_core::CanonicalRequest> {
        if let Some(msg) = req.messages.first_mut() {
            if let Some(ContentBlock::Text(t)) = msg.content.first_mut() {
                t.text = format!("{}-{}", self.marker, t.text);
            }
        }
        // Wait on the barrier so the test can synchronously schedule a
        // reload + R2 between this mutation and the upstream call.
        self.barrier.wait().await;
        Ok(req)
    }
}

/// T9: hot-reload swaps plugins atomically with respect to in-flight
/// requests. A request that captured the snapshot BEFORE the swap must
/// continue to see the ORIGINAL plugin chain through completion, even
/// though a sibling request issued AFTER the swap sees the RELOADED
/// plugin chain.
///
/// The property under test is exactly the reason plugins live on
/// `AppSnapshot` (rather than `AppCore`): `pipeline::dispatch` does
/// `let snapshot = state.snapshot.load_full();` once, and the resulting
/// `Arc<AppSnapshot>` keeps the old `PluginRegistry` alive through R1's
/// lifetime regardless of what `state.snapshot.store(...)` does next.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reload_swap_isolates_in_flight_requests() {
    use agent_shim_core::FrontendKind;
    use agent_shim_plugins::{Hook, HookTimeouts, OnError, PluginRegistry};

    let mut upstream = mockito::Server::new_async().await;

    // R1 must reach upstream with `ORIGINAL-` prefix (proves it saw the
    // ORIGINAL plugin even though reload happened mid-flight).
    let mock_original = upstream
        .mock("POST", "/v1/chat/completions")
        .match_body(mockito::Matcher::Regex(r"ORIGINAL-".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(UPSTREAM_OAI_RESPONSE)
        .expect(1)
        .create_async()
        .await;
    // R2 must reach upstream with `RELOADED-` prefix (sent after reload).
    let mock_reloaded = upstream
        .mock("POST", "/v1/chat/completions")
        .match_body(mockito::Matcher::Regex(r"RELOADED-".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(UPSTREAM_OAI_RESPONSE)
        .expect(1)
        .create_async()
        .await;

    // Barrier of size 2: plugin + main thread.
    let barrier = Arc::new(Barrier::new(2));

    // The barrier-aware plugin blocks for as long as the test schedules
    // the reload + R2 dance — easily past the default 50 ms H2 timeout.
    // Use a 10 s timeout so the supervisor doesn't pre-empt the wait.
    let generous_timeouts = HookTimeouts::uniform(10_000);

    // ORIGINAL registry: barrier-aware plugin (size 2 — waits for the
    // main thread to release it after performing the snapshot swap).
    let original_plugin = Arc::new(BarrierMarkerPlugin {
        marker: "ORIGINAL",
        barrier: barrier.clone(),
    });
    let original_registry = Arc::new(PluginRegistry::for_testing_single_plugin_with_timeouts(
        "barrier_marker",
        original_plugin,
        OnError::Fail,
        Hook::DecodedRequest,
        FrontendKind::AnthropicMessages,
        "test-model",
        generous_timeouts,
    ));

    // Minimal config: no plugin block needed because we inject the
    // registry directly via `new_with_plugins`. Server port is a non-zero
    // placeholder (T7 lesson: `port: 0` fails `validate_for_reload`).
    let cfg_yaml = format!(
        r#"
server:
  bind: 127.0.0.1
  port: 18080
  keepalive_secs: 15
upstreams:
  mocked:
    type: open_ai_compatible
    base_url: "{}"
    api_key: "k"
    tier: standard
routes:
  - frontend: anthropic_messages
    model: test-model
    upstream: mocked
    upstream_model: test-model
"#,
        upstream.url()
    );
    let cfg: GatewayConfig = serde_yaml::from_str(&cfg_yaml).expect("yaml parses");
    let (state, _rx) = AppState::new_with_plugins(cfg, original_registry)
        .await
        .expect("AppState::new_with_plugins");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server_state = state.clone();
    let _server = tokio::spawn(async move {
        let _ = run_on_listener(listener, server_state, async {
            let _ = rx.await;
        })
        .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Spawn R1: enters dispatch, captures the snapshot, runs the ORIGINAL
    // plugin (which prepends ORIGINAL- and then blocks on the barrier).
    let url = format!("http://{}/v1/messages", addr);
    let url_r1 = url.clone();
    let r1_handle = tokio::spawn(async move {
        reqwest::Client::new()
            .post(&url_r1)
            .header("content-type", "application/json")
            .body(REQUEST_BODY)
            .send()
            .await
            .expect("R1 send")
            .status()
    });
    // Give R1 enough time to enter dispatch and hit the barrier.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Build the RELOADED registry. Its plugin has a barrier of size 1
    // (unused — `barrier.wait()` on a size-1 barrier returns immediately,
    // so R2 sees a non-blocking ORIGINAL-style mutation only with the
    // RELOADED marker).
    let unused_barrier = Arc::new(Barrier::new(1));
    let reloaded_plugin = Arc::new(BarrierMarkerPlugin {
        marker: "RELOADED",
        barrier: unused_barrier,
    });
    let reloaded_registry = Arc::new(PluginRegistry::for_testing_single_plugin_with_timeouts(
        "barrier_marker",
        reloaded_plugin,
        OnError::Fail,
        Hook::DecodedRequest,
        FrontendKind::AnthropicMessages,
        "test-model",
        generous_timeouts,
    ));

    // Atomic snapshot swap, bypassing `handle_reload`'s YAML path (which
    // can only construct registries from the configured factory set, not
    // a hand-rolled test plugin). The semantics are the same as
    // production: `ArcSwap::store` publishes a new snapshot; the
    // refcount-positive `Arc<AppSnapshot>` captured at the top of
    // `pipeline::dispatch` in R1's stack keeps the old plugins alive
    // until R1 completes.
    let current = state.snapshot.load_full();
    let new_snap = Arc::new(agent_shim_gateway::state::AppSnapshot {
        config: current.config.clone(),
        auth_enabled: current.auth_enabled,
        auth_required: current.auth_required,
        configured_key_hashes: current.configured_key_hashes.clone(),
        plugins: reloaded_registry,
    });
    state.snapshot.store(new_snap);

    // R2: sent AFTER the swap; must see RELOADED.
    let r2_status = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .body(REQUEST_BODY)
        .send()
        .await
        .expect("R2 send")
        .status();
    assert_eq!(r2_status, 200);

    // Release the barrier so R1 can resume and reach the upstream with
    // the ORIGINAL-prefixed body.
    barrier.wait().await;
    let r1_status = r1_handle.await.expect("R1 join");
    assert_eq!(r1_status, 200);

    let _ = tx.send(());

    // The mockito mocks prove the property:
    // - mock_original.expect(1) was hit once → R1 reached upstream with
    //   ORIGINAL- prefix even though reload happened mid-flight.
    // - mock_reloaded.expect(1) was hit once → R2 (sent after reload)
    //   reached upstream with RELOADED- prefix.
    mock_original.assert_async().await;
    mock_reloaded.assert_async().await;
}
