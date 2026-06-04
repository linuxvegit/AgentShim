//! Live e2e: AgentShim gateway -> real Zhipu GLM API.
//!
//! Gated behind the `live` cargo feature AND the `GLM_API_KEY` env var.
//! Same posture as `deepseek_live.rs` / `anthropic_live.rs` — runs only
//! when both signals are present. When the feature is off, the entire
//! file is `cfg`-excluded and contributes nothing to compilation; when
//! the feature is on but the env var is absent, individual tests print
//! a skip notice and return Ok.
//!
//! Manual invocation:
//!   cargo nextest run -p agent-shim --features live glm_live
//!
//! Runs nightly from `.github/workflows/nightly-live.yaml` once the
//! `GLM_API_KEY` secret is wired up.

#![cfg(feature = "live")]

use std::collections::BTreeMap;

use agent_shim_config::{
    schema::{GlmUpstream, LoggingConfig, RouteEntry, ServerConfig, Tier, UpstreamConfig},
    GatewayConfig, Secret,
};
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use futures::StreamExt;
use tokio::net::TcpListener;

const UPSTREAM_NAME: &str = "glm-live";
const MODEL: &str = "glm-5.1";

fn make_config(api_key: String) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert(
        UPSTREAM_NAME.to_string(),
        UpstreamConfig::Glm(GlmUpstream {
            base_url: "https://open.bigmodel.cn/api/paas/v4".to_string(),
            api_key: Secret::new(api_key),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
            tier: Tier::Standard,
            cost: None,
            p95_latency_budget_ms: None,
        }),
    );

    GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams,
        routes: vec![RouteEntry::singular(
            "openai_chat",
            MODEL,
            UPSTREAM_NAME,
            MODEL,
        )],
        plugins: BTreeMap::new(),
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
    api_key: String,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let cfg = make_config(api_key);
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

fn api_key_or_skip(test_name: &str) -> Option<String> {
    match std::env::var("GLM_API_KEY") {
        Ok(k) if !k.is_empty() => Some(k),
        _ => {
            eprintln!("[live-skip] {test_name}: GLM_API_KEY not set");
            None
        }
    }
}

#[tokio::test]
async fn glm_live_streaming() {
    let Some(key) = api_key_or_skip("glm_live_streaming") else {
        return;
    };
    let (addr, tx) = spawn_gateway(key).await;

    let request_body = serde_json::json!({
        "model": MODEL,
        "max_tokens": 16,
        "stream": true,
        "messages": [
            {"role": "user", "content": "Say hi in one word."}
        ]
    });

    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", addr))
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("glm streaming request failed");

    assert_eq!(
        resp.status(),
        200,
        "glm streaming non-200: {:?}",
        resp.text().await
    );
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        content_type.contains("text/event-stream"),
        "expected SSE content-type, got: {content_type}"
    );

    let mut accumulated = String::new();
    let mut byte_stream = resp.bytes_stream();
    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk.expect("glm stream chunk error");
        accumulated.push_str(&String::from_utf8_lossy(&chunk));
        if accumulated.contains("data: [DONE]") {
            break;
        }
    }

    assert!(
        accumulated.contains("chat.completion.chunk"),
        "expected at least one chat.completion.chunk SSE event, got:\n{accumulated}"
    );
    assert!(
        accumulated.contains("[DONE]"),
        "expected [DONE] terminator, got:\n{accumulated}"
    );

    let _ = tx.send(());
}

#[tokio::test]
async fn glm_live_unary() {
    let Some(key) = api_key_or_skip("glm_live_unary") else {
        return;
    };
    let (addr, tx) = spawn_gateway(key).await;

    let request_body = serde_json::json!({
        "model": MODEL,
        "max_tokens": 16,
        "stream": false,
        "messages": [
            {"role": "user", "content": "Say hi in one word."}
        ]
    });

    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", addr))
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("glm unary request failed");

    assert_eq!(
        resp.status(),
        200,
        "glm unary non-200: {:?}",
        resp.text().await
    );
    let body: serde_json::Value = resp.json().await.expect("response not valid JSON");
    assert_eq!(
        body["object"], "chat.completion",
        "unexpected response shape: {body}"
    );
    let content = body["choices"][0]["message"]["content"]
        .as_str()
        .expect("choices[0].message.content missing or not a string");
    assert!(!content.is_empty(), "glm returned empty assistant content");

    let _ = tx.send(());
}
