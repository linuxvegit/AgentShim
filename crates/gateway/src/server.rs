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
        .layer(crate::metrics_layer::MetricsLayer)
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
    info!("Listening on {} (public)", bind);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Run BOTH listeners (public + admin) concurrently with shared graceful
/// shutdown. Used when `state.core.admin_config` is `Some`.
pub async fn run_with_admin(state: AppState) -> Result<()> {
    let public_bind: SocketAddr = format!(
        "{}:{}",
        state.core.server_config.bind, state.core.server_config.port
    )
    .parse()?;
    let admin_cfg = state
        .core
        .admin_config
        .clone()
        .expect("run_with_admin called with admin_config = None");
    let admin_bind: SocketAddr = format!("{}:{}", admin_cfg.bind, admin_cfg.port).parse()?;

    let public_listener = tokio::net::TcpListener::bind(public_bind).await?;
    info!("Listening on {} (public)", public_bind);
    let admin_listener = tokio::net::TcpListener::bind(admin_bind).await?;
    info!("Listening on {} (admin)", admin_bind);

    let public_app = build_router(state.clone());
    let admin_app = crate::admin::build_router(state);

    // Plan 01 P01 T4 followup: CancellationToken caches cancellation,
    // so a SIGTERM arriving during boot (before either listener's
    // graceful_shutdown future has registered with the notify) still
    // shuts down both listeners. The previous Notify::notify_waiters
    // shape had a race window where this could hang.
    let token = tokio_util::sync::CancellationToken::new();
    {
        let token = token.clone();
        let signal_task: tokio::task::JoinHandle<()> = tokio::spawn(async move {
            shutdown_signal().await;
            token.cancel();
        });
        // The signal task is detached deliberately — it terminates when
        // shutdown_signal() resolves, which must happen for the process
        // to exit. A future enhancement could abort it on early return
        // from select! below; for now the runtime drop on main exit
        // collects it. We `drop` the JoinHandle (rather than `let _ =`)
        // because clippy's let_underscore_future would otherwise flag
        // it as accidentally discarding a future.
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
