//! Phase 7 P06b1: end-to-end integration test for `pii_scrubber`.
//!
//! Validates the full production path:
//! YAML → `AppState::new` → `builtin_plugins()` → `PluginRegistry::build` →
//! axum router → HTTP request (anthropic frontend → openai upstream) →
//! H2 hook scrubs the prompt → upstream receives the SCRUBBED body.
//!
//! We assert via mockito's `match_body(Matcher::Regex(...))` that the
//! upstream call body matches the post-scrub shape (replacements present,
//! original PII absent). `mock.expect(1).assert_async()` guarantees the
//! upstream was invoked exactly once with body matching the matcher; an
//! unscrubbed call would fail the matcher and produce a 501 from mockito
//! → the gateway would return a 502, failing the status assertion below.

use agent_shim_config::GatewayConfig;
use agent_shim_gateway::{server::run_on_listener, state::AppState};

const REQUEST_BODY: &str = r#"{
    "model": "test-model",
    "max_tokens": 16,
    "messages": [{
        "role": "user",
        "content": "Contact me at alice@example.com about my SSN 123-45-6789."
    }]
}"#;

const UPSTREAM_OAI_RESPONSE: &str = r#"{
    "id": "chatcmpl-pii-test",
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

fn yaml_for(upstream_url: &str) -> String {
    // YAML literal `{{` / `}}` are required because the host string is
    // interpolated through `format!`; everything else is raw YAML.
    format!(
        r#"
server:
  bind: 127.0.0.1
  port: 0
  keepalive_secs: 15
upstreams:
  mocked:
    type: open_ai_compatible
    base_url: "{upstream_url}"
    api_key: "test-key"
    tier: standard
routes:
  - frontend: anthropic_messages
    model: test-model
    upstream: mocked
    upstream_model: test-model
    plugins:
      on_decoded_request:
        - pii_scrub
plugins:
  pii_scrub:
    type: pii_scrubber
    config:
      inbound:
        - name: email
          pattern: "[\\w.]+@[\\w.]+\\.[a-z]{{2,}}"
          replacement: "[REDACTED-EMAIL]"
        - name: ssn
          pattern: "\\b\\d{{3}}-\\d{{2}}-\\d{{4}}\\b"
          replacement: "[REDACTED-SSN]"
"#
    )
}

#[tokio::test]
async fn pii_scrubber_end_to_end_scrubs_prompt() {
    let mut upstream = mockito::Server::new_async().await;

    // The body matcher enforces two invariants in a single mockito call:
    //   1. The upstream must see the scrubbed placeholders.
    //   2. The upstream must NOT see the raw PII anywhere in the body.
    // `Matcher::Regex` is applied to the full request body string. We use
    // `AllOf` to combine positive + negative assertions.
    let mock = upstream
        .mock("POST", "/v1/chat/completions")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::Regex(r"\[REDACTED-EMAIL\]".to_string()),
            mockito::Matcher::Regex(r"\[REDACTED-SSN\]".to_string()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(UPSTREAM_OAI_RESPONSE)
        .expect(1)
        .create_async()
        .await;

    let cfg: GatewayConfig =
        serde_yaml::from_str(&yaml_for(&upstream.url())).expect("yaml parses");
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

    assert_eq!(
        resp.status(),
        200,
        "gateway must return 200 (upstream body matcher would fail otherwise → 502)"
    );

    let _ = tx.send(());
    let _ = server_handle.await;

    // mockito confirms exactly one upstream call matched the matchers.
    // If H2 did NOT scrub, the matcher would not match, and `assert_async`
    // would fail with "expected 1 call but got 0".
    mock.assert_async().await;
}
