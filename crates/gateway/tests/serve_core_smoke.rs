// crates/gateway/tests/serve_core_smoke.rs
//
// New integration test for the run_core refactor. Drives run_core directly
// with a custom shutdown future and on_listening callback, verifies both
// fire correctly.

use agent_shim_config::schema::{LoggingConfig, ServerConfig};
use agent_shim_config::GatewayConfig;
use agent_shim_gateway::commands::serve::run_core;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

const LISTEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn ephemeral_config() -> GatewayConfig {
    GatewayConfig {
        // port: 0 → OS picks a free port at bind time
        server: ServerConfig {
            bind: "127.0.0.1".into(),
            port: 0,
            keepalive_secs: 60,
        },
        logging: LoggingConfig::default(),
        upstreams: BTreeMap::new(),
        routes: vec![],
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
    }
}

#[tokio::test]
async fn run_core_invokes_on_listening_and_respects_custom_shutdown() {
    let cfg = ephemeral_config();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let bound_addr: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));

    let bound_addr_clone = bound_addr.clone();
    let shutdown_fut = async move {
        let _ = shutdown_rx.await;
    };
    let on_listening = move |addr: SocketAddr| {
        *bound_addr_clone.lock().unwrap() = Some(addr);
    };

    let server_task =
        tokio::spawn(async move { run_core(cfg, None, shutdown_fut, on_listening).await });

    // Poll for the listening address (on_listening should fire within a few
    // milliseconds of binding).
    let addr = tokio::time::timeout(LISTEN_TIMEOUT, async {
        loop {
            if let Some(addr) = *bound_addr.lock().unwrap() {
                return addr;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("on_listening never fired");

    // Hit the root endpoint to confirm the server is live.
    let resp = reqwest::get(format!("http://{addr}/")).await.unwrap();
    assert_eq!(resp.status(), 200);

    // Trigger graceful shutdown via our custom signal.
    let _ = shutdown_tx.send(());

    // Server should return Ok within shutdown grace window.
    let result = tokio::time::timeout(SHUTDOWN_TIMEOUT, server_task)
        .await
        .expect("server did not shut down within 5s")
        .expect("server task panicked");

    result.expect("run_core returned an error");
}
