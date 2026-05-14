use crate::handlers;
use crate::state::AppState;
use agent_shim_observability::RequestIdLayer;
use anyhow::Result;
use axum::{
    routing::{get, post},
    Router,
};
use tower_http::trace::TraceLayer;

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
        .layer(crate::metrics_layer::MetricsLayer)
        .layer(RequestIdLayer)
        // Plan 03 P03 T4: inbound `traceparent` extraction is applied
        // inside the pipeline `dispatch` (where the root `gateway.request`
        // span is owned), NOT as a tower layer here. A layer can't reach
        // the per-request span — `Span::current()` is `Span::none()` at
        // the layer call site — so the parent context never propagates.
        // The pipeline calls `extract_context_from_headers` directly and
        // calls `set_parent` on its own root span.
        .with_state(state)
}

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

/// Phase 1 P01: dual-listener variant accepting pre-bound listeners and a
/// shared shutdown future. Mirrors `run_with_admin` but lets `run_core`
/// invoke `on_listening` immediately after the public listener is bound.
pub async fn run_with_admin_on_listeners(
    public_listener: tokio::net::TcpListener,
    admin_listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let public_app = build_router(state.clone());
    let admin_app = crate::admin::build_router(state);

    let token = tokio_util::sync::CancellationToken::new();
    {
        let token = token.clone();
        let signal_task: tokio::task::JoinHandle<()> = tokio::spawn(async move {
            shutdown.await;
            token.cancel();
        });
        drop(signal_task);
    }

    let public_shutdown = {
        let token = token.clone();
        async move { token.cancelled().await }
    };
    let admin_shutdown = {
        let token = token.clone();
        async move { token.cancelled().await }
    };

    tokio::select! {
        res = axum::serve(public_listener, public_app)
            .with_graceful_shutdown(public_shutdown) => res?,
        res = axum::serve(admin_listener, admin_app)
            .with_graceful_shutdown(admin_shutdown) => res?,
    }
    Ok(())
}
