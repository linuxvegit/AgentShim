pub mod anthropic_messages;
pub mod openai_chat;
pub mod openai_responses;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use thiserror::Error;

use agent_shim_core::{
    CanonicalResponse, ContentBlock, FrontendKind, ResponseId, StopReason, StreamEvent,
    ToolCallArguments, ToolCallBlock, ToolCallId, Usage,
};
use agent_shim_frontends::{FrontendError, FrontendResponse};
use agent_shim_providers::ProviderError;
use agent_shim_router::RouteError;

#[derive(Debug, Error)]
pub enum HandlerError {
    #[error("route error: {0}")]
    Route(#[from] RouteError),
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("frontend error: {0}")]
    Frontend(#[from] FrontendError),
    /// Surfaced when the gateway capability gate rejects a request because the
    /// target provider can't service a feature the inbound asked for (e.g.
    /// image blocks routed to a text-only backend). Carrying the frontend
    /// kind here lets `IntoResponse` shape the JSON error envelope to match
    /// what the client SDK expects (Anthropic-style vs OpenAI-style),
    /// instead of returning the generic `{"error":{"message":...}}` body
    /// other variants use.
    #[error("capability mismatch: {message}")]
    CapabilityMismatch { kind: FrontendKind, message: String },
}

impl IntoResponse for HandlerError {
    fn into_response(self) -> Response {
        // CapabilityMismatch is the only variant that needs frontend-shaped
        // body formatting today. Pull it out first so the rest of the match
        // can stay as a flat status-code lookup.
        if let HandlerError::CapabilityMismatch { kind, message } = &self {
            let body = match kind {
                FrontendKind::AnthropicMessages => serde_json::json!({
                    "type": "error",
                    "error": {
                        "type": "invalid_request_error",
                        "message": message,
                    },
                }),
                FrontendKind::OpenAiChat | FrontendKind::OpenAiResponses => serde_json::json!({
                    "error": {
                        "message": message,
                        "type": "invalid_request_error",
                        "code": "capability_mismatch",
                    },
                }),
            };
            return (StatusCode::BAD_REQUEST, axum::Json(body)).into_response();
        }

        let status = match &self {
            HandlerError::Route(RouteError::NoRoute { .. }) => StatusCode::NOT_FOUND,
            HandlerError::Provider(ProviderError::Upstream { status, .. }) => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            HandlerError::Provider(ProviderError::UnknownProvider(_)) => StatusCode::BAD_GATEWAY,
            HandlerError::Provider(ProviderError::Network(_)) => StatusCode::BAD_GATEWAY,
            HandlerError::Provider(ProviderError::CapabilityMismatch(_)) => {
                // Reachable only if a provider raises CapabilityMismatch
                // mid-request (the gateway gate catches the
                // image-vs-vision case earlier and routes through the
                // dedicated `HandlerError::CapabilityMismatch` arm).
                StatusCode::BAD_REQUEST
            }
            HandlerError::Frontend(FrontendError::InvalidBody(_)) => StatusCode::BAD_REQUEST,
            HandlerError::Frontend(FrontendError::Unsupported(_)) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            HandlerError::CapabilityMismatch { .. } => unreachable!("handled above"),
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = serde_json::json!({ "error": { "message": self.to_string() } });
        (status, axum::Json(body)).into_response()
    }
}

pub(crate) fn frontend_response_to_axum(resp: FrontendResponse) -> Response {
    match resp {
        FrontendResponse::Unary { content_type, body } => {
            let mut r = Response::new(Body::from(body));
            r.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_str(&content_type)
                    .unwrap_or_else(|_| HeaderValue::from_static("application/json")),
            );
            r
        }
        FrontendResponse::Stream {
            content_type,
            stream,
        } => {
            let body = Body::from_stream(stream.map(|r| r.map_err(|e| e.to_string())));
            let mut r = Response::new(body);
            r.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_str(&content_type)
                    .unwrap_or_else(|_| HeaderValue::from_static("text/event-stream")),
            );
            r
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
    let mut tool_args: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    let mut text_buf: std::collections::HashMap<u32, String> = std::collections::HashMap::new();

    while let Some(ev) = stream.next().await {
        let ev = ev.map_err(|e| {
            HandlerError::Provider(agent_shim_providers::ProviderError::Decode(e.to_string()))
        })?;
        match ev {
            StreamEvent::ResponseStart {
                id: rid, model: m, ..
            } => {
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
            StreamEvent::ToolCallStart {
                index,
                id: tc_id,
                name,
            } => {
                tool_names.insert(index, (tc_id, name));
            }
            StreamEvent::ToolCallArgumentsDelta {
                index,
                json_fragment,
            } => {
                tool_args.entry(index).or_default().push_str(&json_fragment);
            }
            StreamEvent::MessageStop {
                stop_reason: sr,
                stop_sequence: ss,
            } => {
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
