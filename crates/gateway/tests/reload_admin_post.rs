//! Plan 04 P04 T5: POST /admin/reload integration tests.

use std::net::SocketAddr;
use std::sync::Arc;

mod common {
    pub async fn pick_port() -> u16 {
        tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }
}

async fn spawn(
    yaml: &str,
) -> (
    SocketAddr,
    SocketAddr,
    Arc<agent_shim_gateway::state::AppCore>,
) {
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let public_addr: SocketAddr = format!("{}:{}", cfg.server.bind, cfg.server.port)
        .parse()
        .unwrap();
    let admin_cfg = cfg.admin.clone().expect("admin block required");
    let admin_addr: SocketAddr = format!("{}:{}", admin_cfg.bind, admin_cfg.port)
        .parse()
        .unwrap();
    let (state, mut reload_rx) = agent_shim_gateway::state::AppState::new(cfg).await;

    // Run the reload-applying task locally inside the test. This mirrors
    // `commands::serve::run`'s task body so the handler's mpsc send →
    // oneshot reply round-trip actually completes. Crucially the harness
    // does NOT set `config_path` on `AppCore`, so a body-less POST hits
    // the handler's pre-channel "no config path" branch — exercised by
    // `reload_without_body_without_config_path_returns_500` below.
    let state_for_task = state.clone();
    tokio::spawn(async move {
        while let Some(req) = reload_rx.recv().await {
            let outcome = agent_shim_gateway::commands::serve::handle_reload_for_test(
                &state_for_task,
                req.source,
            )
            .await;
            let _ = req.respond_to.send(outcome);
        }
    });

    let pl = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let al = tokio::net::TcpListener::bind(admin_addr).await.unwrap();
    let pa = agent_shim_gateway::server::build_router(state.clone());
    let aa = agent_shim_gateway::admin::build_router(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(pl, pa).await;
    });
    tokio::spawn(async move {
        let _ = axum::serve(al, aa).await;
    });

    let core = state.core.clone();
    (public_addr, admin_addr, core)
}

const BASE_YAML: &str = r#"
server: {bind: 127.0.0.1, port: __PUBLIC__}
admin: {bind: 127.0.0.1, port: __ADMIN__}
upstreams:
  m: {type: open_ai_compatible, base_url: http://x/v1, api_key: a}
routes:
  - {frontend: openai_chat, model: x, upstream: m, upstream_model: x}
"#;

fn build_yaml(public: u16, admin: u16) -> String {
    BASE_YAML
        .replace("__PUBLIC__", &public.to_string())
        .replace("__ADMIN__", &admin.to_string())
}

#[tokio::test]
async fn reload_with_yaml_body_succeeds() {
    let public = common::pick_port().await;
    let admin = common::pick_port().await;
    let yaml = build_yaml(public, admin);
    let (_p, admin_addr, _core) = spawn(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Reload with same YAML — must succeed.
    let resp = reqwest::Client::new()
        .post(format!("http://{}/admin/reload", admin_addr))
        .header("content-type", "application/yaml")
        .body(yaml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], serde_json::Value::Bool(true));
}

#[tokio::test]
async fn reload_with_invalid_yaml_returns_400() {
    let public = common::pick_port().await;
    let admin = common::pick_port().await;
    let yaml = build_yaml(public, admin);
    let (_p, admin_addr, _core) = spawn(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{}/admin/reload", admin_addr))
        .header("content-type", "application/yaml")
        .body("[ : not valid yaml")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn reload_with_changed_server_port_returns_403() {
    let public = common::pick_port().await;
    let admin = common::pick_port().await;
    let yaml = build_yaml(public, admin);
    let (_p, admin_addr, _core) = spawn(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let mut bad: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
    bad["server"]["port"] = serde_yaml::Value::from(public + 1);
    let bad_yaml = serde_yaml::to_string(&bad).unwrap();

    let resp = reqwest::Client::new()
        .post(format!("http://{}/admin/reload", admin_addr))
        .header("content-type", "application/yaml")
        .body(bad_yaml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], serde_json::Value::Bool(false));
}

#[tokio::test]
async fn reload_without_body_without_config_path_returns_500() {
    let public = common::pick_port().await;
    let admin = common::pick_port().await;
    let yaml = build_yaml(public, admin);
    let (_p, admin_addr, _core) = spawn(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The test harness in `spawn()` does not set config_path, so a
    // body-less reload should fail with a clear message.
    let resp = reqwest::Client::new()
        .post(format!("http://{}/admin/reload", admin_addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 500);
}
