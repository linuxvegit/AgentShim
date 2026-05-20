//! Admin endpoint handlers — Plan 01 P01 T3.

use axum::{extract::State, http::StatusCode, response::IntoResponse};

use crate::state::AppState;

/// Liveness — process is up.
pub async fn healthz() -> &'static str {
    "ok"
}

/// Readiness — providers initialized AND at least one route loaded.
///
/// Returns 200 + "ready" when both predicates hold; otherwise 503 +
/// "not ready". A gateway booted with providers but zero routes
/// reports not-ready because it cannot serve traffic.
///
/// Today (P01) this is the full readiness check. Future plans may
/// extend the predicate (e.g. P02 could add a metrics-recorder probe
/// once the recorder install is fallible; P03 could add an OTel
/// exporter probe). Those extensions are speculative — drop the
/// "future" sentence if they don't materialize.
pub async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    let snap = state.snapshot.load_full();
    // ProviderRegistry has no is_empty(); peek the iterator instead. The
    // frozen-core invariant (Phase 5 design §0) forbids touching the
    // providers crate to add one.
    let providers_ready = state.core.providers.iter().next().is_some();
    let routes_loaded = !snap.config.routes.is_empty();
    if providers_ready && routes_loaded {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    fn yaml_with_providers_no_routes() -> &'static str {
        r#"
server: {bind: 127.0.0.1, port: 8787}
upstreams:
  noop:
    type: open_ai_compatible
    base_url: http://localhost:9999/v1
    api_key: dummy
    tier: standard
"#
    }

    fn yaml_with_providers_and_routes() -> &'static str {
        r#"
server: {bind: 127.0.0.1, port: 8787}
upstreams:
  noop:
    type: open_ai_compatible
    base_url: http://localhost:9999/v1
    api_key: dummy
    tier: standard
routes:
  - frontend: openai_chat
    model: x
    upstream: noop
    upstream_model: x
"#
    }

    #[tokio::test]
    async fn readyz_not_ready_without_routes() {
        let cfg: agent_shim_config::GatewayConfig =
            serde_yaml::from_str(yaml_with_providers_no_routes()).unwrap();
        let (state, _reload_rx) = crate::state::AppState::new(cfg).await.expect("test AppState build");
        let response = readyz(axum::extract::State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn readyz_ready_with_routes_and_providers() {
        let cfg: agent_shim_config::GatewayConfig =
            serde_yaml::from_str(yaml_with_providers_and_routes()).unwrap();
        let (state, _reload_rx) = crate::state::AppState::new(cfg).await.expect("test AppState build");
        let response = readyz(axum::extract::State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
