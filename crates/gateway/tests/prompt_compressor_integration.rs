//! Phase 7 P06b2: end-to-end integration test for prompt_compressor.
//!
//! Validates the full production path:
//! YAML -> AppState::new -> ProviderRegistry built before PluginRegistry ->
//! FactoryDependencies populated -> PromptCompressorFactory resolves
//! `summarizer` upstream -> H2 calls provider -> upstream `main` receives
//! a body containing the summary message.

use agent_shim_config::GatewayConfig;
use agent_shim_gateway::{server::run_on_listener, state::AppState};

const REQUEST_BODY: &str = r#"{
    "model": "test-model",
    "max_tokens": 16,
    "messages": [
        {"role": "user", "content": "msg1"},
        {"role": "assistant", "content": "reply1"},
        {"role": "user", "content": "msg2"},
        {"role": "assistant", "content": "reply2"},
        {"role": "user", "content": "msg3"},
        {"role": "assistant", "content": "reply3"},
        {"role": "user", "content": "msg4"},
        {"role": "assistant", "content": "reply4"},
        {"role": "user", "content": "msg5"},
        {"role": "assistant", "content": "reply5"},
        {"role": "user", "content": "final question"}
    ]
}"#;

const UPSTREAM_OAI_RESPONSE: &str = r#"{
    "id": "chatcmpl-test",
    "object": "chat.completion",
    "created": 1700000000,
    "model": "test-model",
    "choices": [{
        "index": 0,
        "message": {"role": "assistant", "content": "ok"},
        "finish_reason": "stop"
    }],
    "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
}"#;

const SUMMARIZER_OAI_RESPONSE: &str = r#"{
    "id": "chatcmpl-summarizer",
    "object": "chat.completion",
    "created": 1700000000,
    "model": "cheap-model",
    "choices": [{
        "index": 0,
        "message": {"role": "assistant", "content": "FAKE SUMMARY"},
        "finish_reason": "stop"
    }],
    "usage": {"prompt_tokens": 100, "completion_tokens": 5, "total_tokens": 105}
}"#;

fn yaml_for(main_url: &str, summarizer_url: &str) -> String {
    format!(
        r#"
server:
  bind: 127.0.0.1
  port: 0
  keepalive_secs: 15
upstreams:
  main:
    type: open_ai_compatible
    base_url: "{main_url}"
    api_key: "test-key"
    tier: standard
  summarizer:
    type: open_ai_compatible
    base_url: "{summarizer_url}"
    api_key: "test-key"
    tier: standard
routes:
  - frontend: anthropic_messages
    model: test-model
    upstream: main
    upstream_model: test-model
    plugins:
      on_decoded_request:
        - compressor
plugins:
  compressor:
    type: prompt_compressor
    config:
      strategy:
        type: summarize_old_turns
        keep_last_n: 2
        summarizer:
          upstream: summarizer
          model: cheap-model
          max_summary_tokens: 100
          timeout_ms: 5000
"#
    )
}

#[tokio::test]
async fn prompt_compressor_summarize_path_end_to_end() {
    let mut main_upstream = mockito::Server::new_async().await;
    let mut summarizer_upstream = mockito::Server::new_async().await;

    // Main upstream MUST see a body containing "[Prior conversation summary]: FAKE SUMMARY".
    let main_mock = main_upstream
        .mock("POST", "/v1/chat/completions")
        .match_body(mockito::Matcher::AllOf(vec![mockito::Matcher::Regex(
            r"\[Prior conversation summary\]: FAKE SUMMARY".to_string(),
        )]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(UPSTREAM_OAI_RESPONSE)
        .expect(1)
        .create_async()
        .await;

    let summarizer_mock = summarizer_upstream
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SUMMARIZER_OAI_RESPONSE)
        .expect(1)
        .create_async()
        .await;

    let cfg: GatewayConfig =
        serde_yaml::from_str(&yaml_for(&main_upstream.url(), &summarizer_upstream.url()))
            .expect("yaml parses");
    let (state, _reload_rx) = AppState::new(cfg).await.expect("AppState::new");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        let _ = run_on_listener(listener, state, async {
            let _ = rx.await;
        })
        .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url = format!("http://{}/v1/messages", addr);
    let resp = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .body(REQUEST_BODY)
        .send()
        .await
        .expect("send request");

    assert_eq!(resp.status(), 200);

    let _ = tx.send(());
    let _ = server_handle.await;

    main_mock.assert_async().await;
    summarizer_mock.assert_async().await;
}
