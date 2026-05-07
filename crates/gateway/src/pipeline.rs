//! The request pipeline — the deep module that turns an inbound HTTP request
//! into an HTTP response.
//!
//! Each frontend handler is now a thin axum binding that builds a
//! [`PipelineSpec`] and calls [`dispatch`]. Everything between
//! "decode the body" and "write the response" — route resolution, fuzzy model
//! matching, [`agent_shim_core::RoutePolicy`] resolution, provider lookup,
//! `proxy_raw` short-circuit, streaming vs unary branch, request/response
//! logging — lives here.
//!
//! This is the **interface is the test surface**: behaviour that used to be
//! re-implemented across three handlers (with subtle drift) now sits behind
//! [`dispatch`] and can be tested through it once.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use parking_lot::Mutex;

use agent_shim_core::{
    BackendTarget, CanonicalRequest, CanonicalStream, ContentBlock, StreamEvent, Usage,
};
use agent_shim_frontends::{FrontendProtocol, FrontendResponse};
use agent_shim_providers::{BackendProvider, ProviderCapabilities, ProviderError};

use crate::handlers::{collect_stream, frontend_response_to_axum, HandlerError};
use crate::state::AppState;

/// Per-frontend configuration for [`dispatch`].
///
/// Keep this struct small and dumb: it captures the genuine variations
/// between frontends, nothing more. New variations should be expressed by
/// adding a field here, not by branching on `frontend.kind()` inside the
/// pipeline.
pub struct PipelineSpec<'a> {
    /// The frontend doing decode/encode for this request.
    pub frontend: &'a dyn FrontendProtocol,
    /// Endpoint label used in log lines (e.g. `/v1/messages`).
    pub endpoint_label: &'static str,
    /// If true, every `anthropic-*` header on the inbound request is captured
    /// and threaded through to the provider via the resolved policy. Today
    /// only the Anthropic frontend uses this.
    pub capture_anthropic_headers: bool,
    /// If true, attempt `provider.proxy_raw` before decoding the body. Today
    /// only the OpenAI Responses frontend uses this — its raw passthrough
    /// avoids a parse/re-encode round-trip when the upstream natively speaks
    /// the Responses API.
    pub try_proxy_raw: bool,
    /// If true, emit a final usage log line when the streaming response is
    /// dropped. The Anthropic handler did this with a drop-guard for
    /// SSE-shaped output; the other two log immediately after spawning.
    pub log_streaming_usage_on_drop: bool,
}

