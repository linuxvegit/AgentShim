//! Plan 05 P05 T7: end-to-end smoke test exercising all three v0.5
//! pillars (Prometheus metrics, OTel-shaped spans rendered through the
//! tracing fmt layer, and hot-reload) in a single flow.
//!
//! This test does NOT assert OTel exporter output (no collector); it
//! asserts the metric pipe shows the right counters, and that a reload
//! POST returns 200 and increments `agent_shim_config_reloads_total`.

use std::net::SocketAddr;

async fn pick_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn phase5_pillars_compose() {
    let public = pick_port().await;
    let admin = pick_port().await;
    let yaml = format!(
        r#"
server: {{bind: 127.0.0.1, port: {public}}}
admin: {{bind: 127.0.0.1, port: {admin}}}
upstreams:
  m: {{type: open_ai_compatible, base_url: http://x/v1, api_key: a}}
routes:
  - {{frontend: openai_chat, model: x, upstream: m, upstream_model: x}}
"#
    );
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(&yaml).unwrap();
    let public_addr: SocketAddr = format!("127.0.0.1:{public}").parse().unwrap();
    let admin_addr: SocketAddr = format!("127.0.0.1:{admin}").parse().unwrap();
    let (state, mut reload_rx) = agent_shim_gateway::state::AppState::new(cfg).await;

    // Drive the reload-applying task locally so POST /admin/reload's
    // mpsc send → oneshot reply round-trip completes inside the test.
    // The handler renamed `handle_reload_for_test` → `handle_reload`
    // in the Plan 04 review followup (commit 449b093); the function is
    // public because both production (`commands::serve::run`) and
    // integration tests share it.
    let state_for_task = state.clone();
    tokio::spawn(async move {
        while let Some(req) = reload_rx.recv().await {
            let o = agent_shim_gateway::commands::serve::handle_reload(&state_for_task, req.source)
                .await;
            let _ = req.respond_to.send(o);
        }
    });
    let pl = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let al = tokio::net::TcpListener::bind(admin_addr).await.unwrap();
    let pa = agent_shim_gateway::server::build_router(state.clone());
    let aa = agent_shim_gateway::admin::build_router(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(pl, pa).await;
    });
    tokio::spawn(async move {
        let _ = axum::serve(al, aa).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Pillar 3 (run first so the metrics-rs exporter has at least one
    // counter sample to render — metrics-rs registers counters lazily on
    // the first `increment()` call, so a fresh process with zero
    // request/reload traffic produces an empty scrape body).
    //
    // POST /admin/reload with the same YAML must validate cleanly and
    // return 200; this increments `agent_shim_config_reloads_total`.
    let resp = reqwest::Client::new()
        .post(format!("http://{}/admin/reload", admin_addr))
        .header("content-type", "application/yaml")
        .body(yaml.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "reload must succeed for same YAML");

    // Hit a public endpoint so the metrics middleware records a
    // request_started → request_completed pair. The handler will return
    // an error (no real provider) but the middleware fires regardless,
    // which is exactly what we want for the counter assertion below.
    let _ = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .body(r#"{"model":"x","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await;

    // Pillar 1: Prometheus metrics scrape works. After the above
    // traffic the exporter renders the request + reload counters.
    let m = reqwest::get(format!("http://{}/metrics", admin_addr))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        m.contains("agent_shim_requests_total"),
        "expected requests_total in metrics body; got:\n{m}"
    );

    // Pillar 2: in-flight gauge / reload counter visibility. We assert
    // the reload counter rather than the in-flight gauge because the
    // gauge is decremented back to 0 by the time we scrape, which the
    // text exporter may still render but is not the strongest signal.
    assert!(
        m.contains("agent_shim_config_reloads_total"),
        "expected config_reloads_total in metrics body; got:\n{m}"
    );
}
