//! Gateway-level smoke test: Anthropic Messages inbound -> GlmProvider ->
//! mock Zhipu endpoint. Verifies:
//!   - the outbound request body has `thinking:{type:"enabled"}` injected,
//!   - the outbound request body has no top-level `reasoning_effort`,
//!   - upstream `reasoning_content` deltas surface as canonical Reasoning
//!     events and are re-encoded as Anthropic `thinking_delta` SSE events.
//!
//! This is the GLM-specific contract. Cross-dialect lifecycle behaviour is
//! already exercised by the protocol-tests harness; this test focuses on
//! the two GLM quirks plus end-to-end wiring.
//!
//! Frontend choice: `anthropic_messages` is used here because the OpenAI
//! Chat encoder intentionally drops canonical `ReasoningDelta` events (see
//! `crates/frontends/src/openai_chat/encode_stream.rs::ReasoningDelta`),
//! while the Anthropic encoder re-emits them as `content_block_delta`
//! events with a `thinking_delta` payload. To assert that the upstream
//! `reasoning_content` actually made it through the canonical pipeline
//! end-to-end, we need a frontend that surfaces reasoning back to clients.

use std::collections::BTreeMap;

use agent_shim_config::{
    schema::{GlmUpstream, LoggingConfig, RouteEntry, ServerConfig, Tier, UpstreamConfig},
    GatewayConfig, Secret,
};
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use futures::StreamExt;
use tokio::net::TcpListener;

fn make_config(upstream_url: &str) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert(
        "glm".to_string(),
        UpstreamConfig::Glm(GlmUpstream {
            base_url: upstream_url.to_string(),
            api_key: Secret::new("test-key"),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
            tier: Tier::Standard,
            cost: None,
            p95_latency_budget_ms: None,
        }),
    );

    GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams,
        routes: vec![RouteEntry::singular(
            "anthropic_messages",
            "claude-glm-test",
            "glm",
            "glm-5.1",
        )],
        plugins: BTreeMap::new(),
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
        shutdown: Default::default(),
        validation: Default::default(),
    }
}

async fn spawn_gateway(
    upstream_url: &str,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let cfg = make_config(upstream_url);
    let (state, _reload_rx) = AppState::new(cfg).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        run_on_listener(listener, state, async {
            let _ = rx.await;
        })
        .await
        .unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (addr, tx)
}

#[tokio::test]
async fn glm_request_carries_thinking_and_no_reasoning_effort_and_streams_reasoning() {
    let mut mock_server = mockito::Server::new_async().await;

    // SSE body: one reasoning_content delta, one content delta, finish stop.
    // Mirror the deepseek SSE shape since glm reuses the deepseek parser.
    let sse_body = concat!(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,",
        "\"model\":\"glm-5.1\",\"choices\":[{\"index\":0,\"delta\":",
        "{\"role\":\"assistant\",\"reasoning_content\":\"thinking step\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,",
        "\"model\":\"glm-5.1\",\"choices\":[{\"index\":0,\"delta\":",
        "{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    // mockito matches the outbound body against PartialJsonString. If the
    // GLM encoder failed to inject `thinking:{type:"enabled"}` or failed
    // to forward the correct model, the mock won't match and the
    // `mock.assert_async().await` at the bottom of the test will fail.
    //
    // The absence of top-level `reasoning_effort` is verified by the
    // unit tests in `crates/providers/src/glm/request.rs`. Replicating
    // the absence check here would require a body capture callback that
    // this project does not currently expose; the structural guarantee
    // comes from the request::build implementation plus its own tests.
    let mock = mock_server
        .mock("POST", "/chat/completions")
        .match_body(mockito::Matcher::AllOf(vec![
            // Positive: thinking type must be enabled (effort = High, not Minimal).
            mockito::Matcher::PartialJsonString(r#"{"thinking":{"type":"enabled"}}"#.to_string()),
            // Positive: model name forwarded correctly to upstream.
            mockito::Matcher::PartialJsonString(r#"{"model":"glm-5.1"}"#.to_string()),
        ]))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(sse_body)
        .expect(1)
        .create_async()
        .await;

    let (addr, tx) = spawn_gateway(&mock_server.url()).await;

    // Inbound Anthropic Messages request carries `output_config.effort=high`
    // (the 2026-05-28 effort vocabulary). The GLM encoder must:
    //   1) inject `thinking:{type:"enabled"}` (High != Minimal), and
    //   2) NOT emit a top-level `reasoning_effort` field (Zhipu would 400).
    // The mock body matcher asserts (1). Item (2) is covered by the
    // provider-level unit tests in `glm/request.rs`.
    let request_body = serde_json::json!({
        "model": "claude-glm-test",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
        "stream": true,
        "output_config": {"effort": "high"}
    });

    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/messages", addr))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .json(&request_body)
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        content_type.contains("text/event-stream"),
        "expected SSE content-type, got: {content_type}"
    );

    // Drain the streaming body, looking for evidence that the upstream
    // `reasoning_content` made it through the canonical pipeline. The
    // Anthropic Messages encoder turns canonical `ReasoningDelta` events
    // into `content_block_delta` SSE events with a `thinking_delta`
    // payload (see `anthropic_messages/encode_stream.rs` ReasoningDelta arm).
    let mut accumulated = String::new();
    let mut byte_stream = resp.bytes_stream();
    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk.expect("stream chunk error");
        let text = String::from_utf8_lossy(&chunk);
        accumulated.push_str(&text);
        if accumulated.contains("message_stop") {
            break;
        }
    }

    assert!(
        accumulated.contains("thinking step"),
        "upstream reasoning_content should surface in outbound SSE; got: {accumulated}"
    );
    assert!(
        accumulated.contains("thinking_delta"),
        "expected canonical Reasoning to be re-encoded as Anthropic thinking_delta; got: {accumulated}"
    );
    assert!(
        accumulated.contains("hello"),
        "upstream content should appear in outbound SSE; got: {accumulated}"
    );
    assert!(
        accumulated.contains("message_stop"),
        "expected message_stop terminator; got: {accumulated}"
    );

    mock.assert_async().await;
    let _ = tx.send(());
}
