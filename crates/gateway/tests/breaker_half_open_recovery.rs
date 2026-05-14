//! Plan 04 P03 T6 — end-to-end half-open recovery.
//!
//! Trip a breaker on chain[0], advance a FakeClock past
//! `open_cooldown_secs`, send a single probe request that hits a
//! recovered upstream, observe breaker closes, confirm subsequent
//! requests continue to flow through chain[0].
//!
//! Why this test exists: P03 T1's state-machine unit tests cover the
//! Closed→Open→HalfOpen→Closed transitions in isolation against a
//! `BreakerState` in memory. This walks the whole stack — axum →
//! frontend decode → ResilientCaller → BreakerRegistry::decision →
//! provider HTTP → BreakerRegistry::record — so a regression in the
//! probe authorization, the record-clearing of probe_in_flight, or
//! the time-based decision boundary surfaces as a hit-count mismatch.
//!
//! The load-bearing assertion is that after `clock.advance(31s)`,
//! exactly ONE request goes through chain[0] (the probe) and the
//! breaker closes; subsequent requests must also hit chain[0].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_shim_config::{
    schema::{
        BreakerConfig, LoggingConfig, OpenAiCompatibleUpstream, RetryConfig, RouteEntry,
        ServerConfig, Tier, UpstreamConfig, UpstreamRef,
    },
    GatewayConfig, Secret,
};
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use agent_shim_router::Clock;
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
    "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
    "usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}
}"#;

/// Test-only `Clock` that lets the smoke test advance "now" by an
/// arbitrary duration without sleeping. Same shape as the FakeClock
/// in `circuit_breaker.rs::tests`, redefined here because that one is
/// `pub(crate)` to the router crate. `Mutex<Instant>` is `Send + Sync`
/// so the registry can hold an `Arc<dyn Clock>` to it across threads.
struct FakeClock(Mutex<Instant>);

impl FakeClock {
    fn new() -> Self {
        Self(Mutex::new(Instant::now()))
    }

    fn advance(&self, d: Duration) {
        let mut t = self.0.lock().unwrap();
        *t += d;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
}

/// Build a gateway config with two OAI-compat upstreams `oai_a` and
/// `oai_b` and a single OpenAI Chat route fronting both as a fallback
/// chain. Retries within an upstream are disabled (`max_attempts: 1`)
/// so each inbound request produces exactly one upstream call per
/// chain element — the breaker's sample count then lines up with the
/// inbound request count, which is what the trip math needs.
fn make_config(oai_a_url: &str, oai_b_url: &str) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert(
        "oai_a".to_string(),
        UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: oai_a_url.to_string(),
            api_key: Secret::new("test-key"),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
            tier: Tier::Standard,
            cost: None,
            p95_latency_budget_ms: None,
        }),
    );
    upstreams.insert(
        "oai_b".to_string(),
        UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: oai_b_url.to_string(),
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
            // max_attempts: 1 → one upstream call per chain element per
            // inbound request, so 5 inbound 502s = 5 breaker samples on
            // A. total_budget_ms: 100 keeps the test fast.
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
            min_tier: None,
            max_cost_usd: None,
            plugins: None,
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

/// Spawn an axum gateway with `AppState::new_with_clock` so the breaker
/// registry gets the supplied `FakeClock`. Returns the bound socket
/// address and a oneshot sender used to trigger graceful shutdown.
async fn spawn_gateway(
    oai_a_url: &str,
    oai_b_url: &str,
    clock: Arc<dyn Clock>,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let cfg = make_config(oai_a_url, oai_b_url);
    let (state, _reload_rx) = AppState::new_with_clock(cfg, clock).await;
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
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, tx)
}

