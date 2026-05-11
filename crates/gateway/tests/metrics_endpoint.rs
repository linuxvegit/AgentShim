//! Plan 02 P02 T3: /metrics endpoint integration test.

use std::net::SocketAddr;

async fn pick_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn spawn_with_admin() -> (SocketAddr, SocketAddr) {
    let public_port = pick_port().await;
    let admin_port = pick_port().await;
    let yaml = format!(
        r#"
server: {{bind: 127.0.0.1, port: {public_port}}}
admin: {{bind: 127.0.0.1, port: {admin_port}}}
upstreams:
  noop:
    type: open_ai_compatible
    base_url: http://localhost:9999/v1
    api_key: dummy
routes:
  - frontend: openai_chat
    model: x
    upstream: noop
    upstream_model: x
"#
    );
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(&yaml).unwrap();
    let public_addr: SocketAddr = format!("127.0.0.1:{public_port}").parse().unwrap();
    let admin_addr: SocketAddr = format!("127.0.0.1:{admin_port}").parse().unwrap();
    let state = agent_shim_gateway::state::AppState::new(cfg).await;
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
    (public_addr, admin_addr)
}

#[tokio::test]
async fn metrics_endpoint_serves_prometheus_text() {
    let (_p, admin) = spawn_with_admin().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let resp = reqwest::get(format!("http://{}/metrics", admin))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.starts_with("text/plain"), "unexpected content-type: {ct}");
    // Body may legitimately be empty before any observation lands; the
    // Prometheus exporter omits metric lines until the first record_*
    // call. The companion test (metrics_text_parses_as_prometheus)
    // exercises the wire-format invariant. Here we just need to confirm
    // the handler is wired and returns the right Content-Type.
    let _body = resp.text().await.unwrap();
}

#[tokio::test]
async fn metrics_text_parses_as_prometheus() {
    let (_p, admin) = spawn_with_admin().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let body = reqwest::get(format!("http://{}/metrics", admin))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // Use prometheus_parse to ensure the wire format is valid even if empty.
    let lines: Vec<_> = body
        .lines()
        .map(|s| Ok::<String, std::io::Error>(s.to_string()))
        .collect();
    let _parsed = prometheus_parse::Scrape::parse(lines.into_iter()).expect("parses");
}
