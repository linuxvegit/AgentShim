//! Plan 02 P02 T5: a reload that changes `rate_limit.*` produces a new
//! `LimiterRegistry` on the very next request.
//!
//! Strategy:
//! 1. Boot the gateway with `rate_limit.enabled = false`.
//! 2. POST `/v1/chat/completions` — should NOT be 429 (upstream is
//!    unreachable so it'll be 5xx, but specifically not 429).
//! 3. POST `/admin/reload` with `rate_limit { enabled: true, per_route { ..
//!    burst: 1 } }`. Assert 200.
//! 4. POST `/v1/chat/completions` twice CONCURRENTLY against the
//!    just-reloaded bucket (burst=1, rate_per_sec=1). Exactly one of the
//!    two MUST be 429 — the new bucket only had a single token.
//!
//! Why concurrent and not sequential? Each upstream call against the
//! unreachable port takes ~1-2s to fail with connection refused on
//! Windows. Sequential requests would let the bucket refill (rate=1/sec)
//! between them and we'd lose the swap signal. Concurrent dispatch makes
//! the assertion timing-independent.
//!
//! Without P02's atomic-swap, both post-reload requests would hit the
//! OLD (disabled) registry and BOTH would be 5xx-not-429. The fact that
//! exactly one is 429 proves the swap took effect on the next request.
//! This closes the v0.5 §5.4 "deferred" deviation.

use std::net::SocketAddr;

async fn pick_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn yaml_for(public: u16, admin: u16, rate_limited: bool) -> String {
    let rl = if rate_limited {
        r#"rate_limit:
  enabled: true
  per_route:
    "openai_chat/x":
      rate_per_sec: 1
      burst: 1
"#
    } else {
        r#"rate_limit:
  enabled: false
"#
    };
    format!(
        r#"
server: {{bind: 127.0.0.1, port: {public}}}
admin: {{bind: 127.0.0.1, port: {admin}}}
{rl}
upstreams:
  m:
    type: open_ai_compatible
    base_url: http://localhost:9999/v1
    api_key: dummy
routes:
  - frontend: openai_chat
    model: x
    upstream: m
    upstream_model: x
    # No retries — each attempt fails fast against the unreachable
    # upstream so the test can fire two requests within the 1s window
    # of the post-reload token bucket (rate=1/sec, burst=1). Default
    # max_attempts=2 with total_budget_ms=5000 would let the bucket
    # refill before the second request lands.
    retry:
      max_attempts: 1
      initial_backoff_ms: 1
      total_budget_ms: 1
"#
    )
}

#[tokio::test]
async fn rate_limit_reload_takes_effect_on_next_request() {
    let public = pick_port().await;
    let admin = pick_port().await;
    let initial_yaml = yaml_for(public, admin, /*rate_limited=*/ false);
    let updated_yaml = yaml_for(public, admin, /*rate_limited=*/ true);

    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(&initial_yaml).unwrap();
    let admin_cfg = cfg.admin.clone().expect("admin block required");
    let public_addr: SocketAddr = format!("{}:{}", cfg.server.bind, cfg.server.port)
        .parse()
        .unwrap();
    let admin_addr: SocketAddr = format!("{}:{}", admin_cfg.bind, admin_cfg.port)
        .parse()
        .unwrap();
    let (state, mut reload_rx) = agent_shim_gateway::state::AppState::new(cfg).await;

    // Run the reload-applying task locally inside the test. This mirrors
    // `commands::serve::run`'s task body so the handler's mpsc send →
    // oneshot reply round-trip actually completes.
    let state_for_task = state.clone();
    tokio::spawn(async move {
        while let Some(req) = reload_rx.recv().await {
            let outcome =
                agent_shim_gateway::commands::serve::handle_reload(&state_for_task, req.source)
                    .await;
            let _ = req.respond_to.send(outcome);
        }
    });

    let pl = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let al = tokio::net::TcpListener::bind(admin_addr).await.unwrap();
    let pa = agent_shim_gateway::server::build_router(state.clone());
    let aa = agent_shim_gateway::admin::build_router(state);
    tokio::spawn(async move {
        let _ = axum::serve(pl, pa).await;
    });
    tokio::spawn(async move {
        let _ = axum::serve(al, aa).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    let body = r#"{"model":"x","messages":[{"role":"user","content":"hi"}]}"#;

    // (1) Pre-reload: rate-limit disabled — request goes through to the
    //     (unreachable) upstream, returns 5xx but NOT 429.
    let pre = client
        .post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_ne!(
        pre.status(),
        429,
        "pre-reload should NOT be 429; rate-limit is off (got {})",
        pre.status()
    );

    // (2) Reload to enabled with burst=1.
    let reload_resp = client
        .post(format!("http://{}/admin/reload", admin_addr))
        .header("content-type", "application/yaml")
        .body(updated_yaml.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(reload_resp.status(), 200, "reload must succeed");

    // (3) Fire two post-reload requests concurrently. The new bucket has
    //     burst=1 and rate=1/sec: exactly one of the two MUST get the
    //     token (status != 429); the other MUST be 429.
    //
    //     We must fire concurrently because each upstream call against
    //     the unreachable port takes ~1-2s to fail with connection
    //     refused on Windows. If we issued them sequentially, the bucket
    //     would refill (rate=1/sec) before the second request arrived,
    //     and we'd see two non-429 results — losing the swap signal.
    //
    //     Without P02's atomic-swap, the second request would hit the
    //     OLD (disabled) registry and BOTH would be 5xx-not-429. So the
    //     fact that *exactly one* is 429 proves the swap took effect.
    let url = format!("http://{}/v1/chat/completions", public_addr);
    let req_a = client
        .post(&url)
        .header("content-type", "application/json")
        .body(body)
        .send();
    let req_b = client
        .post(&url)
        .header("content-type", "application/json")
        .body(body)
        .send();
    let (resp_a, resp_b) = tokio::join!(req_a, req_b);
    let status_a = resp_a.unwrap().status();
    let status_b = resp_b.unwrap().status();

    let any_429 = status_a == 429 || status_b == 429;
    let any_non_429 = status_a != 429 || status_b != 429;
    assert!(
        any_429,
        "post-reload bucket (burst=1) must reject one of two concurrent \
         requests with 429; got a={} b={}",
        status_a, status_b
    );
    assert!(
        any_non_429,
        "post-reload bucket (burst=1) must admit one of two concurrent \
         requests; got a={} b={} (both 429 means routing bypassed)",
        status_a, status_b
    );
}
