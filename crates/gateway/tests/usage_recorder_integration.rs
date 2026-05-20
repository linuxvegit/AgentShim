//! Phase 7 P06a T9: end-to-end integration test for usage_recorder.
//!
//! Validates the full production path:
//! YAML → AppState::new → builtin_plugins() → PluginRegistry::build →
//! axum router → HTTP request (anthropic frontend → openai upstream) →
//! H7 fires on the unary success path → usage_recorder emits structured log line.
//!
//! Uses a mockito upstream that returns a complete OpenAI-shaped chat
//! response so the unary pipeline reaches its H7 emission point.

use agent_shim_config::GatewayConfig;
use agent_shim_gateway::{server::run_on_listener, state::AppState};

fn yaml_for(upstream_url: &str) -> String {
    format!(
        r#"
server:
  bind: 127.0.0.1
  port: 0
  keepalive_secs: 15
upstreams:
  mocked:
    type: open_ai_compatible
    base_url: "{upstream_url}"
    api_key: "test-key"
    tier: standard
routes:
  - frontend: anthropic_messages
    model: test-model
    upstream: mocked
    upstream_model: test-model
    plugins:
      on_response_complete:
        - usage_logger
plugins:
  usage_logger:
    type: usage_recorder
    config:
      sink: log
      level: info
"#
    )
}

const REQUEST_BODY: &str = r#"{
    "model": "test-model",
    "max_tokens": 16,
    "messages": [{"role": "user", "content": "hi"}]
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

#[tokio::test]
#[tracing_test::traced_test]
async fn usage_recorder_emits_on_h7_via_full_build_path() {
    let mut upstream = mockito::Server::new_async().await;
    let _mock = upstream
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(UPSTREAM_OAI_RESPONSE)
        .create_async()
        .await;

    let cfg: GatewayConfig = serde_yaml::from_str(&yaml_for(&upstream.url())).expect("yaml parse");
    let (state, _reload_rx) = AppState::new(cfg).await.expect("AppState::new");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        let _ = run_on_listener(listener, state, async {
            let _ = rx.await;
        })
        .await;
    });
    // Brief wait for server to start accepting.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url = format!("http://{}/v1/messages", addr);
    let _ = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .body(REQUEST_BODY)
        .send()
        .await;

    // Give the H7 spawn enough time to run and emit.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Trigger graceful shutdown.
    let _ = tx.send(());
    let _ = server_handle.await;

    // Verify the usage_recorder emission.
    assert!(
        logs_contain("plugin.kind=\"usage_recorder\""),
        "usage_recorder must emit the plugin.kind=usage_recorder field"
    );
    assert!(
        logs_contain("usage.upstream_status=\"success\""),
        "H7 fires on the unary success path"
    );
}
