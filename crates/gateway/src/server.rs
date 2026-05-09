use crate::handlers;
use crate::shutdown::shutdown_signal;
use crate::state::AppState;
use agent_shim_observability::RequestIdLayer;
use anyhow::Result;
use axum::{
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use tower_http::trace::TraceLayer;
use tracing::info;

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { "ok" }))
        .route("/v1/messages", post(handlers::anthropic_messages::handle))
        .route(
            "/v1/messages/count_tokens",
            post(handlers::anthropic_count_tokens::handle),
        )
        .route("/v1/chat/completions", post(handlers::openai_chat::handle))
        .route("/v1/responses", post(handlers::openai_responses::handle))
        .layer(TraceLayer::new_for_http())
        .layer(RequestIdLayer)
        .with_state(state)
}

/// Start the server, binding to the address in the config.
pub async fn run(state: AppState) -> Result<()> {
    let bind: SocketAddr = format!(
        "{}:{}",
        state.core.server_config.bind, state.core.server_config.port
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

#[allow(dead_code)]
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
