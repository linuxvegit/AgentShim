//! Plan 04 P04 T7: breaker state survives reload.
//!
//! The `BreakerRegistry` lives on `AppCore` (immutable across reload) — only
//! `AppSnapshot` (config + auth) gets swapped. This test pins that invariant
//! by tripping the breaker, triggering a reload through the same channel the
//! production handler uses, and verifying the breaker is still tripped after
//! the swap completes.

#[tokio::test]
async fn breaker_state_survives_reload() {
    use agent_shim_router::circuit_breaker::{BreakerPolicy, BreakerRegistry};

    let public = pick_port().await;
    let admin = pick_port().await;
    let yaml = format!(
        r#"
server: {{bind: 127.0.0.1, port: {public}}}
admin: {{bind: 127.0.0.1, port: {admin}}}
upstreams:
  m: {{type: open_ai_compatible, base_url: http://x/v1, api_key: a}}
routes:
  - frontend: openai_chat
    model: x
    upstream: m
    upstream_model: x
    breaker:
      enabled: true
      failure_threshold_pct: 50
      min_requests: 2
      window_secs: 60
      open_cooldown_secs: 60
"#
    );
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(&yaml).unwrap();
    let (state, mut reload_rx) = agent_shim_gateway::state::AppState::new(cfg).await;

    // Compile-time check that `breaker_registry` is the expected type — pins
    // the assumption that the registry sits on `AppCore` and therefore
    // survives the snapshot swap.
    let _: &std::sync::Arc<BreakerRegistry> = &state.core.breaker_registry;

    // Spawn the reload-applying task locally, mirroring
    // `commands::serve::run`'s task body so the mpsc send → oneshot reply
    // round-trip actually completes inside the test.
    let state_for_task = state.clone();
    tokio::spawn(async move {
        while let Some(req) = reload_rx.recv().await {
            let outcome = agent_shim_gateway::commands::serve::handle_reload(
                &state_for_task,
                req.source,
            )
            .await;
            let _ = req.respond_to.send(outcome);
        }
    });

    // Force two failures into the breaker registry to trip it.
    // 2 failures over 2 samples = 100% > 50% threshold, samples >=
    // min_requests (2) → Closed → Open.
    let policy = BreakerPolicy {
        enabled: true,
        failure_threshold_pct: 50,
        min_requests: 2,
        window: std::time::Duration::from_secs(60),
        open_cooldown: std::time::Duration::from_secs(60),
    };
    state.core.breaker_registry.record("m", "x", false, &policy);
    state.core.breaker_registry.record("m", "x", false, &policy);

    // Confirm Open: the registry-level API is `decision(...)` (the inline
    // doc comment in circuit_breaker.rs uses the older name "query_state",
    // but the actual public method is `decision`).
    let decision = state.core.breaker_registry.decision("m", "x", &policy);
    assert!(
        matches!(decision, agent_shim_router::BreakerDecision::Skip),
        "breaker must be Skip before reload; got {decision:?}"
    );

    // Trigger a reload (same YAML — must validate cleanly).
    let (tx, rx) = tokio::sync::oneshot::channel();
    state
        .core
        .reload_tx
        .send(agent_shim_gateway::reload_trigger::ReloadRequest {
            source: agent_shim_gateway::reload_trigger::ReloadSource::Yaml(yaml.clone()),
            respond_to: tx,
        })
        .await
        .unwrap();
    let outcome = rx.await.unwrap();
    assert!(
        matches!(outcome, agent_shim_gateway::reload_trigger::ReloadOutcome::Ok(_)),
        "reload must succeed for same-yaml input",
    );

    // After reload, breaker MUST still be Open. The registry is on AppCore
    // and only AppSnapshot was swapped, so any per-(provider, model) state
    // accumulated before the swap is intact.
    let decision = state.core.breaker_registry.decision("m", "x", &policy);
    assert!(
        matches!(decision, agent_shim_router::BreakerDecision::Skip),
        "breaker state must survive reload; got {decision:?}"
    );
}

async fn pick_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
