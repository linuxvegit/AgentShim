//! Plan 05 P05 T7: end-to-end Phase 6 smoke test.
//!
//! Exercises two of the three v0.6 pillars in one flow plus the
//! associated `cost_filtered_total` metric:
//!
//!   1. **Outbound traceparent injection** (Plan 01 / P01) — a request
//!      to a route whose upstream is healthy reaches mockito carrying a
//!      well-formed W3C `traceparent` header. Verified with a mockito
//!      header matcher.
//!   3. **Cost filter** (Plan 04 / P04) — a request to a route whose
//!      upstream has an absurd per-token cost and a tight
//!      `max_cost_usd` cap is rejected by the cost filter before any
//!      HTTP call is made. Verified by HTTP 503 + the additive
//!      `filtered: [...]` top-level array on the OpenAI envelope
//!      (per P04 T6 commits 70a31ff + 12af3ac).
//!
//! After the 503 the test scrapes `/metrics` on the admin port and
//! asserts `agent_shim_cost_filtered_total{reason="cap"}` is at least
//! 1, confirming the metric pipe is wired end-to-end.
//!
//! **Why pillar 2 (rate-limit reload) is intentionally excluded**:
//! Plan 05 T7 §2 calls out that combining all three pillars in one
//! smoke test introduces timing dependencies between the rate-limit
//! bucket reset and the cost-filter reload step that make the test
//! flaky. Rate-limit reload behaviour is covered by the dedicated
//! integration test `reload_rate_limit_buckets_reset.rs` (P02 T5).
//!
//! Design choice: **one config, two routes, no reload**. Both routes
//! live in the initial YAML — pillar 1 hits route `cheap-x`, pillar 3
//! hits route `expensive-x`. This avoids any `/admin/reload` round
//! trip and keeps the test deterministic. The v0.5 sibling
//! `phase5_smoke.rs` did need a reload because its three pillars
//! (metrics, OTel-fmt, hot-reload) included reload itself; v0.6's
//! pillars don't, so a single config suffices.
//!
//! Pattern crib:
//!   - `crates/gateway/tests/outbound_traceparent.rs` — OTel subscriber
//!     install + mockito traceparent matcher.
//!   - `crates/gateway/tests/cost_filter_full_skip.rs` — 503 envelope
//!     + `filtered: [...]` array assertion.
//!   - `crates/gateway/tests/cost_filter_metric_counter.rs` — admin
//!     port + `/metrics` scrape + prometheus_parse counter sum.

use std::net::SocketAddr;
use std::sync::Once;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::TracerProvider;
use tracing_subscriber::layer::SubscriberExt;

/// Install a process-wide tracing subscriber that contains the
/// `tracing-opentelemetry` layer. Without it the gateway's root span
/// has no OTel `SpanContext` and `inject_context_into_headers` writes
/// nothing — pillar 1's matcher would never fire. Gated behind `Once`
/// because `set_global_default` panics on second call and multiple
/// integration tests in the same binary share the subscriber.
fn install_otel_subscriber() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_simple_exporter(exporter)
            .build();
        let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("test"));
        let subscriber = tracing_subscriber::registry().with(layer);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

