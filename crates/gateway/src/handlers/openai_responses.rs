//! OpenAI Responses handler — thin axum binding over the request pipeline.

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
        frontend: state.openai_responses.as_ref(),
        endpoint_label: "/v1/responses",
        capture_anthropic_headers: false,
        try_proxy_raw: true,
        log_streaming_usage_on_drop: false,
    };
    dispatch(&state, spec, headers, body).await
}
