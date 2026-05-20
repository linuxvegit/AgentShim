//! Plan 04 P04 T5 — per-key rate-limit envelope smoke.
//!
//! Tiny bucket (1 RPS, burst 2): 3 sequential requests with the
//! same Authorization key produce 200, 200, 429 respectively. The
//! 429 carries a Retry-After header and a dialect-correct envelope:
//!   - OpenAI: error.type = "rate_limit_error", error.code =
//!     "rate_limited_per_key" (per P02 T5 + P04 T2 dimension naming).
//!   - Anthropic: error.type = "rate_limit_error", no code field
//!     (Anthropic dialect doesn't carry an `error.code`).
//!
//! This is the load-bearing operator-facing contract: SDKs key
//! catch blocks off `error.type`, dashboards key off `error.code`.
//! A regression in P02 T5 envelope generation OR in the rate-limit
//! integration in P04 T4 surfaces here as a body-shape mismatch.

use std::collections::BTreeMap;

use agent_shim_config::{
    schema::{
        AuthConfig, AuthKeyEntry, BreakerConfig, BucketConfigYaml, LoggingConfig,
        OpenAiCompatibleUpstream, PerKeyConfig, RateLimitConfig, RetryConfig, RouteEntry,
        ServerConfig, Tier, UpstreamConfig, UpstreamRef,
    },
    GatewayConfig, Secret,
};
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use agent_shim_router::hash_key;
use tokio::net::TcpListener;

const PLAINTEXT_KEY: &str = "test-secret";

/// Minimal OpenAI Chat Completions unary success body. Same shape as
/// the breaker-trip smoke uses — `OpenAiCompatibleProvider`'s unary
/// parser accepts this and produces a well-formed `CanonicalStream`
/// that either the OpenAI Chat or Anthropic Messages encoder can
/// serialize back to the client. We use this body for both dialects
/// because the test's load-bearing assertion is on the 429 envelope,
/// not the 200 body shape.
const OAI_CHAT_SUCCESS_BODY: &str = r#"{
    "id":"x",
    "object":"chat.completion",
    "created":1700000000,
    "model":"gpt-4o",
    "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
    "usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}
}"#;

/// Build the shared auth + rate-limit config blocks. Both fields key
/// off the SAME `sha256:<hex>` produced by `hash_key("test-secret")`.
/// `auth.required = false` is deliberate — T6 covers the required-but-
/// unauthorized path; here we only want the per_key bucket to fire
/// against the configured key.
fn auth_and_rate_limit_for(key_hash: &str) -> (AuthConfig, RateLimitConfig) {
    let mut keys = BTreeMap::new();
    keys.insert(
        key_hash.to_string(),
        AuthKeyEntry {
            label: "test".to_string(),
        },
    );
    let auth = AuthConfig {
        enabled: true,
        required: false,
        keys,
    };

    // Tiny bucket: 1 RPS, burst 2 → request 1 + 2 succeed (consume the
    // initial burst), request 3 lands before 1 second has elapsed so
    // the bucket is empty and the gate returns RateLimited.
    let mut overrides = BTreeMap::new();
    overrides.insert(
        key_hash.to_string(),
        BucketConfigYaml {
            rate_per_sec: 1,
            burst: 2,
        },
    );
    let rate_limit = RateLimitConfig {
        enabled: true,
        per_key: PerKeyConfig {
            default: None,
            anonymous: None,
            overrides,
        },
        per_route: BTreeMap::new(),
        per_upstream: BTreeMap::new(),
        per_ip: Default::default(),
    };
    (auth, rate_limit)
}

/// Build a single-upstream OpenAI Chat config gated by the per-key
/// bucket. Retries disabled (`max_attempts: 1`) so the rate-limit
/// gate's pass/reject decision is not muddied by retry loops, and
/// breaker disabled so 429 comes from the per-key bucket and not
/// from a tripped breaker.
fn make_chat_config(oai_url: &str, key_hash: &str) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert(
        "oai".to_string(),
        UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: oai_url.to_string(),
            api_key: Secret::new("test-key"),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
            tier: Tier::Standard,
            cost: None,
            p95_latency_budget_ms: None,
        }),
    );

    let (auth, rate_limit) = auth_and_rate_limit_for(key_hash);

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
            min_tier: None,
            max_cost_usd: None,
            plugins: None,
        }],
        plugins: ::std::collections::BTreeMap::new(),
        auth,
        rate_limit,
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
        shutdown: Default::default(),
    }
}

