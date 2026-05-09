//! Integration tests for POST /v1/messages/count_tokens.
//!
//! Validates the HTTP contract end-to-end: response shape, status codes,
//! and behavior on the captured Claude Desktop preflight body.

use agent_shim_config::GatewayConfig;
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use std::collections::BTreeMap;
use tokio::net::TcpListener;

fn minimal_config() -> GatewayConfig {
    use agent_shim_config::schema::{LoggingConfig, ServerConfig};
    GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams: BTreeMap::new(),
        routes: vec![],
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
    }
}

/// Spawn the server on an ephemeral port and return (base_url, shutdown_tx).
async fn spawn_server() -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::new(minimal_config()).await;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        run_on_listener(listener, state, async {
            let _ = rx.await;
        })
        .await
        .unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (format!("http://{}", addr), tx)
}

#[tokio::test]
async fn happy_path_returns_input_tokens() {
    let (base, shutdown) = spawn_server().await;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "claude-opus-4-7",
        "messages": [{"role":"user","content":"hello"}]
    });
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", base))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    let n = json.get("input_tokens").and_then(|v| v.as_u64()).unwrap();
    assert!(n > 0, "input_tokens should be positive, got {}", n);
    let _ = shutdown.send(());
}

#[tokio::test]
async fn claude_desktop_replay_returns_200() {
    // Exact body captured in claudeDesktop.pcapng — preflight from
    // agent-sdk/0.2.121. Before the fix this 404'd and Claude Desktop
    // never proceeded.
    let captured = r#"{"model":"claude-opus-4-7","messages":[{"role":"user","content":"a331d0994e97:build-perf Agent for diagnosing and optimizing MSBuild build performance. Runs multi-step analysis: generates binlogs, analyzes timeline and bottlenecks, identifies expensive targets/tasks/analyzers, and suggests concrete optimizations. Invoke when builds are slow or when asked to optimize build times."}],"tools":[]}"#;

    let (base, shutdown) = spawn_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages/count_tokens?beta=true", base))
        .header("content-type", "application/json")
        .header("x-api-key", "agent shim") // exact header Claude Desktop sent, with the space
        .header("anthropic-version", "2023-06-01")
        .header(
            "anthropic-beta",
            "claude-code-20250219,interleaved-thinking-2025-05-14,token-counting-2024-11-01",
        )
        .body(captured.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "Claude Desktop preflight must succeed");
    let json: serde_json::Value = resp.json().await.unwrap();
    let n = json.get("input_tokens").and_then(|v| v.as_u64()).unwrap();
    assert!(n > 0);
    let _ = shutdown.send(());
}

#[tokio::test]
async fn malformed_body_returns_400() {
    let (base, shutdown) = spawn_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", base))
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert!(json.get("error").is_some());
    let _ = shutdown.send(());
}

#[tokio::test]
async fn unknown_role_returns_400() {
    let (base, shutdown) = spawn_server().await;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "m",
        "messages": [{"role":"system","content":"hi"}]  // 'system' is not a valid Anthropic role
    });
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", base))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let _ = shutdown.send(());
}

#[tokio::test]
async fn no_auth_headers_still_returns_200() {
    // The endpoint must not require x-api-key or Authorization.
    let (base, shutdown) = spawn_server().await;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "m",
        "messages": [{"role":"user","content":"hi"}]
    });
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", base))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = shutdown.send(());
}

#[tokio::test]
async fn beta_query_string_is_ignored() {
    let (base, shutdown) = spawn_server().await;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "m",
        "messages": [{"role":"user","content":"hi"}]
    });
    let with_beta = client
        .post(format!("{}/v1/messages/count_tokens?beta=true", base))
        .json(&body)
        .send()
        .await
        .unwrap();
    let without_beta = client
        .post(format!("{}/v1/messages/count_tokens", base))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(with_beta.status(), 200);
    assert_eq!(without_beta.status(), 200);
    let a: serde_json::Value = with_beta.json().await.unwrap();
    let b: serde_json::Value = without_beta.json().await.unwrap();
    assert_eq!(a, b, "?beta=true must not change the response");
    let _ = shutdown.send(());
}
