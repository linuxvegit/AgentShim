use anyhow::Result;
use axum::{routing::{get, post}, Router};
use std::net::SocketAddr;
use tower_http::trace::TraceLayer;
use tracing::info;
use agent_shim_observability::RequestIdLayer;
use crate::handlers;
use crate::state::AppState;
use crate::shutdown::shutdown_signal;

async fn healthz() -> &'static str {
    "ok"
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/messages", post(handlers::anthropic_messages::handle))
        .route("/v1/chat/completions", post(handlers::openai_chat::handle))
        .layer(TraceLayer::new_for_http())
        .layer(RequestIdLayer)
        .with_state(state)
}

/// Start the server, binding to the address in the config.
pub async fn run(state: AppState) -> Result<()> {
    let bind: SocketAddr = format!(
        "{}:{}",
        state.config.server.bind,
        state.config.server.port
    )
    .parse()?;

    let app = build_router(state);
    info!("Listening on {}", bind);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Start the server on an already-bound listener (useful for tests with port 0).
pub async fn run_on_listener(
    listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let app = build_router(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}
