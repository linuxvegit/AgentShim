//! Plan 06 P04 T6 — cost filter tier-axis partial-skip end-to-end smoke.
//!
//! Two OpenAI-compat upstreams: `eco` (tier: economy) and `std`
//! (tier: standard), both pointing at distinct mockito servers. A
//! single OpenAI Chat route with `min_tier: standard` and a chain of
//! `[eco, std]`. The cost filter rejects `eco` on the tier axis,
//! survives `std`, and the resilience layer routes the actual HTTP
//! call to `std` only.
//!
//! Load-bearing assertions:
//!   - mockito `std_mock` sees exactly 1 hit (the inbound request
//!     reached the standard-tier upstream).
//!   - mockito `eco_mock` sees exactly 0 hits (`expect(0)` — the
//!     economy-tier upstream was skipped by the filter and not
//!     contacted at all). This is the proof that the filter, not
//!     just the chain walk's fallback machinery, did the rejection:
//!     a fallback-on-failure path would still hit eco once.
//!
//! Two separate mockito servers (each `Server::new_async`) give us
//! distinct base URLs so we can prove WHICH upstream was contacted
//! via mockito's per-server hit counter. Rule 17 (validation.rs)
//! permits this config because the chain has at least one upstream
//! (`std`) meeting the route's `min_tier`.

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

/// Minimal OpenAI Chat Completions unary success body — the
/// `OpenAiCompatibleProvider`'s unary parser will produce a
/// well-formed `CanonicalStream` that the OpenAI Chat encoder can
/// serialize back to the client.
const OAI_CHAT_SUCCESS_BODY: &str = r#"{
    "id":"x",
    "object":"chat.completion",
    "created":1700000000,
    "model":"gpt-4o",
    "choices":[{"index":0,"message":{"role":"assistant","content":"from std"},"finish_reason":"stop"}],
    "usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}
}"#;

fn upstream(base_url: &str, tier: Tier) -> UpstreamConfig {
    UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
        base_url: base_url.to_string(),
        api_key: Secret::new("test-key"),
        default_headers: BTreeMap::new(),
        request_timeout_secs: 30,
        tier,
        cost: None,
        p95_latency_budget_ms: None,
    })
}

fn make_config(eco_url: &str, std_url: &str) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert("eco".to_string(), upstream(eco_url, Tier::Economy));
    upstreams.insert("std".to_string(), upstream(std_url, Tier::Standard));

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
                    name: "eco".to_string(),
                    model: "gpt-4o-up".to_string(),
                },
                UpstreamRef {
                    name: "std".to_string(),
                    model: "gpt-4o-up".to_string(),
                },
            ],
            reasoning_effort: None,
            anthropic_beta: None,
            retry: RetryConfig {
                max_attempts: 1,
                initial_backoff_ms: 1,
                multiplier: 2.0,
                jitter_pct: 0.0,
                total_budget_ms: 100,
                retry_on: vec![
                    "network".into(),
                    "upstream_5xx".into(),
                    "upstream_429".into(),
                ],
            },
            breaker: BreakerConfig::default(),
            // The load-bearing knob: economy-tier `eco` is below
            // this threshold; the filter rejects it on the `tier`
            // axis before the resilience layer's chain walk runs.
            min_tier: Some(Tier::Standard),
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
    eco_url: &str,
    std_url: &str,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let cfg = make_config(eco_url, std_url);
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
async fn cost_filter_tier_partial_skip_routes_only_to_standard() {
    // eco_mock: configured but `expect(0)` — the filter must skip
    // this upstream on the tier axis, so the gateway never makes an
    // HTTP call to it. If the filter were broken (e.g. tier axis
    // disabled, or skip semantics flipped to "first-match-wins")
    // we'd see eco_mock get a hit and assert_async would fail.
    let mut server_eco = mockito::Server::new_async().await;
    let eco_mock = server_eco
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body("eco should never be called (below min_tier=standard)")
        .expect(0)
        .create_async()
        .await;

    // std_mock: the only legitimate destination — `expect(1)` proves
    // the chain walk actually happened end-to-end (and through the
    // expected survivor).
    let mut server_std = mockito::Server::new_async().await;
    let std_mock = server_std
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(OAI_CHAT_SUCCESS_BODY)
        .expect(1)
        .create_async()
        .await;

    let (addr, tx) = spawn_gateway(&server_eco.url(), &server_std.url()).await;

    let request_body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", addr))
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("request send");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "expected 200 from std upstream; body: {body}");
    // The std upstream's `content` is "from std" — its presence end-to-end
    // proves the canonical decode + OpenAI Chat encode delivered the
    // expected upstream's bytes (not eco's `should never be called`).
    assert!(
        body.contains("from std"),
        "expected std upstream content in response body; got: {body}"
    );

    // Per-server hit-count assertions: this is the test's main
    // structural claim. `expect(1)` on std + `expect(0)` on eco are
    // enforced by mockito at `assert_async` time.
    std_mock.assert_async().await;
    eco_mock.assert_async().await;

    let _ = tx.send(());
}
