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

    // Validate the count_tokens-specific shape and pull the model alias for logging.
    let ct_req: CountTokensRequest = serde_json::from_slice(&body)
        .map_err(|e| HandlerError::Frontend(FrontendError::InvalidBody(e.to_string())))?;
    let model_alias = ct_req.model.clone();

    // Re-frame as a MessagesRequest-shaped body by patching `max_tokens=0` if
    // missing, then run the standard decoder. This shares all of decode's
    // validation (role checks, block shape, tool_choice forms) with /v1/messages.
    let mut value: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| HandlerError::Frontend(FrontendError::InvalidBody(e.to_string())))?;
    if let serde_json::Value::Object(map) = &mut value {
        map.entry("max_tokens")
            .or_insert(serde_json::Value::Number(0.into()));
    }
    let normalized = serde_json::to_vec(&value)
        .map_err(|e| HandlerError::Frontend(FrontendError::InvalidBody(e.to_string())))?;

    let canonical = decode::decode(&normalized).map_err(HandlerError::Frontend)?;
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
