//! Plan 04 P01 T5 — gateway retry smoke test.
//!
//! End-to-end proof that `retry_with_policy` is wired into the pipeline:
//! mockito returns 502 once then 200, the gateway retries the upstream
//! request, and the client sees the success body.
//!
//! This is the only test that exercises retry through the real axum router;
//! unit-level retry behaviour is covered by
//! `crates/router/src/retry.rs::tests`. The two layers together enforce the
//! contract: the helper handles the loop, the pipeline plugs the helper in.

use std::collections::BTreeMap;

use agent_shim_config::{
    schema::{
        BreakerConfig, LoggingConfig, OpenAiCompatibleUpstream, RetryConfig, RouteEntry,
        ServerConfig, Tier, UpstreamConfig,
    },
    GatewayConfig, Secret,
};
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use tokio::net::TcpListener;

/// Build a gateway config with a single OpenAI Chat route and an aggressive
/// retry policy (1ms backoff). We override `max_attempts` to 2 so the test
/// proves *one* retry happens — exactly the §4.5/D12 default — without
/// pretending the test is exercising the default path (it's not; the YAML
/// would have to be empty for that).
fn make_config(upstream_url: &str) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert(
        "test-openai".to_string(),
        UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
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
        routes: vec![RouteEntry {
            frontend: "openai_chat".to_string(),
            model: "gpt-4o".to_string(),
            upstream: Some("test-openai".to_string()),
            upstream_model: Some("gpt-4o".to_string()),
            upstreams: vec![],
            reasoning_effort: None,
            anthropic_beta: None,
            // 2 attempts (one retry), 1ms backoff so the test stays fast.
            // Keep total_budget_ms generous (1s) — we want to exercise the
            // retry path, not the budget cap.
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
        }],
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
    }
}

async fn spawn_gateway(
    upstream_url: &str,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let cfg = make_config(upstream_url);
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
async fn retries_once_on_5xx_then_succeeds() {
    let mut server = mockito::Server::new_async().await;

    // First mock: 502, expected exactly once. mockito matches mocks in
    // creation order on a per-endpoint basis — the second call to
    // /v1/chat/completions falls through to the second mock below.
    let bad = server
        .mock("POST", "/v1/chat/completions")
        .with_status(502)
        .with_body("Bad Gateway")
        .expect(1)
        .create_async()
        .await;

    // Second mock: 200 with a minimal valid OpenAI Chat unary response.
    // Same endpoint as the bad mock — mockito serves them in order.
    let good = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id": "chatcmpl-retry",
                "object": "chat.completion",
                "created": 1700000000,
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "hi"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            }"#,
        )
        .expect(1)
        .create_async()
        .await;

    let (addr, tx) = spawn_gateway(&server.url()).await;

    let request_body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": false
    });

    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", addr))
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("request failed");

    // Client sees the success body — proves retry kicked in transparently.
    assert_eq!(resp.status(), 200, "client must see retry success");
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("\"hi\""),
        "expected retried success body to contain content, got: {body}"
    );

    // Both mocks must see exactly one hit. If the gateway didn't retry, only
    // the bad mock would be hit and the good mock would fail its expectation;
    // if it retried more than once, the bad mock would be over-hit. This is
    // the load-bearing assertion for the wiring contract.
    bad.assert_async().await;
    good.assert_async().await;

    let _ = tx.send(());
}
