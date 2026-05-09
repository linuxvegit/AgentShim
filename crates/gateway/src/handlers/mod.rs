pub mod anthropic_count_tokens;
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
use agent_shim_router::{RateLimitDimension, ResilienceError, RouteError};

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
    /// Every chain element was attempted; every retry budget exhausted; the
    /// final error was fallback-eligible (we walked off the end of the
    /// chain). HTTP 503. Maps from
    /// [`ResilienceError::NoUpstreamSucceeded`].
    #[error("no upstream succeeded after trying {tried_count} options: {last_error}")]
    NoUpstreamSucceeded {
        kind: FrontendKind,
        last_error: ProviderError,
        tried_count: usize,
    },
    /// A chain element returned a terminal (non-fallback-eligible) error;
    /// the caller stopped the walk and surfaced it. HTTP status passes
    /// through from the underlying `ProviderError::Upstream{status}`,
    /// otherwise BAD_GATEWAY. Maps from [`ResilienceError::TerminalError`].
    #[error("terminal upstream error: {error}")]
    TerminalUpstream {
        kind: FrontendKind,
        error: ProviderError,
    },
    /// All chain candidates have open circuit breakers. HTTP 503. Plan 03
    /// produces this; declared in P02 so the envelope mapping is stable
    /// across plans. Maps from [`ResilienceError::AllBreakersOpen`].
    #[error("all {tried_count} upstreams temporarily unavailable (circuit breakers open)")]
    AllBreakersOpen {
        kind: FrontendKind,
        tried_count: usize,
    },
    /// A rate-limit bucket rejected the request. HTTP 429 with
    /// `Retry-After`. Plan 04 produces this; declared in P02 so the
    /// envelope mapping is stable across plans. Maps from
    /// [`ResilienceError::RateLimited`].
    #[error("{dimension:?} rate limit exceeded; retry after {retry_after_secs}s")]
    RateLimited {
        kind: FrontendKind,
        dimension: RateLimitDimension,
        retry_after_secs: u32,
    },
    /// `auth.required = true` and the inbound request had either no key
    /// at all or a key whose hash is not in `auth.keys`. HTTP 401. Plan
    /// 04 P04 T4 produces this; the `IntoResponse` impl shapes a
    /// dialect-correct envelope. We do NOT carry the (hashed or
    /// plaintext) presented key here — operator logs see only the
    /// failure mode, not the presented value.
    #[error("authentication required")]
    Unauthorized { kind: FrontendKind },
}

impl HandlerError {
    /// Bridge from the resilience layer's outcome enum to a HandlerError.
    ///
    /// Takes the inbound `kind` separately because the resilience layer is
    /// frontend-agnostic — it doesn't know which dialect the client is
    /// speaking. The pipeline (P02 T6) supplies it from the request context
    /// so `IntoResponse` can emit the correct error envelope.
    pub fn from_resilience_error(e: ResilienceError, kind: FrontendKind) -> Self {
        match e {
            ResilienceError::NoUpstreamSucceeded { tried, last_error } => {
                HandlerError::NoUpstreamSucceeded {
                    kind,
                    last_error,
                    tried_count: tried.len(),
                }
            }
            ResilienceError::TerminalError { error, .. } => {
                HandlerError::TerminalUpstream { kind, error }
            }
            ResilienceError::AllBreakersOpen { tried } => HandlerError::AllBreakersOpen {
                kind,
                tried_count: tried.len(),
            },
            ResilienceError::RateLimited {
                dimension,
                retry_after_secs,
            } => HandlerError::RateLimited {
                kind,
                dimension,
                retry_after_secs,
            },
        }
    }
}

/// Build the OpenAI-shaped error envelope body for resilience variants.
///
/// OpenAI's error schema is `{"error": {"message", "type", "code"}}`. We
/// stamp `type` with the high-level HTTP error class
/// (`service_unavailable_error` for 503, `rate_limit_error` for 429) and
/// `code` with a stable machine-readable string identifying the specific
/// resilience outcome (`no_upstream_available`, `all_breakers_open`,
/// `rate_limited_<dimension>`). Operators key dashboards off `code`; SDK
/// users key catch blocks off `type`.
fn openai_envelope_body(handler_error: &HandlerError) -> serde_json::Value {
    use serde_json::json;
    match handler_error {
        HandlerError::NoUpstreamSucceeded {
            last_error,
            tried_count,
            ..
        } => json!({
            "error": {
                "message": format!("All {tried_count} upstreams failed: {last_error}"),
                "type": "service_unavailable_error",
                "code": "no_upstream_available",
            }
        }),
        HandlerError::AllBreakersOpen { tried_count, .. } => json!({
            "error": {
                "message": format!(
                    "All {tried_count} upstreams temporarily unavailable (circuit breakers open)"
                ),
                "type": "service_unavailable_error",
                "code": "all_breakers_open",
            }
        }),
        HandlerError::RateLimited {
            dimension,
            retry_after_secs,
            ..
        } => {
            let (code, dimension_msg) = match dimension {
                RateLimitDimension::PerKey => ("rate_limited_per_key", "Per-API-key"),
                RateLimitDimension::PerRoute => ("rate_limited_per_route", "Route"),
                RateLimitDimension::PerUpstream => ("rate_limited_per_upstream", "Upstream"),
                RateLimitDimension::PerIp => ("rate_limited_per_ip", "Per-IP"),
            };
            json!({
                "error": {
                    "message": format!(
                        "{dimension_msg} rate limit exceeded; retry after {retry_after_secs}s"
                    ),
                    "type": "rate_limit_error",
                    "code": code,
                }
            })
        }
        // TerminalUpstream and non-resilience variants fall through to the
        // generic `{"error":{"message":...}}` body — no special OpenAI
        // shape needed. A single upstream's pass-through error already
        // tells the operator what happened.
        _ => unreachable!(
            "openai_envelope_body called only for NoUpstreamSucceeded / \
             AllBreakersOpen / RateLimited"
        ),
    }
}