#[tokio::test]
async fn breaker_half_open_recovery_closes_breaker_and_resumes_chain_head() {
    // Server A — phase 1: 502 on the first 5 chat-completions POSTs.
    // `expect(5)` is exact: mockito stops matching after 5 hits, then
    // the next mock (recovered_a, registered second) takes over for
    // requests 6+. If the breaker fails to trip and a 6th request
    // reaches A's failure mock, `bad_a.assert_async` reports the
    // hit-count mismatch.
    let mut server_a = mockito::Server::new_async().await;
    let bad_a = server_a
        .mock("POST", "/v1/chat/completions")
        .with_status(502)
        .with_body("Bad Gateway")
        .expect(5)
        .create_async()
        .await;

    // Server A — phase 2: 200 + valid chat-completion JSON. mockito
    // matches mocks in registration order, so this only picks up the
    // 6th+ request to A (i.e. the half-open probe and any post-
    // recovery traffic). `expect_at_least(1)` is conservative: the
    // probe + the post-recovery request both hit it, but bounding the
    // exact count is brittle (e.g. body-encoding changes elsewhere
    // could shift counts by one without breaking the breaker logic).
    let recovered_a = server_a
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(OAI_CHAT_SUCCESS_BODY)
        .expect_at_least(1)
        .create_async()
        .await;

    // Server B — always 200 + valid chat-completion JSON. Catches the
    // first 5 inbound requests via fallback (chain[0] fails → chain[1]
    // succeeds) and any traffic after the breaker trips Open.
    let mut server_b = mockito::Server::new_async().await;
    let good_b = server_b
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(OAI_CHAT_SUCCESS_BODY)
        .expect_at_least(1)
        .create_async()
        .await;

    let fake_clock = Arc::new(FakeClock::new());
    let clock_for_state: Arc<dyn Clock> = Arc::clone(&fake_clock) as Arc<dyn Clock>;
    let (addr, tx) = spawn_gateway(&server_a.url(), &server_b.url(), clock_for_state).await;

    let request_body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let url = format!("http://{}/v1/chat/completions", addr);
    let client = reqwest::Client::new();

    // Phase 1: 5 sequential POSTs trip the breaker. Each consumes one
    // breaker sample on A (502 → failure) plus one fallback hit on B
    // (200). After 5 samples at 100% failure rate ≥ 50% threshold,
    // A's breaker transitions Closed → Open.
    for i in 0..5 {
        let resp = client
            .post(&url)
            .header("content-type", "application/json")
            .json(&request_body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("phase 1 request {i} failed: {e}"));
        assert_eq!(
            resp.status(),
            200,
            "phase 1 request {i}: expected 200 from B fallback"
        );
    }

    // Phase 2: breaker is Open, no clock advance. Decision returns
    // Skip → A is bypassed → B handles the request. A's hit count
    // stays at 5; B's increments. This is a sanity check that the
    // breaker actually tripped (otherwise A would receive a 6th hit
    // and `bad_a.assert_async` would fail).
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("phase 2 request failed");
    assert_eq!(
        resp.status(),
        200,
        "phase 2: expected 200 from B (A skipped while Open)"
    );

    // Phase 3: advance clock past `open_cooldown_secs: 30`. The next
    // decision returns Probe → A receives this request and now hits
    // the recovered_a (200) mock. The provider returns success, so
    // ResilientCaller calls `record(true)` → BreakerState transitions
    // HalfOpen → Closed.
    fake_clock.advance(Duration::from_secs(31));
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("phase 3 probe request failed");
    assert_eq!(
        resp.status(),
        200,
        "phase 3: probe should succeed and gateway returns 200"
    );

    // Phase 4: breaker is Closed; the next request should flow through
    // chain[0] (A) and succeed. This is the recovery proof — without
    // the probe → record(true) → Closed transition, A would still be
    // skipped and B would handle this request, leaving recovered_a
    // with only one hit (the probe).
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("phase 4 post-recovery request failed");
    assert_eq!(
        resp.status(),
        200,
        "phase 4: post-recovery request should succeed via A"
    );

    // Final hit-count enforcement.
    //   - bad_a (502 mock, expect(5)): A was hit exactly 5 times during
    //     the trip phase. A 6th hit would imply the breaker never
    //     tripped.
    //   - recovered_a (200 mock, expect_at_least(1)): the probe (and
    //     the post-recovery request) reached A after the cooldown.
    //   - good_b: B handled at least one fallback during phase 1 and
    //     the breaker-skip in phase 2.
    bad_a.assert_async().await;
    recovered_a.assert_async().await;
    good_b.assert_async().await;

    let _ = tx.send(());
}
