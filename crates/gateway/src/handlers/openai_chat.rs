use axum::{extract::State, http::HeaderMap, response::Response};
use bytes::Bytes;

use agent_shim_core::{BackendTarget, FrontendKind};
use agent_shim_frontends::FrontendProtocol;
use agent_shim_router::Router;

use crate::state::AppState;

use super::{collect_stream, frontend_response_to_axum, HandlerError};

pub async fn handle(
    State(state): State<AppState>,
    _headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    let body_bytes = body.len();
    let started = std::time::Instant::now();

    let canonical = state
        .openai
        .decode_request(&body)
        .map_err(HandlerError::Frontend)?;

    let model_alias = canonical.model.as_str().to_string();
    let is_stream = canonical.stream;
    let max_tokens = canonical.generation.max_tokens;

    let target = state
        .router
        .resolve(FrontendKind::OpenAiChat, &model_alias)
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
                provider: target.provider,
                model: resolved.to_string(),
            };
        }
    }

    let upstream_model = target.model.clone();

    tracing::info!(
        "→ /v1/chat/completions | model: {} → {} | bodyBytes: {} | maxTokens: {} | stream: {}",
        model_alias,
        upstream_model,
        body_bytes,
        max_tokens.unwrap_or(0),
        is_stream
    );

    let provider = state.providers.get(&target.provider).ok_or_else(|| {
        tracing::error!(provider = %target.provider, "provider not registered");
        HandlerError::Provider(agent_shim_providers::ProviderError::UnknownProvider(
            target.provider.clone(),
        ))
    })?;

    if is_stream {
        let upstream_stream =
            super::anthropic_messages::spawn_provider_stream(provider.clone(), canonical, target);
        let frontend_response = state.openai.encode_stream(upstream_stream);
        tracing::info!(
            "← /v1/chat/completions (stream) | model: {} → {} | {:.1}s",
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
            "← /v1/chat/completions (unary) | model: {} → {} | input: {} | output: {} | {:.1}s",
            model_alias,
            upstream_model,
            input,
            output,
            started.elapsed().as_secs_f64()
        );
        let frontend_response = state
            .openai
            .encode_unary(response)
            .map_err(HandlerError::Frontend)?;
        Ok(frontend_response_to_axum(frontend_response))
    }
}
