use anyhow::Result;
use axum::{routing::get, Router};
use std::net::SocketAddr;
use tower_http::trace::TraceLayer;
use tracing::info;
use agent_shim_observability::RequestIdLayer;
use crate::state::AppState;
use crate::shutdown::shutdown_signal;

async fn healthz() -> &'static str {
    "ok"
}

pub async fn run(state: AppState) -> Result<()> {
    let bind: SocketAddr = format!(
        "{}:{}",
        state.config.server.bind,
        state.config.server.port
    )
    .parse()?;

    let app = Router::new()
        .route("/healthz", get(healthz))
        .layer(TraceLayer::new_for_http())
        .layer(RequestIdLayer)
        .with_state(state);

    info!("Listening on {}", bind);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}