/// Run the request through the pipeline and produce an HTTP response.
pub async fn dispatch(
    state: &AppState,
    spec: PipelineSpec<'_>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    let body_bytes = body.len();
    let started = std::time::Instant::now();

    // Resolve the route up front. The Responses passthrough path needs the
    // target before it has decoded the body, so route resolution has to come
    // first regardless of which path we take.
    let model_alias = if spec.try_proxy_raw {
        // For raw passthrough we have to peek at the body for the model.
        extract_model_from_body(&body)?
    } else {
        // Otherwise we'll know the model after decode; use a placeholder for
        // the route lookup until decode runs. The decode path below replaces
        // this.
        String::new()
    };

    let mut decoded: Option<agent_shim_core::CanonicalRequest> = None;
    let model_alias = if spec.try_proxy_raw {
        model_alias
    } else {
        let canonical = spec
            .frontend
            .decode_request(&body)
            .map_err(HandlerError::Frontend)?;
        let alias = canonical.model.as_str().to_string();
        decoded = Some(canonical);
        alias
    };

    let target = state
        .resolver
        .resolve(spec.frontend.kind(), &model_alias)
        .map_err(|e| {
            tracing::warn!(model = %model_alias, error = %e, "no route");
            HandlerError::Route(e)
        })?;

    let upstream_model = target.model.clone();

    let provider = state.providers.get(&target.provider).ok_or_else(|| {
        tracing::error!(provider = %target.provider, "provider not registered");
        HandlerError::Provider(agent_shim_providers::ProviderError::UnknownProvider(
            target.provider.clone(),
        ))
    })?;

    // Raw-passthrough short-circuit: only the Responses frontend uses this.
    // We don't know reasoning_effort etc. without decoding the body, so log
    // the route default as the best approximation.
    if spec.try_proxy_raw {
        tracing::info!(
            "→ {} | model: {} → {} | bodyBytes: {} | reasoning_default: {}",
            spec.endpoint_label,
            model_alias,
            upstream_model,
            body_bytes,
            target
                .policy
                .default_reasoning_effort
                .map(|e| e.as_str())
                .unwrap_or("none"),
        );

        if let Some((content_type, byte_stream)) = provider
            .proxy_raw(body.clone(), target.clone(), spec.frontend.kind())
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "proxy_raw failed");
                HandlerError::Provider(e)
            })?
        {
            tracing::info!(
                "← {} (passthrough) | model: {} → {} | {:.1}s",
                spec.endpoint_label,
                model_alias,
                upstream_model,
                started.elapsed().as_secs_f64()
            );
            let body_stream = Body::from_stream(byte_stream.map(|r| r.map_err(|e| e.to_string())));
            let mut r = Response::new(body_stream);
            r.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_str(&content_type)
                    .unwrap_or_else(|_| HeaderValue::from_static("text/event-stream")),
            );
            return Ok(r);
        }

        // Passthrough not supported — fall through with a fresh decode.
        let canonical = spec
            .frontend
            .decode_request(&body)
            .map_err(HandlerError::Frontend)?;
        decoded = Some(canonical);
    }

    let mut canonical = decoded.expect("canonical request was decoded above");

    if spec.capture_anthropic_headers {
        canonical.inbound_anthropic_headers = capture_anthropic_headers(&headers);
    }

    // Snapshot the merged route policy onto the canonical request. Providers
    // and logging both read from `resolved_policy` so the merge rule lives in
    // exactly one place (RoutePolicy::resolve).
    canonical.resolved_policy = target.policy.resolve(&canonical);

    // Capability gate. Reject before any network call when the request asks
    // for a feature the target provider can't deliver — today this is just
    // vision (image blocks against a text-only backend). The check has to
    // run after decode (so we can scan content blocks) but before
    // run_stream / run_unary (so we never reach the upstream).
    check_capabilities(&canonical, provider.capabilities()).map_err(|e| {
        tracing::warn!(
            provider = %target.provider,
            error = %e,
            "capability mismatch — rejecting before upstream call"
        );
        HandlerError::CapabilityMismatch {
            kind: spec.frontend.kind(),
            message: e.to_string(),
        }
    })?;

    let is_stream = canonical.stream;
    let max_tokens = canonical.generation.max_tokens;
    let reasoning_budget = canonical
        .generation
        .reasoning
        .as_ref()
        .and_then(|r| r.budget_tokens);
    let beta_log = canonical
        .resolved_policy
        .anthropic_header("anthropic-beta")
        .map(|s| s.to_string());

    tracing::info!(
        "→ {} | model: {} → {} | bodyBytes: {} | maxTokens: {} | stream: {} | reasoning: {}{}{}",
        spec.endpoint_label,
        model_alias,
        upstream_model,
        body_bytes,
        max_tokens.unwrap_or(0),
        is_stream,
        canonical
            .resolved_policy
            .reasoning_effort
            .map(|e| e.as_str())
            .unwrap_or("none"),
        reasoning_budget
            .map(|b| format!(" (budget {} tok)", b))
            .unwrap_or_default(),
        beta_log
            .as_deref()
            .map(|b| format!(" | beta: {}", b))
            .unwrap_or_default(),
    );

    if is_stream {
        run_stream(
            spec,
            provider,
            canonical,
            target,
            RunContext {
                model_alias,
                upstream_model,
                started,
            },
        )
    } else {
        run_unary(
            spec,
            provider,
            canonical,
            target,
            RunContext {
                model_alias,
                upstream_model,
                started,
            },
        )
        .await
    }
}

