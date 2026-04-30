//! Anthropic Messages handler — thin axum binding over the request pipeline.

use axum::{extract::State, http::HeaderMap, response::Response};
use bytes::Bytes;

use crate::pipeline::{dispatch, PipelineSpec};
use crate::state::AppState;

use super::HandlerError;

pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    let spec = PipelineSpec {
        frontend: state.anthropic.as_ref(),
        endpoint_label: "/v1/messages",
        capture_anthropic_headers: true,
        try_proxy_raw: false,
        log_streaming_usage_on_drop: true,
    };
    dispatch(&state, spec, headers, body).await
}
