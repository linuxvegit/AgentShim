use agent_shim_gateway::{server::run_on_listener, state::AppState};
use agent_shim_config::GatewayConfig;
use std::collections::BTreeMap;
use tokio::net::TcpListener;

fn minimal_config() -> GatewayConfig {
    use agent_shim_config::schema::{ServerConfig, LoggingConfig};
    GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams: BTreeMap::new(),
        routes: vec![],
        copilot: None,
    }
}

#[tokio::test]
async fn healthz_returns_200_ok() {
    // Bind on port 0 so the OS picks a free port.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let state = AppState::new(minimal_config());

    // Spawn the server in the background; send shutdown immediately after test.
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        run_on_listener(listener, state, async {
            let _ = rx.await;
        })
        .await
        .unwrap();
    });

    // Give the server a moment to start.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url = format!("http://{}/healthz", addr);
    let resp = reqwest::Client::builder()
        .build()
        .unwrap()
        .get(&url)
        .send()
        .await
        .expect("request to /healthz failed");

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert_eq!(body, "ok");

    let _ = tx.send(());
}