async fn pick_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn phase6_pillars_compose() {
    install_otel_subscriber();

    // ── Pillar 1 mockito: must receive exactly one request carrying a
    // well-formed W3C traceparent header (regex matches the trace
    // version + 32-hex trace-id + 16-hex span-id + 2-hex flags shape).
    let mut cheap_server = mockito::Server::new_async().await;
    let cheap_mock = cheap_server
        .mock("POST", "/v1/chat/completions")
        .match_header(
            "traceparent",
            mockito::Matcher::Regex(r"^00-[0-9a-f]{32}-[0-9a-f]{16}-(00|01)$".to_string()),
        )
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("data: [DONE]\n\n")
        .expect(1)
        .create_async()
        .await;

    // ── Pillar 3 mockito: must NOT be called — the cost filter rejects
    // the expensive route before any provider HTTP call is made.
    // `expect(0)` makes the assertion fail loudly if the filter leaks.
    let mut expensive_server = mockito::Server::new_async().await;
    let expensive_mock = expensive_server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body("should never be called")
        .expect(0)
        .create_async()
        .await;

    let public_port = pick_port().await;
    let admin_port = pick_port().await;
    let yaml = format!(
        r#"
server: {{bind: 127.0.0.1, port: {public_port}}}
admin: {{bind: 127.0.0.1, port: {admin_port}}}
upstreams:
  cheap:
    type: open_ai_compatible
    base_url: {cheap_url}
    api_key: dummy
    tier: standard
  expensive:
    type: open_ai_compatible
    base_url: {expensive_url}
    api_key: dummy
    tier: standard
    cost:
      input_per_million_usd: 1000000.0
      output_per_million_usd: 1000000.0
routes:
  - frontend: openai_chat
    model: cheap-x
    upstream: cheap
    upstream_model: x
  - frontend: openai_chat
    model: expensive-x
    upstream: expensive
    upstream_model: x
    max_cost_usd: 0.0001
"#,
        cheap_url = cheap_server.url(),
        expensive_url = expensive_server.url(),
    );

    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(&yaml).unwrap();
    let public_addr: SocketAddr = format!("127.0.0.1:{public_port}").parse().unwrap();
    let admin_addr: SocketAddr = format!("127.0.0.1:{admin_port}").parse().unwrap();
    let (state, _reload_rx) = agent_shim_gateway::state::AppState::new(cfg).await.unwrap();
    let pl = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let al = tokio::net::TcpListener::bind(admin_addr).await.unwrap();
    let pa = agent_shim_gateway::server::build_router(state.clone());
    let aa = agent_shim_gateway::admin::build_router(state);
    tokio::spawn(async move {
        let _ = axum::serve(pl, pa).await;
    });
    tokio::spawn(async move {
        let _ = axum::serve(al, aa).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // ── Pillar 1: hit the `cheap-x` route. Mockito's traceparent
    // matcher fires only if the outbound request carries a
    // well-formed header; `cheap_mock.assert_async()` below panics
    // otherwise. The streaming response is irrelevant — we only need
    // the request to land at mockito with the right header.
    let _ = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .body(r#"{"model":"cheap-x","messages":[{"role":"user","content":"hi"}],"stream":true}"#)
        .send()
        .await;

    cheap_mock.assert_async().await;

    // ── Pillar 3: hit the `expensive-x` route. The cost filter
    // estimates: even an empty prompt has a few input tokens × $1M/M,
    // far over the $0.0001 cap. The only upstream in the chain is
    // rejected on the `cap` axis and the resilience layer surfaces
    // `NoEligibleUpstream`, which the handler maps to 503 + the
    // OpenAI envelope with the additive top-level `filtered` array.
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .body(r#"{"model":"expensive-x","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .expect("request send");
    assert_eq!(
        resp.status(),
        503,
        "expected 503 NoEligibleUpstream from cost filter"
    );

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
    assert!(
        !filtered.is_empty(),
        "expected at least one filtered entry, got: {body}"
    );
    for entry in filtered {
        let reason = entry["reason"]
            .as_str()
            .expect("filtered entry must have string `reason`");
        let upstream = entry["upstream"]
            .as_str()
            .expect("filtered entry must have string `upstream`");
        assert_eq!(reason, "cap", "expected reason=cap, got: {entry}");
        assert_eq!(upstream, "expensive", "unexpected upstream: {entry}");
    }

    // Sanity: the expensive upstream's mockito was never hit.
    expensive_mock.assert_async().await;

    // ── Metrics: scrape /metrics on the admin port, parse with
    // prometheus_parse, and confirm `agent_shim_cost_filtered_total`
    // with `reason="cap"` has at least 1 increment from the rejected
    // request above. (The exact `route` label is internal to the
    // resilience layer and not asserted — what matters is the
    // per-axis count survives the wire-format round trip.)
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
        cap_total >= 1.0,
        "expected agent_shim_cost_filtered_total{{reason=\"cap\"}} >= 1, got {cap_total}\n\
         metrics body:\n{metrics_body}"
    );
}
