//! Plan 03 P03 T4 (+ T4 followup): inbound `traceparent` does not crash
//! the gateway. Inbound-only for v0.5 — outbound continuation is a v0.6
//! task pending a provider-side injection hook (spec §4.3 footnote).
//!
//! These tests are smoke tests against the public router: they assert
//! that a well-formed and a garbage `traceparent` header both produce a
//! 200 response and don't trigger any panics or 5xx codes.
//!
//! Positive evidence that the parsed context actually parents a
//! downstream span lives in the unit test
//! `agent_shim_observability::otel::extract::tests::extracted_context_parents_a_downstream_span`
//! and the span-tree shape test in `crates/router/tests/otel_spans.rs`.

use std::net::SocketAddr;

async fn pick_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn config_yaml(public_port: u16) -> String {
    format!(
        r#"
server: {{bind: 127.0.0.1, port: {public_port}}}
upstreams:
  noop:
    type: open_ai_compatible
    base_url: http://localhost:9999/v1
    api_key: dummy
    tier: standard
routes:
  - frontend: openai_chat
    model: x
    upstream: noop
    upstream_model: x
"#
    )
}

async fn spawn_gateway(yaml: &str) -> SocketAddr {
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let public_addr: SocketAddr = format!("{}:{}", cfg.server.bind, cfg.server.port)
        .parse()
        .unwrap();
    let (state, _reload_rx) = agent_shim_gateway::state::AppState::new(cfg).await.unwrap();
    let listener = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let actual = listener.local_addr().unwrap();
    let app = agent_shim_gateway::server::build_router(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    actual
}

#[tokio::test]
async fn inbound_traceparent_recognized() {
    // Build a minimal gateway. We don't actually export spans here;
    // we assert the layer accepts the header without 4xx/5xx-ing.
    let public_port = pick_port().await;
    let yaml = config_yaml(public_port);
    let public_addr = spawn_gateway(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // `/` is the public probe in P01+ (healthz moved to admin port).
    let resp = reqwest::Client::new()
        .get(format!("http://{}/", public_addr))
        .header(
            "traceparent",
            "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // The layer is non-rejecting; positive evidence that the trace
    // context was accepted is asserted via in-memory exporter in T5.
}

#[tokio::test]
async fn malformed_traceparent_does_not_500() {
    let public_port = pick_port().await;
    let yaml = config_yaml(public_port);
    let public_addr = spawn_gateway(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let resp = reqwest::Client::new()
        .get(format!("http://{}/", public_addr))
        .header("traceparent", "garbage")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "malformed header must not be fatal");
}
