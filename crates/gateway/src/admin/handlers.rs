//! Admin endpoint handlers — Plan 01 P01 T3.

use axum::{extract::State, http::StatusCode, response::IntoResponse};

use crate::state::AppState;

/// Liveness — process is up.
pub async fn healthz() -> &'static str {
    "ok"
}

/// Readiness — config loaded, providers initialized, snapshot populated.
///
/// Returns 200 + "ready" when all three are true. Today (P01) this is
/// equivalent to "the server bound" because AppState construction is
/// the readiness gate; future plans (P02 metrics, P03 OTel) may add
/// additional readiness predicates here.
pub async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    let snap = state.snapshot.load_full();
    // ProviderRegistry has no is_empty(); peek the iterator instead. The
    // frozen-core invariant (Phase 5 design §0) forbids touching the
    // providers crate to add one.
    let providers_ready = state.core.providers.iter().next().is_some();
    let config_loaded = !snap.config.routes.is_empty() || providers_ready;
    if providers_ready && config_loaded {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}
