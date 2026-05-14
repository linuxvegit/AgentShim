//! Plan 06 P04 T6 — cost-filter metric counter integration test.
//!
//! Same chain shape as `cost_filter_full_skip.rs` — two OpenAI-compat
//! upstreams `a` and `b` with absurdly high per-token cost and a
//! route with `max_cost_usd: 0.0001`. Every candidate is rejected on
//! the `cap` axis. The resilience layer emits one
//! `agent_shim_cost_filtered_total{reason="cap", upstream=..., route=...}`
//! counter increment per rejected upstream.
//!
//! Load-bearing assertion: scrape `/metrics` on the admin port after
//! the 503-producing request, parse with `prometheus_parse`, and
//! confirm the sum of `agent_shim_cost_filtered_total` samples with
//! `reason="cap"` is at least 2 (one per upstream). The exact label
//! values for `upstream` and `route` are not strictly asserted —
//! the metric NAME + reason label is what dashboards key off, and
//! the per-upstream count is the operator-load signal.
//!
//! Why this test exists: P04 T4 wired the per-axis counter emission
//! inside `complete_inner_with_optional_filter`. The unit tests in
//! `crates/router/src/cost_filter.rs` cover the chain math; this
//! walks the full stack (axum → frontend decode → resilience layer
//! → metric emission → prometheus exporter → admin scrape) so a
//! regression in any link (registry plumbing, metric label naming,
//! exporter wiring) surfaces here as a missing/zero counter.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use agent_shim_config::{
    schema::{
        AdminConfig, BreakerConfig, LoggingConfig, OpenAiCompatibleUpstream, RetryConfig,
        RouteEntry, ServerConfig, Tier, UpstreamConfig, UpstreamCost, UpstreamRef,
    },
    GatewayConfig, Secret,
};
use agent_shim_gateway::{admin, server, state::AppState};
use tokio::net::TcpListener;

async fn pick_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn upstream(base_url: &str) -> UpstreamConfig {
    UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
        base_url: base_url.to_string(),
        api_key: Secret::new("test-key"),
        default_headers: BTreeMap::new(),
        request_timeout_secs: 30,
        tier: Tier::Standard,
        cost: Some(UpstreamCost {
            input_per_million_usd: 1_000_000.0,
            output_per_million_usd: 1_000_000.0,
        }),
        p95_latency_budget_ms: None,
    })
}

fn make_config(a_url: &str, b_url: &str, public_port: u16, admin_port: u16) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert("a".to_string(), upstream(a_url));
    upstreams.insert("b".to_string(), upstream(b_url));

    GatewayConfig {
        server: ServerConfig {
            bind: "127.0.0.1".to_string(),
            port: public_port,
            ..ServerConfig::default()
        },
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
            max_cost_usd: Some(0.0001),
        }],
        plugins: ::std::collections::BTreeMap::new(),
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: Some(AdminConfig {
            bind: "127.0.0.1".to_string(),
            port: admin_port,
        }),
        metrics: Default::default(),
        otel: None,
    }
}

#[tokio::test]
async fn cost_filter_emits_per_axis_metric_counter() {
    // Two mockito servers; neither should be hit. They exist to give
    // the gateway live HTTP endpoints in case the filter were broken.
    let mut server_a = mockito::Server::new_async().await;
    let _unreachable_a = server_a
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body("should never be called")
        .expect(0)
        .create_async()
        .await;

    let mut server_b = mockito::Server::new_async().await;
    let _unreachable_b = server_b
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body("should never be called")
        .expect(0)
        .create_async()
        .await;

    let public_port = pick_port().await;
    let admin_port = pick_port().await;
    let cfg = make_config(&server_a.url(), &server_b.url(), public_port, admin_port);

    let public_addr: SocketAddr = format!("127.0.0.1:{public_port}").parse().unwrap();
    let admin_addr: SocketAddr = format!("127.0.0.1:{admin_port}").parse().unwrap();
    let (state, _reload_rx) = AppState::new(cfg).await;
    let pl = TcpListener::bind(public_addr).await.unwrap();
    let al = TcpListener::bind(admin_addr).await.unwrap();
    let pa = server::build_router(state.clone());
    let aa = admin::build_router(state);
    tokio::spawn(async move {
        let _ = axum::serve(pl, pa).await;
    });
    tokio::spawn(async move {
        let _ = axum::serve(al, aa).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Fire the request that the filter rejects.
    let request_body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("request send");
    assert_eq!(resp.status(), 503, "expected 503 NoEligibleUpstream");

    // Scrape /metrics and parse Prometheus text.
    let metrics_body = reqwest::get(format!("http://{}/metrics", admin_addr))
        .await
        .expect("metrics scrape")
        .text()
        .await
        .expect("metrics body");
    let lines: Vec<_> = metrics_body
        .lines()
        .map(|s| Ok::<String, std::io::Error>(s.to_string()))
        .collect();
    let scrape = prometheus_parse::Scrape::parse(lines.into_iter()).expect("parses");

    // Sum all `agent_shim_cost_filtered_total` samples whose `reason`
    // label is `"cap"`. Two upstreams skipped on the cap axis → the
    // sum must be at least 2. We don't pin to exactly 2 because the
    // observed metric (and its `route` label) is robust to internal
    // route_label tweaks; what matters is the per-axis count.
    let cap_total: f64 = scrape
        .samples
        .iter()
        .filter(|s| s.metric == "agent_shim_cost_filtered_total")
        .filter(|s| s.labels.get("reason") == Some("cap"))
        .filter_map(|s| match s.value {
            prometheus_parse::Value::Counter(v) | prometheus_parse::Value::Untyped(v) => Some(v),
            _ => None,
        })
        .sum();
    assert!(
        cap_total >= 2.0,
        "expected agent_shim_cost_filtered_total{{reason=\"cap\"}} >= 2, got {cap_total}\n\
         metrics body:\n{metrics_body}"
    );
}
