//! Admin HTTP listener — Plan 01 P01 T3.
//!
//! Hosts /healthz, /readyz, and (in later plans) /metrics, /admin/reload.
//! Bound to a separate listener from the public request path so operators
//! can firewall it independently.

use axum::{routing::get, Router};

use crate::state::AppState;

mod handlers;

/// Build the admin Router.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handlers::healthz))
        .route("/readyz", get(handlers::readyz))
        .with_state(state)
}

/// Serve the admin router on `listener` until `shutdown` resolves.
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
