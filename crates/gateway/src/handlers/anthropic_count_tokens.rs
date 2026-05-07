//! `/v1/messages/count_tokens` — local-only token approximation.
//!
//! Decodes an Anthropic-shaped count_tokens body, runs the canonical request
//! through `count_tokens::count`, and returns `{"input_tokens": N}`. Never
//! contacts an upstream provider. See
//! `docs/superpowers/specs/2026-05-06-count-tokens-design.md`.

use axum::{response::IntoResponse, response::Response, Json};
use bytes::Bytes;
use serde::Serialize;

use agent_shim_frontends::anthropic_messages::{
    count_tokens, count_tokens_wire::CountTokensRequest, decode,
};
use agent_shim_frontends::FrontendError;

use super::HandlerError;

#[derive(Debug, Serialize)]
struct CountTokensResponse {
    input_tokens: u32,
}

pub async fn handle(body: Bytes) -> Result<Response, HandlerError> {
    let started = std::time::Instant::now();
    let body_bytes = body.len();

    let ct_req: CountTokensRequest = serde_json::from_slice(&body)
        .map_err(|e| HandlerError::Frontend(FrontendError::InvalidBody(e.to_string())))?;
    let model_alias = ct_req.model.clone();

    // Reuse the standard Anthropic decoder for validation (role checks, block
    // shape, tool_choice forms). `into_messages_request` injects max_tokens=0
    // since count_tokens never reads it.
    let canonical =
        decode::decode_request(ct_req.into_messages_request()).map_err(HandlerError::Frontend)?;
    let n = count_tokens::count(&canonical);

    tracing::info!(
        "→ /v1/messages/count_tokens | model: {} | tokens: {} | bodyBytes: {} | {:.3}s",
        model_alias,
        n,
        body_bytes,
        started.elapsed().as_secs_f64()
    );

    Ok(Json(CountTokensResponse { input_tokens: n }).into_response())
}
