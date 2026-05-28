//! Plan 06 P04 T6 — cost filter full-skip end-to-end smoke.
//!
//! Two OpenAI-compat upstreams `a` and `b`, both with an absurdly high
//! per-token cost ($1M per million tokens for both input and output),
//! behind a single OpenAI Chat route with `max_cost_usd: 0.0001`. Every
//! candidate estimate exceeds the cap, so the cost filter rejects both
//! upstreams on the `cap` axis and the resilience layer returns
//! `NoEligibleUpstream` before any provider HTTP call is made.
//!
//! Load-bearing assertions:
//!   - HTTP 503 (per spec §6.3).
//!   - JSON body carries the existing OpenAI `error` envelope shape
//!     (`error.type = service_unavailable_error`, `error.code =
//!     no_eligible_upstream`).
//!   - JSON body carries the additive top-level `filtered` array (per
//!     P04 T6 envelope refactor; see `crates/gateway/src/handlers/mod.rs`
//!     `openai_envelope_body`). Each entry has `{upstream, reason}` with
//!     `reason = "cap"` and `upstream` one of `"a"` / `"b"`.
//!
//! We use the cap variant of full-skip (not tier) because rule 17
//! (`crates/config/src/validation.rs`) rejects at startup any route
//! whose `min_tier` cannot be satisfied by any upstream in its chain —
//! a tier-axis full-skip is unreachable through the configured
//! pipeline. The cap axis has no equivalent startup guard: it depends
//! on per-request token counts which validation cannot know.

use std::collections::BTreeMap;

use agent_shim_config::{
    schema::{
        BreakerConfig, LoggingConfig, OpenAiCompatibleUpstream, RetryConfig, RouteEntry,
        ServerConfig, Tier, UpstreamConfig, UpstreamCost, UpstreamRef,
    },
    GatewayConfig, Secret,
};
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use tokio::net::TcpListener;

fn upstream(base_url: &str) -> UpstreamConfig {
    UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
        base_url: base_url.to_string(),
        api_key: Secret::new("test-key"),
        default_headers: BTreeMap::new(),
        request_timeout_secs: 30,
        // Tier passes rule 17 (Standard matches the implicit absence of
        // min_tier); the cap axis is what trips.
        tier: Tier::Standard,
        cost: Some(UpstreamCost {
            input_per_million_usd: 1_000_000.0,
            output_per_million_usd: 1_000_000.0,
        }),
        p95_latency_budget_ms: None,
    })
}

fn make_config(a_url: &str, b_url: &str) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert("a".to_string(), upstream(a_url));
    upstreams.insert("b".to_string(), upstream(b_url));

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
                    model: "gpt-4o-up".to_string(),
                },
                UpstreamRef {
                    name: "b".to_string(),
                    model: "gpt-4o-up".to_string(),
                },
            ],
            reasoning_effort: None,
            anthropic_beta: None,
            // No retries within an upstream — the cost filter rejects
            // before the chain walk even begins, so retry settings
            // don't matter; we keep them minimal for hygiene.
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
            min_tier: None,
            // Cap any non-trivial request out: even an empty prompt
            // estimates a few input tokens × $1M/M, well over $0.0001.
            max_cost_usd: Some(0.0001),
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
    a_url: &str,
    b_url: &str,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let cfg = make_config(a_url, b_url);
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
async fn cost_filter_full_skip_returns_503_with_filtered_array() {
    // Two mockito servers — they exist so the gateway has live HTTP
    // endpoints in case the filter were to mistakenly let a request
    // through; `expect(0)` on both proves the cost filter shorted
    // the chain before any upstream was contacted.
    let mut server_a = mockito::Server::new_async().await;
    let unreachable_a = server_a
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body("should never be called")
        .expect(0)
        .create_async()
        .await;

    let mut server_b = mockito::Server::new_async().await;
    let unreachable_b = server_b
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body("should never be called")
        .expect(0)
        .create_async()
        .await;

    let (addr, tx) = spawn_gateway(&server_a.url(), &server_b.url()).await;

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
    assert_eq!(resp.status(), 503, "expected 503 NoEligibleUpstream");

    // Body shape: OpenAI envelope + the additive `filtered` array.
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(
        body["error"]["type"], "service_unavailable_error",
        "OpenAI envelope error.type mismatch: {body}"
    );
    assert_eq!(
        body["error"]["code"], "no_eligible_upstream",
        "OpenAI envelope error.code mismatch: {body}"
    );

    let filtered = body["filtered"]
        .as_array()
        .unwrap_or_else(|| panic!("expected `filtered` to be an array, got: {body}"));
    assert_eq!(
        filtered.len(),
        2,
        "expected 2 filtered entries (one per upstream); got: {body}"
    );
    for entry in filtered {
        let reason = entry["reason"]
            .as_str()
            .expect("filtered entry must have string `reason`");
        let upstream = entry["upstream"]
            .as_str()
            .expect("filtered entry must have string `upstream`");
        assert_eq!(reason, "cap", "expected reason=cap, got: {entry}");
        assert!(
            upstream == "a" || upstream == "b",
            "expected upstream=a or b, got: {entry}"
        );
    }

    // Sanity: neither mockito server was hit.
    unreachable_a.assert_async().await;
    unreachable_b.assert_async().await;

    let _ = tx.send(());
}
