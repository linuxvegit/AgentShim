//! Plan 01 P01 T5: end-to-end tests for the admin listener.

use std::net::SocketAddr;

async fn pick_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

fn config_yaml(public_port: u16, admin_port: Option<u16>) -> String {
    let admin_block = match admin_port {
        Some(p) => format!("\nadmin: {{bind: 127.0.0.1, port: {p}}}"),
        None => String::new(),
    };
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
{admin_block}
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

async fn spawn_gateway_with_admin(yaml: &str) -> (SocketAddr, SocketAddr) {
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let public_addr: SocketAddr = format!("{}:{}", cfg.server.bind, cfg.server.port)
        .parse()
        .unwrap();
    let admin_cfg = cfg
        .admin
        .clone()
        .expect("admin block required for this helper");
    let admin_addr: SocketAddr = format!("{}:{}", admin_cfg.bind, admin_cfg.port)
        .parse()
        .unwrap();
    let (state, _reload_rx) = agent_shim_gateway::state::AppState::new(cfg).await.unwrap();

    let public_listener = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let admin_listener = tokio::net::TcpListener::bind(admin_addr).await.unwrap();
    let public_app = agent_shim_gateway::server::build_router(state.clone());
    let admin_app = agent_shim_gateway::admin::build_router(state);

    tokio::spawn(async move {
        let _ = axum::serve(public_listener, public_app).await;
    });
    tokio::spawn(async move {
        let _ = axum::serve(admin_listener, admin_app).await;
    });

    (public_addr, admin_addr)
}

#[tokio::test]
async fn admin_disabled_when_block_absent() {
    let public_port = pick_port().await;
    let yaml = config_yaml(public_port, None);
    let public_addr = spawn_gateway(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Public listener responds at /
    let resp = reqwest::get(format!("http://{}/", public_addr))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // /healthz no longer on the public port (was moved to admin in P01 T3)
    let resp = reqwest::get(format!("http://{}/healthz", public_addr))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // /readyz also moved off the public port (was never there in P01,
    // belt-and-braces for future drift).
    let resp = reqwest::get(format!("http://{}/readyz", public_addr))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn admin_listener_serves_healthz_when_configured() {
    let public_port = pick_port().await;
    let admin_port = pick_port().await;
    let yaml = config_yaml(public_port, Some(admin_port));
    let (_public, admin_addr) = spawn_gateway_with_admin(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let resp = reqwest::get(format!("http://{}/healthz", admin_addr))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn admin_listener_serves_readyz_when_configured() {
    let public_port = pick_port().await;
    let admin_port = pick_port().await;
    let yaml = config_yaml(public_port, Some(admin_port));
    let (_public, admin_addr) = spawn_gateway_with_admin(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let resp = reqwest::get(format!("http://{}/readyz", admin_addr))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ready");
}

#[tokio::test]
async fn admin_listener_does_not_expose_v1_endpoints() {
    let public_port = pick_port().await;
    let admin_port = pick_port().await;
    let yaml = config_yaml(public_port, Some(admin_port));
    let (_public, admin_addr) = spawn_gateway_with_admin(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // /v1/* must not be reachable on admin port (security boundary).
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/messages", admin_addr))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
