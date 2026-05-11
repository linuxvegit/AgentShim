//! Admin HTTP listener — Plan 01 P01 T3.
//!
//! Hosts /healthz, /readyz, /metrics, and (in later plans) /admin/reload.
//! Bound to a separate listener from the public request path so operators
//! can firewall it independently.

use axum::{routing::get, Router};

use crate::state::AppState;

mod handlers;
mod metrics_handler;

/// Build the admin Router.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handlers::healthz))
        .route("/readyz", get(handlers::readyz))
        .route("/metrics", get(metrics_handler::metrics))
        .with_state(state)
}

/// Serve the admin router on `listener` until `shutdown` resolves.
// Plan 01 P01 T3: helper for serving the admin router on a pre-bound
// listener. T4 chose to inline the admin axum::serve call into
// `server::run_with_admin` so both listeners share one shutdown
// notify; this helper remains as a one-listener fallback for future
// use cases (e.g. admin-only test harnesses). Remove if no callsite
// materializes by P05.
#[allow(dead_code)]
pub async fn run(
    listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let app = build_router(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}
