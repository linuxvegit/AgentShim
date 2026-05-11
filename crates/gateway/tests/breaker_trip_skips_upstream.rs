//! Plan 04 P03 T5 — breaker-trip end-to-end smoke.
//!
//! Two OpenAI-compat upstreams behind a single OpenAI Chat route:
//! - A returns 502 to every `/v1/chat/completions` POST.
//! - B returns a valid 200 chat-completion JSON.
//!
//! 30 sequential client requests fire at the gateway. Per-upstream
//! retries are disabled (`max_attempts: 1`), so each inbound request
//! produces exactly one upstream call to A. The breaker config is
//! `failure_threshold_pct: 50, min_requests: 5, window_secs: 60` —
//! once 5 failures have accumulated for the `(oai_a, gpt-4o-up)`
//! breaker key, A's failure rate is 100% and the breaker trips Open.
//! Subsequent requests must skip A entirely and go straight to B.
//!
//! Why this test exists: P03 T1–T4 wired the breaker into `AppState`
//! and into the chain walk inside `ResilientCaller`. Unit tests at
//! both layers cover the math and the gate decision in isolation.
//! This walks the whole stack — axum router → frontend decode →
//! resilience layer → real OpenAI-compat HTTP client → mockito —
//! so a regression in any link (registry plumbing, policy threading,
//! decision/record ordering, fallback eligibility) surfaces here as
//! a hit-count violation rather than as a mystery in production.
//!
//! The load-bearing assertion is `bad_a.expect_at_most(10)`. With
//! `min_requests: 5` and 30 inbound requests, a broken breaker would
//! let all 30 hit A; mockito enforces the bound on `assert_async`.

use std::collections::BTreeMap;

use agent_shim_config::{
    schema::{
        BreakerConfig, LoggingConfig, OpenAiCompatibleUpstream, RetryConfig, RouteEntry,
        ServerConfig, UpstreamConfig, UpstreamRef,
    },
    GatewayConfig, Secret,
};
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use tokio::net::TcpListener;

/// Minimal OpenAI Chat Completions unary success body. Shape matches
/// what `OpenAiCompatibleProvider`'s unary parser expects so the
/// canonical decode produces a well-formed `CanonicalStream` the
/// OpenAI Chat encoder can serialize back to the client.
const OAI_CHAT_SUCCESS_BODY: &str = r#"{
    "id":"x",
    "object":"chat.completion",
    "created":1700000000,
    "model":"gpt-4o",
    "choices":[{"index":0,"message":{"role":"assistant","content":"from B"},"finish_reason":"stop"}],
    "usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}
}"#;

/// Build a gateway config with two OAI-compat upstreams `oai_a` and
/// `oai_b` and a single OpenAI Chat route fronting both as a fallback
/// chain. Retries within an upstream are disabled (`max_attempts: 1`)
/// so each inbound request produces exactly one upstream call per
/// chain element — that makes the breaker's sample count line up with
/// the inbound request count, which is what the trip math needs.
fn make_config(oai_a_url: &str, oai_b_url: &str) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert(
        "oai_a".to_string(),
        UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: oai_a_url.to_string(),
            api_key: Secret::new("test-key"),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
        }),
    );
    upstreams.insert(
        "oai_b".to_string(),
        UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: oai_b_url.to_string(),
            api_key: Secret::new("test-key"),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
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
                    name: "oai_a".to_string(),
                    model: "gpt-4o-up".to_string(),
                },
                UpstreamRef {
                    name: "oai_b".to_string(),
                    model: "gpt-4o-up".to_string(),
                },
            ],
            reasoning_effort: None,
            anthropic_beta: None,
            // max_attempts: 1 → no retries within an upstream. Each
            // inbound request consumes exactly one breaker sample on
            // whichever element actually got hit. Without this,
            // max_attempts: 2 would consume two samples per request
            // and the trip moment would still happen but the math
            // would be muddied.
            //
            // total_budget_ms: 100 keeps the test fast even if the
            // backoff path were ever exercised; with max_attempts: 1
            // there's no backoff at all.
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
            breaker: BreakerConfig {
                enabled: true,
                failure_threshold_pct: 50,
                min_requests: 5,
                window_secs: 60,
                open_cooldown_secs: 30,
            },
        }],
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: None,
        metrics: Default::default(),
    }
}

async fn spawn_gateway(
    oai_a_url: &str,
    oai_b_url: &str,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let cfg = make_config(oai_a_url, oai_b_url);
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
async fn breaker_trip_short_circuits_first_chain_element() {
    // Server A: 502 on every chat-completions POST. `expect_at_most(10)`
    // is the load-bearing assertion: the breaker config trips after 5
    // failure samples; 30 inbound requests must NOT all hit A. We
    // allow a small cushion (10) over the strict trip threshold (5)
    // because exact bounding is fragile — the registry's sliding
    // window math, real elapsed time, and any race between
    // `decision()` and `record()` could legitimately let 1–2 extra
    // requests through before the next `decision()` returns Skip.
    let mut server_a = mockito::Server::new_async().await;
    let bad_a = server_a
        .mock("POST", "/v1/chat/completions")
        .with_status(502)
        .with_body("Bad Gateway")
        .expect_at_most(10)
        .create_async()
        .await;

    // Server B: 200 + valid chat-completion JSON. `expect_at_least(1)`
    // confirms at least one request actually reached B — covers both
    // "early request fell through after A failed" and "later request
    // skipped A entirely after the breaker tripped".
    let mut server_b = mockito::Server::new_async().await;
    let good_b = server_b
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(OAI_CHAT_SUCCESS_BODY)
        .expect_at_least(1)
        .create_async()
        .await;

    let (addr, tx) = spawn_gateway(&server_a.url(), &server_b.url()).await;

    let request_body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let url = format!("http://{}/v1/chat/completions", addr);

    // 30 sequential POSTs. Sequential (not concurrent) by design —
    // P03 T2's stress test already covers concurrent decision/record
    // ordering; here we want serialized samples so the trip moment
    // is deterministic from the breaker's perspective.
    let client = reqwest::Client::new();
    for i in 0..30 {
        let resp = client
            .post(&url)
            .header("content-type", "application/json")
            .json(&request_body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("request {i} failed: {e}"));
        let status = resp.status();
        let body = resp.text().await.expect("body");
        assert_eq!(
            status, 200,
            "request {i}: expected 200 (B is healthy and either chain[0]→chain[1] \
             fallback succeeds or breaker trips and we go straight to B), got {status}\n\
             body: {body}"
        );
        // Status 200 alone doesn't prove B's payload reached the client —
        // a regression in the canonical decode → OpenAI Chat encode path
        // could yield an empty 200. `from B` is server B's `content`
        // field, so its presence end-to-end is the proof that fallback
        // (or breaker-skip) actually delivered B's bytes.
        assert!(
            body.contains("from B"),
            "request {i}: expected upstream B's content in response body; got: {body}"
        );
    }

    // mockito enforces `expect_at_most(10)` on assert_async. If the
    // breaker never tripped, A would have been hit 30 times and this
    // assertion fails with a hit-count mismatch.
    bad_a.assert_async().await;
    good_b.assert_async().await;

    let _ = tx.send(());
}
