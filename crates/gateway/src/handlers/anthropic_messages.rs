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

use agent_shim_core::{BackendTarget, FrontendKind, StreamEvent, Usage};
use agent_shim_frontends::{FrontendProtocol, FrontendResponse};
use agent_shim_router::Router;

use crate::state::AppState;

use super::{collect_stream, HandlerError};

struct StreamLogger {
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
        let elapsed = self.started.elapsed();
        tracing::info!(
            "← /v1/messages (stream) | model: {} → {} | input: {} | output: {} | {:.1}s",
            self.model_alias,
            self.upstream_model,
            input,
            output,
            elapsed.as_secs_f64()
        );
    }
}

/// Spawn the upstream provider call on a background task and return a CanonicalStream.
/// Used by both Anthropic and OpenAI handlers.
pub(crate) fn spawn_provider_stream(
    provider: Arc<dyn agent_shim_providers::BackendProvider>,
    canonical: agent_shim_core::CanonicalRequest,
    target: BackendTarget,
) -> agent_shim_core::CanonicalStream {
    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<StreamEvent, agent_shim_core::StreamError>>(32);

    tokio::spawn(async move {
        match provider.complete(canonical, target).await {
            Ok(mut upstream) => {
                use futures::StreamExt as _;
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

pub async fn handle(
    State(state): State<AppState>,
    _headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    let body_bytes = body.len();
    let started = std::time::Instant::now();

    let canonical = state.anthropic.decode_request(&body).map_err(|e| {
        tracing::warn!(error = %e, "anthropic decode failed");
        HandlerError::Frontend(e)
    })?;

    let model_alias = canonical.model.as_str().to_string();
    let is_stream = canonical.stream;
    let max_tokens = canonical.generation.max_tokens;

    let target = state
        .router
        .resolve(FrontendKind::AnthropicMessages, &model_alias)
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

    let reasoning_effort = canonical
        .generation
        .reasoning
        .as_ref()
        .and_then(|r| r.effort)
        .or(target.default_reasoning_effort);
    let reasoning_budget = canonical
        .generation
        .reasoning
        .as_ref()
        .and_then(|r| r.budget_tokens);

    tracing::info!(
        "→ /v1/messages | model: {} → {} | bodyBytes: {} | maxTokens: {} | reasoning: {}{}",
        model_alias,
        upstream_model,
        body_bytes,
        max_tokens.unwrap_or(0),
        reasoning_effort.map(|e| e.as_str()).unwrap_or("none"),
        reasoning_budget
            .map(|b| format!(" (budget {} tok)", b))
            .unwrap_or_default(),
    );

    let provider = state.providers.get(&target.provider).ok_or_else(|| {
        tracing::error!(provider = %target.provider, "provider not registered");
        HandlerError::Provider(agent_shim_providers::ProviderError::UnknownProvider(
            target.provider.clone(),
        ))
    })?;

    if is_stream {
        let usage_capture: Arc<Mutex<Option<Usage>>> = Arc::new(Mutex::new(None));

        let logger = StreamLogger {
            model_alias: model_alias.clone(),
            upstream_model: upstream_model.clone(),
            usage: usage_capture.clone(),
            started,
        };

        let upstream_stream = spawn_provider_stream(provider.clone(), canonical, target);

        let logging_stream = upstream_stream.map({
            let usage_capture = usage_capture.clone();
            move |event| {
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
            }
        });

        let canonical_stream: agent_shim_core::CanonicalStream = Box::pin(logging_stream);
        let frontend_response = state.anthropic.encode_stream(canonical_stream);

        match frontend_response {
            FrontendResponse::Stream {
                content_type,
                stream: sse_stream,
            } => {
                let guarded_stream = GuardedStream {
                    inner: sse_stream,
                    _logger: logger,
                };
                let body = Body::from_stream(guarded_stream.map(|r| r.map_err(|e| e.to_string())));
                let mut r = Response::new(body);
                r.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    HeaderValue::from_str(&content_type)
                        .unwrap_or_else(|_| HeaderValue::from_static("text/event-stream")),
                );
                Ok(r)
            }
            _ => unreachable!(),
        }
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
        let elapsed = started.elapsed();
        tracing::info!(
            "← /v1/messages (unary) | model: {} → {} | input: {} | output: {} | {:.1}s",
            model_alias,
            upstream_model,
            input,
            output,
            elapsed.as_secs_f64()
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
                    HeaderValue::from_str(&content_type)
                        .unwrap_or_else(|_| HeaderValue::from_static("application/json")),
                );
                Ok(r)
            }
            _ => unreachable!(),
        }
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