/// Per-request context shared between the streaming and unary branches.
struct RunContext {
    model_alias: String,
    upstream_model: String,
    started: std::time::Instant,
}

fn run_stream(
    spec: PipelineSpec<'_>,
    provider: Arc<dyn BackendProvider>,
    canonical: agent_shim_core::CanonicalRequest,
    target: BackendTarget,
    ctx: RunContext,
) -> Result<Response, HandlerError> {
    let label = spec.endpoint_label;
    let RunContext {
        model_alias,
        upstream_model,
        started,
    } = ctx;

    if spec.log_streaming_usage_on_drop {
        // Anthropic-style: log final usage when the SSE stream is dropped.
        let usage_capture: Arc<Mutex<Option<Usage>>> = Arc::new(Mutex::new(None));
        let logger = StreamLogger {
            endpoint_label: label,
            model_alias: model_alias.clone(),
            upstream_model: upstream_model.clone(),
            usage: usage_capture.clone(),
            started,
        };

        let upstream_stream = spawn_provider_stream(provider, canonical, target);
        let logging_stream = upstream_stream.map(move |event| {
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

        let canonical_stream: CanonicalStream = Box::pin(logging_stream);
        let frontend_response = spec.frontend.encode_stream(canonical_stream);

        match frontend_response {
            FrontendResponse::Stream {
                content_type,
                stream: sse_stream,
            } => {
                let guarded = GuardedStream {
                    inner: sse_stream,
                    _logger: logger,
                };
                let body = Body::from_stream(guarded.map(|r| r.map_err(|e| e.to_string())));
                let mut r = Response::new(body);
                r.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    HeaderValue::from_str(&content_type)
                        .unwrap_or_else(|_| HeaderValue::from_static("text/event-stream")),
                );
                Ok(r)
            }
            _ => unreachable!("encode_stream must return Stream"),
        }
    } else {
        // Plain post-spawn log (OpenAI Chat / Responses streaming today).
        let upstream_stream = spawn_provider_stream(provider, canonical, target);
        let frontend_response = spec.frontend.encode_stream(upstream_stream);
        tracing::info!(
            "← {} (stream) | model: {} → {} | {:.1}s",
            label,
            model_alias,
            upstream_model,
            started.elapsed().as_secs_f64()
        );
        Ok(frontend_response_to_axum(frontend_response))
    }
}

async fn run_unary(
    spec: PipelineSpec<'_>,
    provider: Arc<dyn BackendProvider>,
    canonical: agent_shim_core::CanonicalRequest,
    target: BackendTarget,
    ctx: RunContext,
) -> Result<Response, HandlerError> {
    let RunContext {
        model_alias,
        upstream_model,
        started,
    } = ctx;
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
        "← {} (unary) | model: {} → {} | input: {} | output: {} | {:.1}s",
        spec.endpoint_label,
        model_alias,
        upstream_model,
        input,
        output,
        started.elapsed().as_secs_f64()
    );
    let frontend_response = spec
        .frontend
        .encode_unary(response)
        .map_err(HandlerError::Frontend)?;
    Ok(frontend_response_to_axum(frontend_response))
}

// ── internals ─────────────────────────────────────────────────────────────

/// Pull every `anthropic-*` header off the inbound request, dropping the two
/// credential headers that are upstream-owned. Returns name/value pairs in
/// arrival order so providers can replay them verbatim.
fn capture_anthropic_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, value) in headers.iter() {
        let name_str = name.as_str();
        if !name_str.starts_with("anthropic-") {
            continue;
        }
        if matches!(name_str, "anthropic-api-key" | "anthropic-auth-token") {
            continue;
        }
        if let Ok(v) = value.to_str() {
            out.push((name_str.to_string(), v.to_string()));
        }
    }
    out
}