/// Build a single-upstream Anthropic Messages config gated by the
/// per-key bucket. We deliberately back the route with an
/// OpenAI-compatible upstream rather than an Anthropic upstream:
/// `AnthropicProvider::proxy_raw` returns `Some(_)` for inbound
/// AnthropicMessages and the pipeline streams the bytes back without
/// entering `ResilientCaller::complete`, which is where the per-key
/// rate-limit gate lives. Routing the Anthropic-shaped inbound to an
/// OAI-compat upstream forces the canonical decode + chain walk path
/// (proxy_raw returns `None` for non-Responses inbound dialects),
/// which is what fires the per-key bucket. The 429 envelope is still
/// shaped per the inbound `anthropic_messages` frontend, which is the
/// load-bearing assertion for this test (envelope shape, not upstream
/// wire format).
fn make_anthropic_config(oai_url: &str, key_hash: &str) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert(
        "oai".to_string(),
        UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: oai_url.to_string(),
            api_key: Secret::new("test-key"),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 30,
            tier: Tier::Standard,
            cost: None,
            p95_latency_budget_ms: None,
        }),
    );

    let (auth, rate_limit) = auth_and_rate_limit_for(key_hash);

    GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams,
        routes: vec![RouteEntry {
            frontend: "anthropic_messages".to_string(),
            model: "claude-opus-4-7".to_string(),
            upstream: None,
            upstream_model: None,
            upstreams: vec![UpstreamRef {
                name: "oai".to_string(),
                model: "gpt-4o-up".to_string(),
            }],
            reasoning_effort: None,
            anthropic_beta: None,
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
            min_tier: None,
            max_cost_usd: None,
            plugins: None,
        }],
        plugins: ::std::collections::BTreeMap::new(),
        auth,
        rate_limit,
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
        shutdown: Default::default(),
    }
}

