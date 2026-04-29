use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;

use agent_shim_core::{
    BackendTarget, CanonicalResponse, ContentBlock, FrontendKind, ResponseId, StopReason,
    StreamEvent, ToolCallArguments, ToolCallBlock, ToolCallId, Usage,
};
use agent_shim_frontends::{FrontendProtocol, FrontendResponse};
use agent_shim_router::Router;

use crate::state::AppState;

use super::HandlerError;

pub async fn handle(
    State(state): State<AppState>,
    _headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    // Decode request using OpenAI frontend
    let canonical = state
        .openai
        .decode_request(&body)
        .map_err(HandlerError::Frontend)?;

    let model_alias = canonical.model.as_str().to_string();

    // Route
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

    // Get provider
    let provider = state
        .providers
        .get(&target.provider)
        .ok_or_else(|| {
            HandlerError::Provider(agent_shim_providers::ProviderError::UnknownProvider(
                target.provider.clone(),
            ))
        })?;

    let is_stream = canonical.stream;

    // Call backend
    let stream = provider
        .complete(canonical, target)
        .await
        .map_err(HandlerError::Provider)?;

    // Encode response
    if is_stream {
        let frontend_response = state.openai.encode_stream(stream);
        Ok(frontend_response_to_axum(frontend_response))
    } else {
        let response = collect_stream(stream).await?;
        let frontend_response = state
            .openai
            .encode_unary(response)
            .map_err(HandlerError::Frontend)?;
        Ok(frontend_response_to_axum(frontend_response))
    }
}

fn frontend_response_to_axum(resp: FrontendResponse) -> Response {
    match resp {
        FrontendResponse::Unary { content_type, body } => {
            let mut r = Response::new(Body::from(body));
            r.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_str(&content_type).unwrap_or_else(|_| {
                    HeaderValue::from_static("application/json")
                }),
            );
            r
        }
        FrontendResponse::Stream { content_type, stream } => {
            let body = Body::from_stream(stream.map(|r| r.map_err(|e| e.to_string())));
            let mut r = Response::new(body);
            r.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_str(&content_type).unwrap_or_else(|_| {
                    HeaderValue::from_static("text/event-stream")
                }),
            );
            r
        }
    }
}

async fn collect_stream(
    mut stream: agent_shim_core::CanonicalStream,
) -> Result<CanonicalResponse, HandlerError> {
    let mut id = ResponseId::new();
    let mut model = String::new();
    let mut content: Vec<ContentBlock> = Vec::new();
    let mut stop_reason = StopReason::EndTurn;
    let mut stop_sequence: Option<String> = None;
    let mut usage: Option<Usage> = None;

    let mut tool_names: std::collections::HashMap<u32, (ToolCallId, String)> =
        std::collections::HashMap::new();
    let mut tool_args: std::collections::HashMap<u32, String> =
        std::collections::HashMap::new();
    let mut text_buf: std::collections::HashMap<u32, String> =
        std::collections::HashMap::new();

    while let Some(ev) = stream.next().await {
        let ev = ev.map_err(|e| HandlerError::Provider(agent_shim_providers::ProviderError::Decode(e.to_string())))?;
        match ev {
            StreamEvent::ResponseStart { id: rid, model: m, .. } => {
                id = rid;
                model = m;
            }
            StreamEvent::TextDelta { index, text } => {
                text_buf.entry(index).or_default().push_str(&text);
            }
            StreamEvent::ContentBlockStop { index } => {
                if let Some(text) = text_buf.remove(&index) {
                    content.push(ContentBlock::text(text));
                }
                if let Some((tc_id, name)) = tool_names.remove(&index) {
                    let args_str = tool_args.remove(&index).unwrap_or_default();
                    let args_val: serde_json::Value =
                        serde_json::from_str(&args_str).unwrap_or(serde_json::Value::Null);
                    content.push(ContentBlock::ToolCall(ToolCallBlock {
                        id: tc_id,
                        name,
                        arguments: ToolCallArguments::Complete { value: args_val },
                        extensions: Default::default(),
                    }));
                }
            }
            StreamEvent::ToolCallStart { index, id: tc_id, name } => {
                tool_names.insert(index, (tc_id, name));
            }
            StreamEvent::ToolCallArgumentsDelta { index, json_fragment } => {
                tool_args.entry(index).or_default().push_str(&json_fragment);
            }
            StreamEvent::MessageStop { stop_reason: sr, stop_sequence: ss } => {
                stop_reason = sr;
                stop_sequence = ss;
            }
            StreamEvent::UsageDelta { usage: u } | StreamEvent::ResponseStop { usage: Some(u) } => {
                usage = Some(u);
            }
            StreamEvent::Error { message } => {
                return Err(HandlerError::Provider(
                    agent_shim_providers::ProviderError::Upstream {
                        status: 200,
                        body: message,
                    },
                ));
            }
            _ => {}
        }
    }

    Ok(CanonicalResponse {
        id,
        model,
        content,
        stop_reason,
        stop_sequence,
        usage,
    })
}
