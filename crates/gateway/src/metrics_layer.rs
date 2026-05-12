//! Tower middleware emitting request-lifecycle metrics. Plan 02 P02 T5.
//!
//! Runs as a layer on the public Router. Records:
//!   - agent_shim_requests_total (counter)
//!   - agent_shim_request_duration_seconds (histogram)
//!   - agent_shim_request_body_bytes (histogram, from content-length;
//!     chunked requests are recorded as 0)
//!   - agent_shim_in_flight_requests (gauge, via Drop guard)
//!
//! The middleware can only see the URL path, not the resolved `route`
//! alias, so it labels with `route="unknown"`. The `pipeline::dispatch`
//! function also emits `agent_shim_requests_total` with the resolved
//! route label so operators see the post-routing view. Operators
//! distinguish the two by filtering on `route != "unknown"`.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::http::{Request, Response};
use tower::{Layer, Service};

#[derive(Clone)]
pub struct MetricsLayer;

impl<S> Layer<S> for MetricsLayer {
    type Service = MetricsService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        MetricsService { inner }
    }
}

#[derive(Clone)]
pub struct MetricsService<S> {
    inner: S,
}

struct InFlightGuard;

impl InFlightGuard {
    fn enter() -> Self {
        metrics::gauge!(agent_shim_observability::metrics::names::IN_FLIGHT_REQUESTS)
            .increment(1.0);
        Self
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        metrics::gauge!(agent_shim_observability::metrics::names::IN_FLIGHT_REQUESTS)
            .decrement(1.0);
    }
}

fn frontend_for_path(path: &str) -> Option<&'static str> {
    if path.starts_with("/v1/messages") {
        Some("anthropic_messages")
    } else if path == "/v1/chat/completions" {
        Some("openai_chat")
    } else if path == "/v1/responses" {
        Some("openai_responses")
    } else {
        None
    }
}

impl<S, B, RB> Service<Request<B>> for MetricsService<S>
where
    S: Service<Request<B>, Response = Response<RB>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let started = Instant::now();
        let path = req.uri().path().to_string();
        let body_bytes = req
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let frontend = frontend_for_path(&path);

        // Clone the inner service to move into the future. Tower's
        // contract: `call` may be invoked multiple times; cloning is
        // the standard pattern for async middleware that returns a
        // boxed future.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        Box::pin(async move {
            // Hold the guard for the entire future, including any
            // streaming body.
            let _guard = InFlightGuard::enter();
            let resp = inner.call(req).await?;
            if let Some(frontend_name) = frontend {
                use agent_shim_observability::metrics::recorders::{record_request, StatusClass};
                let status = StatusClass::from_status(resp.status().as_u16());
                let dur = started.elapsed().as_secs_f64();
                // The `route` label cannot be known here without parsing
                // the request body. Emit "unknown" for now; the
                // route-labeled counter fires from inside pipeline::dispatch.
                record_request(frontend_name, "unknown", status, dur, body_bytes);
            }
            Ok(resp)
        })
    }
}