async fn spawn_with_config(
    config: GatewayConfig,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let (state, _reload_rx) = AppState::new(config).await.unwrap();
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
async fn per_key_rate_limit_returns_429_with_openai_envelope_chat() {
    let key_hash = hash_key(PLAINTEXT_KEY);

    // Upstream returns 200 for every chat-completions POST. With
    // burst=2 only the first two inbound requests should be allowed
    // through to it; `expect(2)` asserts the rate-limit gate prevents
    // request 3 from leaking through to the upstream.
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(OAI_CHAT_SUCCESS_BODY)
        .expect(2)
        .create_async()
        .await;

    let cfg = make_chat_config(&server.url(), &key_hash);
    let (addr, tx) = spawn_with_config(cfg).await;

    let request_body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let url = format!("http://{}/v1/chat/completions", addr);
    let client = reqwest::Client::new();

    // Requests 1 + 2: consume the burst. The Authorization header
    // carries the plaintext key — the gateway hashes it and looks up
    // the resulting `sha256:<hex>` in `auth.keys` to identify the
    // caller, then keys the per_key bucket off the same hash.
    for i in 0..2 {
        let resp = client
            .post(&url)
            .header("authorization", format!("Bearer {}", PLAINTEXT_KEY))
            .header("content-type", "application/json")
            .json(&request_body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("request {i} failed: {e}"));
        assert_eq!(
            resp.status(),
            200,
            "request {i}: expected 200 (burst not yet consumed); body: {}",
            resp.text().await.unwrap_or_default()
        );
    }

    // Request 3: bucket empty, gate rejects before chain walk. We
    // assert this WITHOUT a sleep between requests 2 and 3 — at
    // 1 RPS, even a few-millisecond test can refill at most a tiny
    // fractional token, so an empty bucket is the only viable state.
    let resp = client
        .post(&url)
        .header("authorization", format!("Bearer {}", PLAINTEXT_KEY))
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("request 3 send failed");
    assert_eq!(resp.status(), 429, "request 3: expected 429");

    // Retry-After: required by P02 T5 + RFC 6585 §4. Real SDKs key
    // off this header to schedule a backoff; missing it would let
    // a polite client hammer the gateway.
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    assert!(
        retry_after.is_some(),
        "request 3: missing Retry-After header"
    );
    assert!(
        retry_after.as_deref().unwrap().parse::<u32>().is_ok(),
        "Retry-After must be an integer-seconds value, got: {retry_after:?}"
    );

    // OpenAI envelope shape (P02 T5 + P04 T2 dimension naming).
    // `error.type = "rate_limit_error"` is what the OpenAI Python
    // SDK matches in its `RateLimitError` catch; `error.code =
    // "rate_limited_per_key"` is the dashboard-facing dimension tag.
    let body: serde_json::Value = resp.json().await.expect("request 3 json body");
    assert_eq!(
        body["error"]["type"], "rate_limit_error",
        "OpenAI envelope error.type mismatch: {body}"
    );
    assert_eq!(
        body["error"]["code"], "rate_limited_per_key",
        "OpenAI envelope error.code mismatch: {body}"
    );
    let msg = body["error"]["message"]
        .as_str()
        .expect("error.message must be a string");
    assert!(
        msg.contains("retry"),
        "error.message must mention retry; got: {msg}"
    );

    // mockito's `expect(2)`: the gate prevented request 3 from
    // reaching the upstream. If the gate were broken, mockito would
    // see 3 hits and `assert_async` would fail.
    upstream.assert_async().await;
    let _ = tx.send(());
}

#[tokio::test]
async fn per_key_rate_limit_uses_anthropic_envelope_for_messages_dialect() {
    let key_hash = hash_key(PLAINTEXT_KEY);

    // Upstream is OpenAI-compatible (chat completions). The route is
    // `anthropic_messages → oai-compat`, so the gateway decodes the
    // Anthropic-shaped inbound, calls the OAI-compat upstream, and
    // re-encodes the canonical response back into Anthropic shape on
    // the way out. proxy_raw doesn't apply (only OpenAI Responses uses
    // it), so the canonical chain walk runs for every request — which
    // is the only path that consults the per-key rate-limit gate.
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(OAI_CHAT_SUCCESS_BODY)
        .expect(2)
        .create_async()
        .await;

    let cfg = make_anthropic_config(&server.url(), &key_hash);
    let (addr, tx) = spawn_with_config(cfg).await;

    // Anthropic Messages request body — minimal but valid: model +
    // max_tokens + a single user message. `stream: false` keeps the
    // path on the unary branch; the 429 envelope shape is identical
    // for streaming and unary so this doesn't weaken the assertion.
    let request_body = serde_json::json!({
        "model": "claude-opus-4-7",
        "max_tokens": 32,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let url = format!("http://{}/v1/messages", addr);
    let client = reqwest::Client::new();

    for i in 0..2 {
        let resp = client
            .post(&url)
            .header("authorization", format!("Bearer {}", PLAINTEXT_KEY))
            .header("content-type", "application/json")
            .json(&request_body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("request {i} failed: {e}"));
        assert_eq!(
            resp.status(),
            200,
            "request {i}: expected 200 (burst not yet consumed); body: {}",
            resp.text().await.unwrap_or_default()
        );
    }

    let resp = client
        .post(&url)
        .header("authorization", format!("Bearer {}", PLAINTEXT_KEY))
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .expect("request 3 send failed");
    assert_eq!(resp.status(), 429, "request 3: expected 429");

    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    assert!(
        retry_after.is_some(),
        "request 3: missing Retry-After header"
    );

    // Anthropic envelope shape (P02 T5): `{type:"error", error:
    // {type:"rate_limit_error", message:"..."}}`. No `code` field —
    // Anthropic's wire format doesn't carry one. Real Anthropic
    // SDKs (anthropic-python, anthropic-typescript) match `type` ==
    // "error" first, then the inner `error.type` to choose an
    // exception class.
    let body: serde_json::Value = resp.json().await.expect("request 3 json body");
    assert_eq!(
        body["type"], "error",
        "Anthropic envelope outer type mismatch: {body}"
    );
    assert_eq!(
        body["error"]["type"], "rate_limit_error",
        "Anthropic envelope error.type mismatch: {body}"
    );
    assert!(
        body["error"]["message"].is_string(),
        "error.message must be a string; got: {body}"
    );
    // Critical D9 invariant: Anthropic dialect must NOT carry an
    // `error.code` field. A regression that copies the OpenAI body
    // builder onto the Anthropic path would surface here.
    assert!(
        body["error"].get("code").is_none(),
        "Anthropic envelope must NOT carry error.code; got: {body}"
    );

    upstream.assert_async().await;
    let _ = tx.send(());
}
