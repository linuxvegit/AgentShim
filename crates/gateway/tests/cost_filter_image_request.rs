//! End-to-end test: an image-laden request hits a cost cap that a
//! text-only request wouldn't. Plan v0.6.1 P04 (M-8, T8).
//!
//! Routes a single Anthropic Messages frontend at one OpenAI-compatible
//! upstream with `input_per_million_usd = 3.0`. The route's
//! `max_cost_usd = 0.001` is sized so that:
//!
//!   * The image variant: AnthropicImageEstimator's Unknown-fallback
//!     (1600 tokens) × 3.0 / 1M ≈ 0.0048 USD → BREACHES the cap →
//!     filter rejects the only upstream → resilience layer surfaces
//!     `NoEligibleUpstream` → HTTP 503 with the Anthropic-shaped
//!     envelope + `filtered:[{upstream, reason:"cap"}]` array.
//!
//!   * The text-only variant: a couple of tokens × 3.0 / 1M plus
//!     `max_tokens × output_per_million / 1M` is well under the cap
//!     → filter passes the upstream through → request reaches the
//!     mockito server, which stubs a 200 OAI Chat Completions body
//!     that the Anthropic Messages encoder serialises back to the
//!     client.
//!
//! Why the inbound is Anthropic but the upstream is OAI-compat:
//! `AnthropicProvider::proxy_raw` returns `Some(_)` for inbound
//! AnthropicMessages and bypasses `ResilientCaller::complete` (which
//! is where the cost filter lives), so the canonical-path entry point
//! is never reached. Routing the Anthropic-shaped inbound to an
//! OAI-compat upstream forces the canonical decode + chain walk path
//! (proxy_raw returns `None` for non-Responses inbound dialects),
//! which is where the cost filter actually fires. The frontend kind
//! is what selects `AnthropicImageEstimator` (via
//! `image_estimator_selector::select_image_estimator`), so the
//! Anthropic-vendor 1600-token Unknown-fallback is what's charged
//! against the cap. This mirrors the same upstream-routing trick that
//! `rate_limit_per_key_envelope.rs` uses to exercise the canonical
//! path under an Anthropic frontend.
//!
//! Mirrors the structure of `cost_filter_full_skip.rs` (v0.6.0).

use std::collections::BTreeMap;

use agent_shim_config::{
    schema::{
        BreakerConfig, LoggingConfig, OpenAiCompatibleUpstream, RetryConfig, RouteEntry,
        ServerConfig, Tier, UpstreamConfig, UpstreamCost, UpstreamRef,
    },
    GatewayConfig, Secret,
};
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use tokio::net::TcpListener;

/// Minimal OpenAI Chat Completions unary success body — the
/// `OpenAiCompatibleProvider`'s unary parser produces a well-formed
/// `CanonicalStream` that the Anthropic Messages encoder can serialise
/// back to the client. Same shape used by `cost_filter_tier_partial.rs`
/// and `rate_limit_per_key_envelope.rs`.
const OAI_CHAT_SUCCESS_BODY: &str = r#"{
    "id":"x",
    "object":"chat.completion",
    "created":1700000000,
    "model":"claude-3-5-haiku",
    "choices":[{"index":0,"message":{"role":"assistant","content":"hello back"},"finish_reason":"stop"}],
    "usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}
}"#;

fn upstream(base_url: &str) -> UpstreamConfig {
    UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
        base_url: base_url.to_string(),
        api_key: Secret::new("test-key"),
        default_headers: BTreeMap::new(),
        request_timeout_secs: 30,
        tier: Tier::Standard,
        // input_per_million_usd = 3.0 is roughly Anthropic Sonnet's
        // documented input rate. Combined with the 1600-token Unknown
        // fallback this gives an image cost of ~0.0048 USD; combined
        // with a few text tokens it gives well under 0.001 USD. The
        // cap is set in `make_config` to land between these two.
        cost: Some(UpstreamCost {
            input_per_million_usd: 3.0,
            output_per_million_usd: 3.0,
        }),
        p95_latency_budget_ms: None,
    })
}

