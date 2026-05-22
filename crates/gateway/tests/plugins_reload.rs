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

use agent_shim_config::GatewayConfig;
use agent_shim_gateway::{server::run_on_listener, state::AppState};

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
