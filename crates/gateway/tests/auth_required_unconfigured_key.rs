//! Plan 04 P04 T6 — auth.required=true gates the pipeline before any
//! provider is contacted.
//!
//! Three sequential, independent scenarios share one config:
//!   - Unknown key (header present but not in `auth.keys`)  → HTTP 401.
//!   - No key (no Authorization, no x-api-key)              → HTTP 401.
//!   - Known key (matches the configured hash)              → HTTP 200.
//!
//! Load-bearing assertions on the rejection paths:
//!   1. The upstream mock is `expect(0)`. This is the regression net
//!      for the gate's *position*: P04 T4-followup moved the auth
//!      check ahead of route resolution + chain walk, and a future
//!      refactor that lets the request slip past the gate would show
//!      up as a non-zero hit count on the mock — `assert_async` would
//!      fail with a hit-count mismatch.
//!   2. `error.type = "authentication_error"`. SDKs key their auth
//!      retry / re-prompt logic off this discriminator (the OpenAI
//!      Python SDK raises `AuthenticationError` for it).
//!   3. `WWW-Authenticate: Bearer realm="agent-shim"` is present.
//!      RFC 7235 §3.1 requires this on 401 responses, and HTTP libs
//!      (curl --anyauth, retrying SDK middlewares) need it to know
//!      the server is challenging for credentials rather than failing
//!      for some unrelated reason.
//!
//! The success path proves the inverse: a request that DOES carry a
//! configured key reaches the upstream (mock `expect(1)` + 200 body).

use std::collections::BTreeMap;

use agent_shim_config::{
    schema::{
        AuthConfig, AuthKeyEntry, BreakerConfig, LoggingConfig, OpenAiCompatibleUpstream,
        RateLimitConfig, RetryConfig, RouteEntry, ServerConfig, UpstreamConfig, UpstreamRef,
    },
    GatewayConfig, Secret,
};
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use agent_shim_router::hash_key;
use tokio::net::TcpListener;

/// Plaintext key the success-path test sends. The gateway hashes it
/// on receipt and compares against `auth.keys`, which we populate
/// with `hash_key(KNOWN_PLAINTEXT_KEY)` — i.e. the same `sha256:<hex>`.
const KNOWN_PLAINTEXT_KEY: &str = "known-key";

/// Minimal OpenAI Chat Completions unary success body, identical in
/// shape to the one used by the T5 per-key rate-limit envelope test.
/// `OpenAiCompatibleProvider` accepts this and produces a well-formed
/// `CanonicalStream` that the OpenAI Chat encoder can serialize back.
const OAI_CHAT_SUCCESS_BODY: &str = r#"{
    "id":"x",
    "object":"chat.completion",
    "created":1700000000,
    "model":"gpt-4o",
    "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
    "usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}
}"#;

/// Build a single-route OpenAI Chat config with `auth.enabled=true,
/// auth.required=true` and exactly one known key hash. Rate-limiting
/// is left disabled (default) — T5 covers that path; this test isolates
/// the auth gate.
///
/// `auth.required=true` requires `auth.enabled=true` (P04 T3 validation
/// rule). Both are set here.
fn make_config(upstream_url: &str, known_hash: &str) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert(
        "oai".to_string(),
        UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: upstream_url.to_string(),
            api_key: Secret::new("test-key"),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
        }),
    );

    let mut keys = BTreeMap::new();
    keys.insert(
        known_hash.to_string(),
        AuthKeyEntry {
            label: "alice".to_string(),
        },
    );
    let auth = AuthConfig {
        enabled: true,
        required: true,
        keys,
    };

    GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams,
        routes: vec![RouteEntry {
            frontend: "openai_chat".to_string(),
            model: "gpt-4o".to_string(),
            upstream: None,
            upstream_model: None,
            upstreams: vec![UpstreamRef {
                name: "oai".to_string(),
                model: "gpt-4o-up".to_string(),
            }],
            reasoning_effort: None,
            anthropic_beta: None,
            // Retries disabled (`max_attempts: 1`) so a hypothetical
            // upstream hit on the rejection paths can't be masked by
            // the retry loop: any contact at all would surface in the
            // mock's `expect(0)`.
            retry: RetryConfig {
                max_attempts: 1,
                initial_backoff_ms: 1,
                multiplier: 2.0,
                jitter_pct: 0.0,
                total_budget_ms: 100,
                retry_on: vec![
                    "network".into(),
                    "upstream_5xx".into(),
                    "upstream_429".into(),
                ],
            },
            breaker: BreakerConfig::default(),
        }],
        auth,
        rate_limit: RateLimitConfig::default(),
        copilot: None,
    }
}

