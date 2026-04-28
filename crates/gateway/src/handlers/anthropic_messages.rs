use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use parking_lot::Mutex;

use agent_shim_core::{
    CanonicalResponse, ContentBlock, FrontendKind, ResponseId, StopReason,
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
    let body_bytes = body.len();
    let started = std::time::Instant::now();

    let canonical = state
        .anthropic
        .decode_request(&body)
        .map_err(|e| { tracing::warn!(error = %e, "anthropic decode failed"); HandlerError::Frontend(e) })?;

    let model_alias = canonical.model.as_str().to_string();
    let is_stream = canonical.stream;
    let max_tokens = canonical.generation.max_tokens;

    let target = state
        .router
        .resolve(FrontendKind::AnthropicMessages, &model_alias)
        .map_err(|e| { tracing::warn!(model = %model_alias, error = %e, "no route"); HandlerError::Route(e) })?;

    let upstream_model = target.model.clone();

    tracing::info!(
        "→ /v1/messages | model: {} → {} | bodyBytes: {} | maxTokens: {}",
        model_alias, upstream_model, body_bytes, max_tokens.unwrap_or(0)
    );

    let provider = state
        .providers
        .get(&target.provider)
        .ok_or_else(|| {
            tracing::error!(provider = %target.provider, "provider not registered");
            HandlerError::Provider(agent_shim_providers::ProviderError::UnknownProvider(
                target.provider.clone(),
            ))
        })?;

    let stream = provider
        .complete(canonical, target)
        .await
        .map_err(|e| { tracing::error!(error = %e, "provider.complete failed"); HandlerError::Provider(e) })?;

    if is_stream {
        let usage_capture: Arc<Mutex<Option<Usage>>> = Arc::new(Mutex::new(None));
        let usage_for_log = usage_capture.clone();
        let model_alias_log = model_alias.clone();
        let upstream_model_log = upstream_model.clone();

        let logging_stream = stream.map(move |event| {
            if let Ok(ref ev) = event {
                match ev {
                    StreamEvent::UsageDelta { usage } => {
                        *usage_capture.lock() = Some(usage.clone());
                    }
                    StreamEvent::ResponseStop { usage: Some(u) } => {
                        *usage_capture.lock() = Some(u.clone());
                    }
                    _ => {}
                }
            }
            event
        });

        let canonical_stream: agent_shim_core::CanonicalStream = Box::pin(logging_stream);
        let frontend_response = state.anthropic.encode_stream(canonical_stream);

        match frontend_response {
            FrontendResponse::Stream { content_type, stream: sse_stream } => {
                let logged_stream = sse_stream.chain(futures::stream::once(async move {
                    let elapsed = started.elapsed();
                    let u = usage_for_log.lock().clone();
                    let (input, output) = match u {
                        Some(ref usage) => (
                            usage.input_tokens.unwrap_or(0),
                            usage.output_tokens.unwrap_or(0),
                        ),
                        None => (0, 0),
                    };
                    tracing::info!(
                        "← /v1/messages (stream) | model: {} → {} | input: {} | output: {} | {:.1}s",
                        model_alias_log, upstream_model_log, input, output,
                        elapsed.as_secs_f64()
                    );
                    // Yield nothing — this is just for the side-effect log
                    Err(agent_shim_frontends::FrontendError::Encode("__log_sentinel__".into()))
                }).filter(|_| futures::future::ready(false)));

                let body = Body::from_stream(logged_stream.map(|r| r.map_err(|e| e.to_string())));
                let mut r = Response::new(body);
                r.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    HeaderValue::from_str(&content_type).unwrap_or_else(|_| {
                        HeaderValue::from_static("text/event-stream")
                    }),
                );
                Ok(r)
            }
            _ => unreachable!(),
        }
    } else {
        let response = collect_stream(stream).await?;
        let (input, output) = match &response.usage {
            Some(u) => (u.input_tokens.unwrap_or(0), u.output_tokens.unwrap_or(0)),
            None => (0, 0),
        };
        let elapsed = started.elapsed();
        tracing::info!(
            "← /v1/messages (unary) | model: {} → {} | input: {} | output: {} | {:.1}s",
            model_alias, upstream_model, input, output, elapsed.as_secs_f64()
        );
        let frontend_response = state
            .anthropic
            .encode_unary(response)
            .map_err(HandlerError::Frontend)?;
        match frontend_response {
            FrontendResponse::Unary { content_type, body } => {
                let mut r = Response::new(Body::from(body));
                r.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    HeaderValue::from_str(&content_type).unwrap_or_else(|_| {
                        HeaderValue::from_static("application/json")
                    }),
                );
                Ok(r)
            }
            _ => unreachable!(),
        }
    }
}

pub(crate) async fn collect_stream(
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
