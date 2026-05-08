//! End-to-end smoke test: real axum gateway → mockito Anthropic upstream.
//!
//! Pipeline exercised end-to-end:
//!
//! ```text
//! HTTP POST /v1/responses (OpenAI Responses body)
//!   → OpenAiResponses::decode_request          [in-process]
//!   → Router::resolve                          [in-process]
//!   → AnthropicProvider::complete              [HTTP to mockito /v1/messages]
//!   → CanonicalStream
//!   → OpenAiResponses::encode_stream           [in-process]
//!   → SSE bytes back to HTTP client
//! ```
//!
//! This is the most realistic Plan 02 test: it spawns a real gateway bound to
//! a random local port, configures an `openai_responses → anthropic` route,
//! and asserts that a Responses-shaped HTTP request streams back a
//! Responses-shaped SSE response, while mockito confirms the upstream got hit
//! at the Anthropic Messages endpoint.
use std::collections::BTreeMap;

use agent_shim_config::{
    schema::{AnthropicUpstream, LoggingConfig, RouteEntry, ServerConfig, UpstreamConfig},
    GatewayConfig, Secret,
};
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use futures::StreamExt;
use tokio::net::TcpListener;

/// Anthropic SSE response body for a "Hello world" text completion. Mirrors
/// the shape `AnthropicProvider::parse_stream` consumes — see
/// `crates/providers/tests/anthropic_canonical_smoke.rs`.
const TEXT_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_text_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-4-7\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":2}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

fn make_config(upstream_url: &str) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert(
        "test-anthropic".to_string(),
        UpstreamConfig::Anthropic(AnthropicUpstream {
            base_url: upstream_url.to_string(),
            api_key: Secret::new("sk-ant-test"),
            anthropic_version: "2023-06-01".to_string(),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
        }),
    );

    GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams,
        routes: vec![RouteEntry {
            frontend: "openai_responses".to_string(),
            model: "claude-opus-4-7".to_string(),
            upstream: Some("test-anthropic".to_string()),
            upstream_model: Some("claude-opus-4-7".to_string()),
            upstreams: vec![],
            reasoning_effort: None,
            anthropic_beta: None,
            retry: Default::default(),
            breaker: Default::default(),
        }],
        copilot: None,
    }
}

async fn spawn_gateway(
    upstream_url: &str,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let cfg = make_config(upstream_url);
    let state = AppState::new(cfg).await;
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
async fn e2e_responses_to_anthropic_streaming() {
    let mut mock_server = mockito::Server::new_async().await;

    // The gateway must POST to /v1/messages with the Anthropic auth/version
    // headers. We do not pin the full body shape here — that is covered by
    // the provider-level canonical smoke tests — but we do require the model
    // be rewritten to the route's `upstream_model` and `stream=true` to be
    // set, which proves the OpenAI Responses → Canonical → Anthropic
    // translation chain ran end-to-end.
    let mock = mock_server
        .mock("POST", "/v1/messages")
        .match_header("x-api-key", "sk-ant-test")
        .match_header("anthropic-version", "2023-06-01")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::PartialJsonString(r#"{"model":"claude-opus-4-7"}"#.to_string()),
            mockito::Matcher::PartialJsonString(r#"{"stream":true}"#.to_string()),
        ]))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(TEXT_SSE)
        .expect(1)
        .create_async()
        .await;

    let (addr, tx) = spawn_gateway(&mock_server.url()).await;

    // Responses API accepts `input` as a string — the decoder converts it
    // into a single user message. See
    // `crates/frontends/src/openai_responses/wire.rs::InputField::Text`.
    let request_body = serde_json::json!({
        "model": "claude-opus-4-7",
        "input": "Hello",
        "stream": true
    });

    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/responses", addr))
        .header("content-type", "application/json")
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

    // Drain the streaming body until we see `response.completed`, which is the
    // tail event for the OpenAI Responses encoder.
    let mut accumulated = String::new();
    let mut byte_stream = resp.bytes_stream();
    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk.expect("stream chunk error");
        let text = String::from_utf8_lossy(&chunk);
        accumulated.push_str(&text);
        if accumulated.contains("event: response.completed") {
            break;
        }
    }

    // ── Lifecycle events: created → output_item.added → output_text.delta(s)
    //    → output_text.done → completed ────────────────────────────────────
    assert!(
        accumulated.contains("event: response.created"),
        "missing response.created\n{accumulated}"
    );
    assert!(
        accumulated.contains("event: response.output_item.added"),
        "missing response.output_item.added\n{accumulated}"
    );
    assert!(
        accumulated.contains("event: response.output_text.delta"),
        "missing response.output_text.delta\n{accumulated}"
    );
    assert!(
        accumulated.contains("Hello"),
        "expected upstream text 'Hello' in body\n{accumulated}"
    );
    assert!(
        accumulated.contains("event: response.completed"),
        "missing response.completed\n{accumulated}"
    );

    // ── response.completed must carry usage from the upstream message ─────
    // TEXT_SSE pins input_tokens=5 (message_start.usage) and output_tokens=2
    // (message_delta.usage); the encoder accumulates those into the
    // response.completed frame. Assert literal values so this can't pass on
    // `"input_tokens":0`.
    assert!(
        accumulated.contains("\"input_tokens\":5"),
        "expected input_tokens=5 in body, got: {accumulated}"
    );
    assert!(
        accumulated.contains("\"output_tokens\":2"),
        "expected output_tokens=2 in body, got: {accumulated}"
    );

    // ── mockito confirms exactly one /v1/messages call: the route worked ──
    mock.assert_async().await;
    let _ = tx.send(());
}
