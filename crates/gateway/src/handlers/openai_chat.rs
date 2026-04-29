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
    let canonical = state
        .openai
        .decode_request(&body)
        .map_err(HandlerError::Frontend)?;

    let model_alias = canonical.model.as_str().to_string();

    let target = state
        .router
        .resolve(FrontendKind::OpenAiChat, &model_alias)
        .map_err(HandlerError::Route)?;

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

    let provider = state.providers.get(&target.provider).ok_or_else(|| {
        HandlerError::Provider(agent_shim_providers::ProviderError::UnknownProvider(
            target.provider.clone(),
        ))
    })?;

    let is_stream = canonical.stream;

    if is_stream {
        let upstream_stream =
            super::anthropic_messages::spawn_provider_stream(provider.clone(), canonical, target);
        let frontend_response = state.openai.encode_stream(upstream_stream);
        Ok(frontend_response_to_axum(frontend_response))
    } else {
        let stream = provider
            .complete(canonical, target)
            .await
            .map_err(HandlerError::Provider)?;
        let response = collect_stream(stream).await?;
        let frontend_response = state
            .openai
            .encode_unary(response)
            .map_err(HandlerError::Frontend)?;
        Ok(frontend_response_to_axum(frontend_response))
    }
}
