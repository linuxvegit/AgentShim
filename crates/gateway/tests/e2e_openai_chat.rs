/// End-to-end test: mockito upstream → gateway → client (OpenAI chat completions)
use std::collections::BTreeMap;

use agent_shim_config::{
    schema::{LoggingConfig, OpenAiCompatibleUpstream, RouteEntry, ServerConfig, UpstreamConfig},
    GatewayConfig, Secret,
};
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use futures::StreamExt;
use tokio::net::TcpListener;

fn make_config(upstream_url: &str) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert(
        "test-openai".to_string(),
        UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: upstream_url.to_string(),
            api_key: Secret::new("test-key"),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
        }),
    );

    GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams,
        routes: vec![RouteEntry {
            frontend: "openai_chat".to_string(),
            model: "gpt-4o".to_string(),
            upstream: "test-openai".to_string(),
            upstream_model: "gpt-4o-2024-11-20".to_string(),
            reasoning_effort: None,
        }],
        copilot: None,
    }
}

async fn spawn_gateway(
    upstream_url: &str,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let cfg = make_config(upstream_url);
    let state = AppState::new(cfg).await;
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

#[tokio::test]
async fn e2e_openai_chat_streaming() {
    let mut mock_server = mockito::Server::new_async().await;

    let sse_body = concat!(
        "data: {\"id\":\"chatcmpl-e2e\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o-2024-11-20\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-e2e\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o-2024-11-20\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" E2E\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-e2e\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o-2024-11-20\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    let mock = mock_server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(sse_body)
        .create_async()
        .await;

    let (addr, tx) = spawn_gateway(&mock_server.url()).await;

    let request_body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": true
    });

    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", addr))
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
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

    // Consume the streaming body chunk-by-chunk, stopping when we see [DONE].
    let mut accumulated = String::new();
    let mut byte_stream = resp.bytes_stream();
    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk.expect("stream chunk error");
        let text = String::from_utf8_lossy(&chunk);
        accumulated.push_str(&text);
        if accumulated.contains("data: [DONE]") {
            break;
        }
    }

    assert!(
        accumulated.contains("Hello"),
        "expected 'Hello' in SSE body, got: {accumulated}"
    );
    assert!(
        accumulated.contains("E2E"),
        "expected 'E2E' in SSE body, got: {accumulated}"
    );
    assert!(accumulated.contains("[DONE]"), "expected [DONE] terminator");

    mock.assert_async().await;
    let _ = tx.send(());
}

#[tokio::test]
async fn e2e_openai_chat_unary() {
    let mut mock_server = mockito::Server::new_async().await;

    let mock = mock_server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "id": "chatcmpl-e2e-unary",
                "object": "chat.completion",
                "created": 1700000000,
                "model": "gpt-4o-2024-11-20",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "World"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
            }"#,
        )
        .create_async()
        .await;

    let (addr, tx) = spawn_gateway(&mock_server.url()).await;

    let request_body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "Hi"}],
        "stream": false
    });

    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", addr))
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("World"),
        "expected 'World' in response body, got: {body}"
    );

    mock.assert_async().await;
    let _ = tx.send(());
}