/// Build the Anthropic-shaped error envelope body for resilience variants.
///
/// Anthropic's error schema is `{"type": "error", "error": {"type",
/// "message"}}` — note the absence of a `code` field. The inner `type`
/// must be one of the Anthropic-canonical strings; we map service-level
/// availability errors to `overloaded_error` (matching what real
/// Anthropic emits when their fleet is saturated) and rate-limit errors
/// to `rate_limit_error`. The message uses the HandlerError's Display
/// impl so operator-facing detail (chain length, dimension name, retry
/// seconds) is preserved.
fn anthropic_envelope_body(handler_error: &HandlerError) -> serde_json::Value {
    use serde_json::json;
    match handler_error {
        HandlerError::NoUpstreamSucceeded { .. } | HandlerError::AllBreakersOpen { .. } => json!({
            "type": "error",
            "error": {
                "type": "overloaded_error",
                "message": handler_error.to_string(),
            },
        }),
        HandlerError::RateLimited { .. } => json!({
            "type": "error",
            "error": {
                "type": "rate_limit_error",
                "message": handler_error.to_string(),
            },
        }),
        _ => unreachable!(
            "anthropic_envelope_body called only for NoUpstreamSucceeded / \
             AllBreakersOpen / RateLimited"
        ),
    }
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

        // Unauthorized (Plan 04 P04 T4): `auth.required = true` and the
        // inbound request was either unauthenticated or presented a key
        // not in the configured allowlist. HTTP 401 with a dialect-shaped
        // body. We deliberately use a generic message ("Authentication
        // required.") instead of distinguishing missing-vs-unknown so
        // probing clients can't enumerate valid key shapes.
        //
        // The `WWW-Authenticate: Bearer realm="agent-shim"` header is
        // RFC 7235-required for 401: HTTP libraries (curl --anyauth,
        // SDKs that key auth-retry off the header) need it to know the
        // server is challenging for credentials rather than failing
        // for some other reason.
        if let HandlerError::Unauthorized { kind } = &self {
            let body = match kind {
                FrontendKind::AnthropicMessages => serde_json::json!({
                    "type": "error",
                    "error": {
                        "type": "authentication_error",
                        "message": "Authentication required.",
                    },
                }),
                FrontendKind::OpenAiChat | FrontendKind::OpenAiResponses => serde_json::json!({
                    "error": {
                        "message": "Authentication required.",
                        "type": "authentication_error",
                        "code": "unauthorized",
                    },
                }),
            };
            let mut resp = (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
            resp.headers_mut().insert(
                axum::http::header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"agent-shim\""),
            );
            return resp;
        }

        // Resilience variants (P02 T5): they all carry `kind` so the body
        // can be shaped per-dialect. They share the structure of pulling
        // out the kind, picking a status, building the dialect body, and
        // (for RateLimited only) attaching a Retry-After header.
        match &self {
            HandlerError::NoUpstreamSucceeded { kind, .. }
            | HandlerError::AllBreakersOpen { kind, .. }
            | HandlerError::RateLimited { kind, .. } => {
                let status = match &self {
                    HandlerError::NoUpstreamSucceeded { .. }
                    | HandlerError::AllBreakersOpen { .. } => StatusCode::SERVICE_UNAVAILABLE,
                    HandlerError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
                    _ => unreachable!("outer arm guarantees one of the three"),
                };
                let body = match kind {
                    FrontendKind::AnthropicMessages => anthropic_envelope_body(&self),
                    FrontendKind::OpenAiChat | FrontendKind::OpenAiResponses => {
                        openai_envelope_body(&self)
                    }
                };
                let mut response = (status, axum::Json(body)).into_response();
                if let HandlerError::RateLimited {
                    retry_after_secs, ..
                } = &self
                {
                    // `retry_after_secs` is u32 → only ASCII digits, so
                    // HeaderValue::from_str cannot fail. Defensive
                    // unwrap_or keeps the response from panicking even if
                    // a future refactor lets the value escape that range.
                    if let Ok(hv) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                        response.headers_mut().insert("Retry-After", hv);
                    }
                }
                return response;
            }
            _ => {}
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
            // TerminalUpstream passes the underlying Upstream{status}
            // through (typically a 4xx like 401/403). For other inner
            // errors (Decode, Network, etc.) we fall back to BAD_GATEWAY,
            // and the generic `{"error":{"message":...}}` body below
            // carries the operator-facing detail.
            HandlerError::TerminalUpstream {
                error: ProviderError::Upstream { status, .. },
                ..
            } => StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY),
            HandlerError::TerminalUpstream { .. } => StatusCode::BAD_GATEWAY,
            HandlerError::CapabilityMismatch { .. } => unreachable!("handled above"),
            HandlerError::Unauthorized { .. } => unreachable!("handled above"),
            HandlerError::NoUpstreamSucceeded { .. }
            | HandlerError::AllBreakersOpen { .. }
            | HandlerError::RateLimited { .. } => unreachable!("handled above"),
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
