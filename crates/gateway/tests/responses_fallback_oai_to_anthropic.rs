//! Plan 04 P02 T6 — cross-protocol fallback smoke.
//!
//! Mockito serves 502 from upstream A (OAI-compat), 200 + Anthropic SSE
//! from upstream B. The pipeline retries A within budget, runs out,
//! falls back to chain[1] (anthropic), and the client receives B's
//! response in the OpenAI Responses event shape.
//!
//! Why this test exists: P02 T6 wires `ResilientCaller` into the
//! pipeline. End-to-end fallback was previously only covered by the
//! caller's own unit tests against `MockProvider`. This walks the full
//! stack — axum router → real frontend decode → resilience layer →
//! real provider HTTP clients → real frontend encode — so a regression
//! anywhere along that chain (routing, lookup adapter, encode shape,
//! status mapping) surfaces here, not as a 5-stack-frame mystery in
//! production.
//!
//! The cross-dialect aspect is load-bearing: it proves the gateway's
//! frontend dialect (Responses) is preserved on the response even when
//! the chain's terminal element speaks a different upstream dialect
//! (Anthropic).

use std::collections::BTreeMap;

use agent_shim_config::{
    schema::{
        AnthropicUpstream, BreakerConfig, LoggingConfig, OpenAiCompatibleUpstream, RetryConfig,
        RouteEntry, ServerConfig, Tier, UpstreamConfig, UpstreamRef,
    },
    GatewayConfig, Secret,
};
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use tokio::net::TcpListener;

/// Minimal Anthropic Messages SSE response: `message_start`, one text
/// delta, `message_stop`. Matches what the AnthropicProvider expects to
/// see on the wire so the canonical decode produces a well-formed
/// CanonicalStream the OpenAI Responses encoder can consume.
const ANTHROPIC_TEXT_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_b\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-4-7\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":7,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi from B\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":3}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Build a gateway config with a two-element fallback chain
/// (`oai_compat` → `anthropic`) under the OpenAI Responses frontend.
/// Aggressive retry policy (1ms backoff, 1s budget) keeps the test fast
/// while still exercising the real retry+fallback path: A is hit 2× (max
/// attempts), then we fall through to B.
fn make_config(oai_url: &str, anthropic_url: &str) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert(
        "oai".to_string(),
        UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: oai_url.to_string(),
            api_key: Secret::new("test-key"),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
            tier: Tier::Standard,
            cost: None,
            p95_latency_budget_ms: None,
        }),
    );
    upstreams.insert(
        "anth".to_string(),
        UpstreamConfig::Anthropic(AnthropicUpstream {
            base_url: anthropic_url.to_string(),
            api_key: Secret::new("test-key"),
            anthropic_version: "2023-06-01".to_string(),
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
        routes: vec![RouteEntry {
            frontend: "openai_responses".to_string(),
            model: "gpt-4o".to_string(),
            upstream: None,
            upstream_model: None,
            upstreams: vec![
                UpstreamRef {
                    name: "oai".to_string(),
                    model: "gpt-4o-2024-11-20".to_string(),
                },
                UpstreamRef {
                    name: "anth".to_string(),
                    model: "claude-opus-4-7".to_string(),
                },
            ],
            reasoning_effort: None,
            anthropic_beta: None,
            retry: RetryConfig {
                max_attempts: 2,
                initial_backoff_ms: 1,
                multiplier: 2.0,
                jitter_pct: 0.0,
                total_budget_ms: 1000,
                retry_on: vec![
                    "network".into(),
                    "upstream_5xx".into(),
                    "upstream_429".into(),
                ],
            },
            breaker: BreakerConfig::default(),
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
    }
}

async fn spawn_gateway(
    oai_url: &str,
    anthropic_url: &str,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let cfg = make_config(oai_url, anthropic_url);
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
async fn falls_back_from_oai_compat_to_anthropic_when_oai_returns_502() {
    // Upstream A: 502 from BOTH passthrough (`/v1/responses`) and
    // canonical chat (`/v1/chat/completions`). The Responses frontend's
    // proxy_raw short-circuit posts to `/v1/responses` first; on
    // failure the pipeline falls through to the canonical decode path,
    // which posts to `/v1/chat/completions`. Mocking both with 502
    // exercises the same eligible-error class along both paths so the
    // chain walk eventually advances to chain[1].
    //
    // `expect_at_least(1)` on each: A may be hit up to `max_attempts`
    // times via either path before falling back, so we don't pin the
    // exact count — we just require it was tried at all.
    let mut server_a = mockito::Server::new_async().await;
    let bad_a_responses = server_a
        .mock("POST", "/v1/responses")
        .with_status(502)
        .with_body("Bad Gateway")
        .expect_at_least(1)
        .create_async()
        .await;
    let bad_a_chat = server_a
        .mock("POST", "/v1/chat/completions")
        .with_status(502)
        .with_body("Bad Gateway")
        .expect_at_least(0) // may or may not be hit, depending on retry budget
        .create_async()
        .await;

    // Upstream B: 200 + Anthropic SSE, expected exactly once. If the
    // pipeline failed to walk to chain[1], B would never be hit and
    // expect(1) fails the test.
    let mut server_b = mockito::Server::new_async().await;
    let good_b = server_b
        .mock("POST", "/v1/messages")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(ANTHROPIC_TEXT_SSE)
        .expect(1)
        .create_async()
        .await;

    let (addr, tx) = spawn_gateway(&server_a.url(), &server_b.url()).await;

    // Inbound request: OpenAI Responses dialect, streaming. The
    // dialect-preservation contract says the response stream MUST be
    // shaped per Responses regardless of which chain element succeeded.
    let request_body = serde_json::json!({
        "model": "gpt-4o",
        "input": "hi",
        "stream": true
    });

    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/responses", addr))
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("request failed");

    // 200 means fallback succeeded — we got a real upstream response.
    // (If both chain elements had failed, we'd get 503.)
    assert_eq!(resp.status(), 200, "fallback to chain[1] must succeed");
    let content_type = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default();
    assert!(
        content_type.starts_with("text/event-stream"),
        "Responses streaming response must be SSE; got content-type: {content_type}"
    );

    let body = resp.text().await.expect("body");
    // The Responses dialect uses `response.output_text.delta` for token
    // deltas; that's the load-bearing assertion that we encoded in
    // Responses shape, NOT Anthropic shape, even though the upstream
    // that produced the bytes spoke Anthropic Messages.
    assert!(
        body.contains("response.output_text.delta") || body.contains("output_text.delta"),
        "expected Responses dialect event names in body, got:\n{body}"
    );
    // The actual text from upstream B should appear somewhere in the
    // stream — confirms the canonical decoder routed B's content
    // through correctly.
    assert!(
        body.contains("hi from B"),
        "expected upstream B's text to appear in encoded stream, got:\n{body}"
    );

    bad_a_responses.assert_async().await;
    bad_a_chat.assert_async().await;
    good_b.assert_async().await;

    let _ = tx.send(());
}