/// Spawn the upstream provider call on a background task and return a
/// CanonicalStream the frontend can encode lazily.
pub(crate) fn spawn_provider_stream(
    provider: Arc<dyn BackendProvider>,
    canonical: agent_shim_core::CanonicalRequest,
    target: BackendTarget,
) -> CanonicalStream {
    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<StreamEvent, agent_shim_core::StreamError>>(32);

    tokio::spawn(async move {
        match provider.complete(canonical, target).await {
            Ok(mut upstream) => {
                while let Some(event) = upstream.next().await {
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "provider.complete failed");
                let _ = tx
                    .send(Err(agent_shim_core::StreamError::Upstream(e.to_string())))
                    .await;
            }
        }
    });

    Box::pin(futures::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|item| (item, rx))
    }))
}

/// Peek at the JSON body to extract the `model` field for raw-passthrough
/// route resolution.
fn extract_model_from_body(body: &[u8]) -> Result<String, HandlerError> {
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

/// Reject the request before dispatch when the inbound canonical body asks
/// for a feature the target provider cannot service. Today this is just
/// vision (any `ContentBlock::Image` against a backend whose
/// `capabilities.vision == false`); future capability mismatches add new
/// branches here.
///
/// Plan 04 D6: the check runs at the gateway boundary, not inside each
/// provider, so the error envelope is shaped by the inbound frontend (not
/// whichever upstream happened to be wired). Returning `Err` here aborts
/// before any network I/O, which is also what the mockito-based test in
/// Plan 04 T9 asserts (zero hits on the upstream mock).
fn check_capabilities(
    req: &CanonicalRequest,
    caps: &ProviderCapabilities,
) -> Result<(), ProviderError> {
    if request_has_image(req) && !caps.vision {
        return Err(ProviderError::CapabilityMismatch(
            "target provider does not support vision (image blocks present in request)".into(),
        ));
    }
    Ok(())
}

/// Scan every message's content blocks for an `Image` variant. Used by the
/// capability gate; pulled out so unit tests can exercise the predicate
/// against handcrafted requests without spinning up a full pipeline.
fn request_has_image(req: &CanonicalRequest) -> bool {
    req.messages
        .iter()
        .flat_map(|m| m.content.iter())
        .any(|b| matches!(b, ContentBlock::Image(_)))
}

struct StreamLogger {
    endpoint_label: &'static str,
    model_alias: String,
    upstream_model: String,
    usage: Arc<Mutex<Option<Usage>>>,
    started: std::time::Instant,
}

impl Drop for StreamLogger {
    fn drop(&mut self) {
        let u = self.usage.lock().clone();
        let (input, output) = match u {
            Some(ref usage) => (
                usage.input_tokens.unwrap_or(0),
                usage.output_tokens.unwrap_or(0),
            ),
            None => (0, 0),
        };
        tracing::info!(
            "← {} (stream) | model: {} → {} | input: {} | output: {} | {:.1}s",
            self.endpoint_label,
            self.model_alias,
            self.upstream_model,
            input,
            output,
            self.started.elapsed().as_secs_f64()
        );
    }
}

struct GuardedStream<S> {
    inner: S,
    _logger: StreamLogger,
}

impl<S: futures::Stream + Unpin> futures::Stream for GuardedStream<S> {
    type Item = S::Item;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_model_from_body_reads_top_level_field() {
        let body = br#"{"model":"gpt-4o","messages":[]}"#;
        assert_eq!(extract_model_from_body(body).unwrap(), "gpt-4o");
    }

    #[test]
    fn extract_model_from_body_rejects_missing_model() {
        let body = br#"{"messages":[]}"#;
        let err = extract_model_from_body(body).unwrap_err();
        assert!(matches!(err, HandlerError::Frontend(_)));
    }

    #[test]
    fn capture_anthropic_headers_filters_prefix_and_drops_credentials() {
        let mut h = HeaderMap::new();
        h.insert("content-type", "application/json".parse().unwrap());
        h.insert("anthropic-beta", "context-1m-2025-08-07".parse().unwrap());
        h.insert("anthropic-version", "2023-06-01".parse().unwrap());
        h.insert(
            "anthropic-api-key",
            "secret-should-be-dropped".parse().unwrap(),
        );
        h.insert("ANTHROPIC-AUTH-TOKEN", "another-secret".parse().unwrap());

        let captured = capture_anthropic_headers(&h);
        let names: Vec<&str> = captured.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"anthropic-beta"));
        assert!(names.contains(&"anthropic-version"));
        assert!(!names.iter().any(|n| n.contains("api-key")));
        assert!(!names.iter().any(|n| n.contains("auth-token")));
        assert!(!names.contains(&"content-type"));
    }

    // ── capability gate ───────────────────────────────────────────────

    use agent_shim_core::{
        media::BinarySource, request::RequestMetadata, ContentBlock, ExtensionMap, FrontendInfo,
        FrontendKind, FrontendModel, GenerationOptions, ImageBlock, Message, MessageRole,
        RequestId, ResolvedPolicy,
    };

    /// Build a CanonicalRequest carrying a single user message whose content
    /// blocks come from the caller. Keeps every other field at its default
    /// — the gate only inspects `messages[*].content`, so this is enough.
    fn req_with_content(blocks: Vec<ContentBlock>) -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: FrontendModel::from("test-model"),
            },
            model: FrontendModel::from("test-model"),
            system: vec![],
            messages: vec![Message {
                role: MessageRole::User,
                content: blocks,
                name: None,
                extensions: ExtensionMap::new(),
            }],
            tools: vec![],
            tool_choice: Default::default(),
            generation: GenerationOptions::default(),
            response_format: None,
            stream: false,
            metadata: RequestMetadata::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: ResolvedPolicy::default(),
            extensions: ExtensionMap::new(),
        }
    }

    fn img_block() -> ContentBlock {
        ContentBlock::Image(ImageBlock {
            source: BinarySource::Url {
                url: "https://example.com/cat.png".into(),
            },
            extensions: Default::default(),
        })
    }

    #[test]
    fn request_has_image_detects_image_block() {
        let req = req_with_content(vec![ContentBlock::text("hi"), img_block()]);
        assert!(request_has_image(&req));
    }

    #[test]
    fn request_has_image_returns_false_for_text_only() {
        let req = req_with_content(vec![ContentBlock::text("hi")]);
        assert!(!request_has_image(&req));
    }

    #[test]
    fn check_capabilities_passes_when_no_image_and_no_vision() {
        // Provider without vision is still fine for a text-only request.
        let caps = ProviderCapabilities {
            streaming: true,
            tool_use: false,
            vision: false,
            json_mode: false,
        };
        let req = req_with_content(vec![ContentBlock::text("hi")]);
        assert!(check_capabilities(&req, &caps).is_ok());
    }

    #[test]
    fn check_capabilities_passes_when_image_and_vision() {
        let caps = ProviderCapabilities {
            streaming: true,
            tool_use: false,
            vision: true,
            json_mode: false,
        };
        let req = req_with_content(vec![ContentBlock::text("describe"), img_block()]);
        assert!(check_capabilities(&req, &caps).is_ok());
    }

    #[test]
    fn check_capabilities_rejects_image_against_text_only_provider() {
        let caps = ProviderCapabilities {
            streaming: true,
            tool_use: false,
            vision: false,
            json_mode: false,
        };
        let req = req_with_content(vec![img_block()]);
        let err = check_capabilities(&req, &caps).unwrap_err();
        // Variant + message both observable so future refactors can't quietly
        // turn this into a different error class.
        match err {
            ProviderError::CapabilityMismatch(msg) => {
                assert!(msg.contains("vision"), "message must mention vision: {msg}");
            }
            other => panic!("expected CapabilityMismatch, got {other:?}"),
        }
    }
}
