//! Plan 04 P02 T6 — D4 regression pin: pre-stream-only fallback.
//!
//! Locks down §4.2 / D4 of the Phase 4 design: once `provider.complete()`
//! returns Ok(stream) and bytes start flowing to the client, mid-stream
//! failures surface as stream errors. We do NOT switch to chain[i+1]
//! mid-stream — at that point the client has already started consuming
//! the response and a switch would either duplicate prefix tokens or
//! corrupt the SSE state machine.
//!
//! The contract is enforced by the structure of `ResilientCaller`: once
//! `complete()` returns Ok, the caller is done; the chain walk has
//! already terminated. There is no further code path that could decide
//! to fall back. This test pins that behaviour with a load-bearing
//! `expect(0)` on chain[1]'s mock — if a future refactor introduced a
//! mid-stream restart, chain[1] would be hit ≥1× and mockito's
//! verification would fail.
//!
//! Both upstreams are OpenAI-compatible to keep the wire format simple:
//! the test is about WHEN fallback fires, not how dialects translate.

use std::collections::BTreeMap;

use agent_shim_config::{
    schema::{
        BreakerConfig, LoggingConfig, OpenAiCompatibleUpstream, RetryConfig, RouteEntry,
        ServerConfig, Tier, UpstreamConfig, UpstreamRef,
    },
    GatewayConfig, Secret,
};
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use tokio::net::TcpListener;

/// Truncated OpenAI Chat SSE: a single delta with content followed by a
/// `finish_reason=stop` chunk, but NO `[DONE]` sentinel and no `usage`.
/// The HTTP body is well-formed (200 + `text/event-stream`), so
/// `OpenAiCompatibleProvider::complete()` returns `Ok(stream)` —
/// fallback is not eligible at that point. The `finish_reason=stop`
/// chunk lets the SSE parser emit `MessageStop`, which lets the OAI
/// Chat encoder emit `[DONE]` and terminate the response body cleanly
/// (via the `terminate_on_sentinel` scan in `encode_stream::encode`).
/// (The contract is that fallback doesn't fire mid-stream — the body
/// shape doesn't matter as long as the gateway returns 200 and bytes
/// flow.)
const TRUNCATED_OAI_SSE: &str = concat!(
    "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"par\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",",
    "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
);

fn make_config(a_url: &str, b_url: &str) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert(
        "a".to_string(),
        UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: a_url.to_string(),
            api_key: Secret::new("test-key"),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
            tier: Tier::Standard,
            cost: None,
            p95_latency_budget_ms: None,
        }),
    );
    upstreams.insert(
        "b".to_string(),
        UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: b_url.to_string(),
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
        routes: vec![RouteEntry {
            frontend: "openai_chat".to_string(),
            model: "gpt-4o".to_string(),
            upstream: None,
            upstream_model: None,
            upstreams: vec![
                UpstreamRef {
                    name: "a".to_string(),
                    model: "gpt-4o".to_string(),
                },
                UpstreamRef {
                    name: "b".to_string(),
                    model: "gpt-4o".to_string(),
                },
            ],
            reasoning_effort: None,
            anthropic_beta: None,
            // max_attempts=1 so chain[0] doesn't burn retries on a 200
            // (it wouldn't anyway — 200 is not retryable — but pinning
            // 1 makes the trace easier to read if this fails).
            retry: RetryConfig {
                max_attempts: 1,
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
        }],
        plugins: ::std::collections::BTreeMap::new(),
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
    }
}

async fn spawn_gateway(
    a_url: &str,
    b_url: &str,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let cfg = make_config(a_url, b_url);
    let (state, _reload_rx) = AppState::new(cfg).await;
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
async fn mid_stream_failure_does_not_trigger_fallback() {
    // Upstream A: 200 + truncated SSE (no `[DONE]`). The HTTP layer
    // sees success; the canonical stream surfaces incomplete content
    // but never errors at the provider boundary. This is exactly the
    // scenario D4 covers: a "bad" stream that LOOKS like success at
    // the provider level.
    let mut server_a = mockito::Server::new_async().await;
    let truncated = server_a
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(TRUNCATED_OAI_SSE)
        .expect(1)
        .create_async()
        .await;

    // Upstream B: configured but never expected to be hit. The load-
    // bearing assertion is `expect(0)`: mockito's `assert_async()`
    // verifies B saw zero requests, which is what enforces D4.
    let mut server_b = mockito::Server::new_async().await;
    let b_unused = server_b
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body("never-called")
        .expect(0)
        .create_async()
        .await;

    let (addr, tx) = spawn_gateway(&server_a.url(), &server_b.url()).await;

    let request_body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": true
    });

    // Send the request and wait for the headers to arrive. Reading just one
    // chunk is enough to prove bytes flowed; the gateway's stream now
    // terminates correctly after `data: [DONE]\n\n` (see the
    // `terminate_on_sentinel` block in
    // `crates/frontends/src/openai_chat/encode_stream.rs`), so draining
    // would also work, but isn't necessary for what we're asserting here.
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", addr))
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("request failed");

    // A returned 200 + an SSE body; the gateway forwards that as 200
    // even though the body is truncated. The truncation surfaces
    // (silently or as a stream error) on the consuming end, NOT as a
    // status flip to 503.
    assert_eq!(
        resp.status(),
        200,
        "chain[0] returned 200 — gateway must propagate, not switch to chain[1]"
    );

    // Read just one chunk to prove bytes flowed (the gateway crossed
    // the "stream is open" threshold). Draining is also safe now that
    // the OAI Chat encoder terminates on the [DONE] sentinel.
    let mut byte_stream = resp.bytes_stream();
    use futures::StreamExt;
    let first = tokio::time::timeout(std::time::Duration::from_secs(2), byte_stream.next())
        .await
        .expect("first chunk should arrive within 2s")
        .expect("stream should produce at least one chunk")
        .expect("first chunk must not be an error");
    assert!(
        !first.is_empty(),
        "expected non-empty first chunk from chain[0]"
    );
    drop(byte_stream); // close the connection so mockito's mock_a
                       // transaction completes cleanly.

    truncated.assert_async().await;
    // The load-bearing assertion: B was NEVER called. Without D4,
    // a future "graceful chain restart on mid-stream error" would
    // hit B and `expect(0)` would fail.
    b_unused.assert_async().await;

    let _ = tx.send(());
}