fn make_config(upstream_url: &str) -> GatewayConfig {
    let mut upstreams = BTreeMap::new();
    upstreams.insert("oai".to_string(), upstream(upstream_url));

    GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams,
        routes: vec![RouteEntry {
            frontend: "anthropic_messages".to_string(),
            model: "claude-3-5-haiku".to_string(),
            upstream: None,
            upstream_model: None,
            upstreams: vec![UpstreamRef {
                name: "oai".to_string(),
                model: "claude-3-5-haiku".to_string(),
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
            // Window: image-bearing variant breaches (~0.0048 USD),
            // text-only variant passes (~few text tokens + 16 output
            // tokens × 3.0/1M ≈ 0.00006 USD). 0.001 sits comfortably
            // inside the gap.
            max_cost_usd: Some(0.001),
        }],
        plugins: ::std::collections::BTreeMap::new(),
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
    }
}

async fn spawn_gateway(
    upstream_url: &str,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let cfg = make_config(upstream_url);
    let (state, _reload_rx) = AppState::new(cfg).await;
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

/// Anthropic-style request body carrying a single image block alongside
/// a small text block. The image source is the URL form because it's
/// trivially well-formed; the canonical decoder produces a
/// `ContentBlock::Image(_)` that the cost estimator counts at the
/// AnthropicImageEstimator's Unknown-fallback (1600 tokens). See
/// `crates/frontends/src/anthropic_messages/decode.rs::decode_image_url_source_yields_canonical_image_block`.
const ANTHROPIC_IMAGE_BODY: &str = r#"{
    "model": "claude-3-5-haiku",
    "max_tokens": 16,
    "messages": [{
        "role": "user",
        "content": [
            {"type": "text", "text": "describe this"},
            {"type": "image", "source": {"type": "url", "url": "https://example.com/cat.png"}}
        ]
    }]
}"#;

/// Same request stripped of the image block. With only a few input
/// tokens and a tiny `max_tokens` cap, the estimated cost is ≈0.00006
/// USD — well under the route's 0.001 cap.
const ANTHROPIC_TEXT_ONLY_BODY: &str = r#"{
    "model": "claude-3-5-haiku",
    "max_tokens": 16,
    "messages": [{
        "role": "user",
        "content": [
            {"type": "text", "text": "describe this"}
        ]
    }]
}"#;

#[tokio::test]
async fn image_request_hits_cap_text_only_passes_through() {
    // mockito server: `expect(1)` — only the text-only variant should
    // reach the upstream. The image variant must be filtered before
    // the upstream is contacted; if a regression let it through, this
    // hit count would be 2 and `assert_async` would fail.
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(OAI_CHAT_SUCCESS_BODY)
        .expect(1)
        .create_async()
        .await;

    let (addr, tx) = spawn_gateway(&server.url()).await;
    let client = reqwest::Client::new();

    // ── 1. Image-bearing request: cost cap rejects, HTTP 503. ─────────
    let resp_image = client
        .post(format!("http://{}/v1/messages", addr))
        .header("content-type", "application/json")
        .body(ANTHROPIC_IMAGE_BODY)
        .send()
        .await
        .expect("image request send");
    assert_eq!(
        resp_image.status(),
        503,
        "image-laden request should breach the cost cap and surface NoEligibleUpstream"
    );

    // Body shape: Anthropic envelope + the additive `filtered` array.
    // Same envelope assertions as `cost_filter_full_skip.rs` but in
    // the Anthropic dialect (per `anthropic_envelope_body` in
    // `crates/gateway/src/handlers/mod.rs`).
    let body: serde_json::Value = resp_image.json().await.expect("json body");
    assert_eq!(
        body["type"], "error",
        "Anthropic envelope type mismatch: {body}"
    );
    assert_eq!(
        body["error"]["type"], "overloaded_error",
        "Anthropic envelope error.type mismatch: {body}"
    );
    let filtered = body["filtered"]
        .as_array()
        .unwrap_or_else(|| panic!("expected `filtered` to be an array, got: {body}"));
    assert_eq!(
        filtered.len(),
        1,
        "expected exactly one filtered entry (the single upstream); got: {body}"
    );
    let entry = &filtered[0];
    assert_eq!(
        entry["reason"], "cap",
        "filtered entry should carry reason=cap; got: {entry}"
    );
    assert_eq!(
        entry["upstream"], "oai",
        "filtered entry should reference the only upstream; got: {entry}"
    );

    // ── 2. Text-only request: cost cap passes, HTTP 200. ──────────────
    let resp_text = client
        .post(format!("http://{}/v1/messages", addr))
        .header("content-type", "application/json")
        .body(ANTHROPIC_TEXT_ONLY_BODY)
        .send()
        .await
        .expect("text-only request send");
    let status = resp_text.status();
    let text_body = resp_text.text().await.unwrap_or_default();
    assert_eq!(
        status, 200,
        "text-only request should pass the cap and reach the upstream; body: {text_body}"
    );
    // The mockito body has the assistant content "hello back"; the
    // Anthropic Messages encoder serialises it through to the client,
    // so its presence end-to-end proves the canonical decode + encode
    // chain delivered the upstream's bytes (and that the cost filter
    // didn't accidentally short the text-only branch too).
    assert!(
        text_body.contains("hello back"),
        "expected upstream content 'hello back' in response body; got: {text_body}"
    );

    // mockito confirms exactly 1 hit: the image variant never reached
    // the upstream, the text-only variant did.
    mock.assert_async().await;

    let _ = tx.send(());
}
