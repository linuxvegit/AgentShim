use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;

use agent_shim_core::{BackendTarget, FrontendKind};
use agent_shim_frontends::FrontendProtocol;
use agent_shim_router::Router;

use crate::state::AppState;

use super::{collect_stream, frontend_response_to_axum, HandlerError};

fn extract_model(body: &[u8]) -> Result<String, super::HandlerError> {
    #[derive(serde::Deserialize)]
    struct Minimal {
        model: String,
    }
    let m: Minimal = serde_json::from_slice(body).map_err(|e| {
        HandlerError::Frontend(agent_shim_frontends::FrontendError::InvalidBody(format!(
            "cannot extract model: {e}"
        )))
    })?;
    Ok(m.model)
}

pub async fn handle(
    State(state): State<AppState>,
    _headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    let body_bytes = body.len();
    let started = std::time::Instant::now();

    let model_alias = extract_model(&body)?;

    let target = state
        .router
        .resolve(FrontendKind::OpenAiResponses, &model_alias)
        .map_err(|e| {
            tracing::warn!(model = %model_alias, error = %e, "no route");
            HandlerError::Route(e)
        })?;

    let mut target = target;
    if let Some(resolved) = state.model_index.resolve(&target.provider, &target.model) {
        if resolved != target.model {
            tracing::info!(
                requested = %target.model,
                resolved = %resolved,
                provider = %target.provider,
                "fuzzy model match"
            );
            target = BackendTarget {
                model: resolved.to_string(),
                ..target
            };
        }
    }

    let upstream_model = target.model.clone();

    // The Responses handler tries raw passthrough before decoding, so we only
    // know the route-level default here. Per-request `reasoning.effort` from
    // the body shows up in the upstream payload regardless.
    let reasoning_effort = target.default_reasoning_effort;

    tracing::info!(
        "→ /v1/responses | model: {} → {} | bodyBytes: {} | reasoning_default: {}",
        model_alias,
        upstream_model,
        body_bytes,
        reasoning_effort.map(|e| e.as_str()).unwrap_or("none"),
    );

    let provider = state.providers.get(&target.provider).ok_or_else(|| {
        tracing::error!(provider = %target.provider, "provider not registered");
        HandlerError::Provider(agent_shim_providers::ProviderError::UnknownProvider(
            target.provider.clone(),
        ))
    })?;

    // Try raw passthrough first — avoids parse/re-encode round-trip
    if let Some((content_type, byte_stream)) = provider
        .proxy_raw(body.clone(), target.clone())
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "proxy_raw failed");
            HandlerError::Provider(e)
        })?
    {
        tracing::info!(
            "← /v1/responses (passthrough) | model: {} → {} | {:.1}s",
            model_alias,
            upstream_model,
            started.elapsed().as_secs_f64()
        );
        let body = Body::from_stream(byte_stream.map(|r| r.map_err(|e| e.to_string())));
        let mut r = Response::new(body);
        r.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_str(&content_type)
                .unwrap_or_else(|_| HeaderValue::from_static("text/event-stream")),
        );
        return Ok(r);
    }

    // Fallback: full decode → canonical → encode
    let canonical = state
        .openai_responses
        .decode_request(&body)
        .map_err(HandlerError::Frontend)?;

    let is_stream = canonical.stream;

    if is_stream {
        let upstream_stream =
            super::anthropic_messages::spawn_provider_stream(provider.clone(), canonical, target);
        let frontend_response = state.openai_responses.encode_stream(upstream_stream);
        tracing::info!(
            "← /v1/responses (stream) | model: {} → {} | {:.1}s",
            model_alias,
            upstream_model,
            started.elapsed().as_secs_f64()
        );
        Ok(frontend_response_to_axum(frontend_response))
    } else {
        let stream = provider.complete(canonical, target).await.map_err(|e| {
            tracing::error!(error = %e, "provider.complete failed");
            HandlerError::Provider(e)
        })?;
        let response = collect_stream(stream).await?;
        let (input, output) = match &response.usage {
            Some(u) => (u.input_tokens.unwrap_or(0), u.output_tokens.unwrap_or(0)),
            None => (0, 0),
        };
        tracing::info!(
            "← /v1/responses (unary) | model: {} → {} | input: {} | output: {} | {:.1}s",
            model_alias,
            upstream_model,
            input,
            output,
            started.elapsed().as_secs_f64()
        );
        let frontend_response = state
            .openai_responses
            .encode_unary(response)
            .map_err(HandlerError::Frontend)?;
        Ok(frontend_response_to_axum(frontend_response))
    }
}
