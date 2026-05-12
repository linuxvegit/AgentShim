//! GET /metrics — render Prometheus text. Plan 02 P02 T3.

use axum::{extract::State, http::StatusCode, response::IntoResponse};

use crate::state::AppState;

pub async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let body = state.core.metrics.render();
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}