/// Spawn the gateway on an ephemeral port with the given config.
/// Returns the bound address and a oneshot sender used to trigger
/// graceful shutdown at the end of the test.
async fn spawn_gateway(
    upstream_url: &str,
    known_hash: &str,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let cfg = make_config(upstream_url, known_hash);
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
    // 50ms is comfortably above the time it takes axum to start
    // accepting on the listener — same value the T5 test uses.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (addr, tx)
}

#[tokio::test]
async fn auth_required_with_unknown_key_returns_401() {
    let known_hash = hash_key(KNOWN_PLAINTEXT_KEY);

    // `expect(0)`: load-bearing. If the auth gate regressed and let
    // the unknown key through, the request would reach this mock and
    // `assert_async` would fail with a hit-count mismatch.
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(OAI_CHAT_SUCCESS_BODY)
        .expect(0)
        .create_async()
        .await;

    let (addr, tx) = spawn_gateway(&server.url(), &known_hash).await;

    let request_body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let url = format!("http://{}/v1/chat/completions", addr);

    let resp = reqwest::Client::new()
        .post(&url)
        .header("authorization", "Bearer wrong-key")
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("request send failed");

    assert_eq!(resp.status(), 401, "unknown key must yield 401");

    // RFC 7235 §3.1 challenge header. SDKs/curl key auth-retry off it.
    let www_auth = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    assert_eq!(
        www_auth.as_deref(),
        Some("Bearer realm=\"agent-shim\""),
        "401 must carry RFC 7235 challenge header",
    );

    // OpenAI dialect envelope shape.
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(
        body["error"]["type"], "authentication_error",
        "OpenAI envelope error.type mismatch: {body}"
    );

    // Confirms the upstream was never contacted.
    upstream.assert_async().await;
    let _ = tx.send(());
}

#[tokio::test]
async fn auth_required_with_no_key_returns_401() {
    let known_hash = hash_key(KNOWN_PLAINTEXT_KEY);

    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(OAI_CHAT_SUCCESS_BODY)
        .expect(0)
        .create_async()
        .await;

    let (addr, tx) = spawn_gateway(&server.url(), &known_hash).await;

    let request_body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let url = format!("http://{}/v1/chat/completions", addr);

    // No Authorization, no x-api-key — `extract_identity_from_headers`
    // returns Anonymous and the gate rejects it.
    let resp = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("request send failed");

    assert_eq!(resp.status(), 401, "missing key must yield 401");

    let www_auth = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    assert_eq!(
        www_auth.as_deref(),
        Some("Bearer realm=\"agent-shim\""),
        "401 must carry RFC 7235 challenge header",
    );

    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(
        body["error"]["type"], "authentication_error",
        "OpenAI envelope error.type mismatch: {body}"
    );

    upstream.assert_async().await;
    let _ = tx.send(());
}

#[tokio::test]
async fn auth_required_with_known_key_passes() {
    let known_hash = hash_key(KNOWN_PLAINTEXT_KEY);

    // Mirror image of the rejection tests: `expect(1)` confirms the
    // request DID reach the upstream when the presented key is in the
    // allowlist.
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(OAI_CHAT_SUCCESS_BODY)
        .expect(1)
        .create_async()
        .await;

    let (addr, tx) = spawn_gateway(&server.url(), &known_hash).await;

    let request_body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let url = format!("http://{}/v1/chat/completions", addr);

    // Plaintext key on the wire — the gateway hashes it and matches
    // against the `sha256:<hex>` we put in `auth.keys`.
    let resp = reqwest::Client::new()
        .post(&url)
        .header("authorization", format!("Bearer {}", KNOWN_PLAINTEXT_KEY))
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("request send failed");

    assert_eq!(
        resp.status(),
        200,
        "known key must reach upstream; body: {}",
        resp.text().await.unwrap_or_default()
    );

    upstream.assert_async().await;
    let _ = tx.send(());
}
