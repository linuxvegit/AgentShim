//! Inbound `traceparent` extraction. Plan 03 P03 T4.
//!
//! A small `tower::Layer` that, on every incoming request, parses any
//! `traceparent` header (W3C Trace Context) and attaches the resulting
//! `opentelemetry::Context` to the current `tracing::Span`. The OTel
//! layer (`tracing-opentelemetry`) propagates it from there.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use http::{HeaderMap, Request};
use opentelemetry::propagation::{Extractor, TextMapPropagator};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tower::{Layer, Service};
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Tower layer that extracts an inbound W3C `traceparent` header and
/// attaches the resulting OTel context to the current `tracing::Span`.
///
/// Place this layer ahead of [`RequestIdLayer`](crate::RequestIdLayer)
/// and any per-request tracing-instrumented work so child spans inherit
/// the parent context.
#[derive(Clone, Default)]
pub struct TraceparentLayer;

impl<S> Layer<S> for TraceparentLayer {
    type Service = TraceparentService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        TraceparentService { inner }
    }
}

#[derive(Clone)]
pub struct TraceparentService<S> {
    inner: S,
}

struct HeaderExtractor<'a>(&'a HeaderMap);

impl<'a> Extractor for HeaderExtractor<'a> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

impl<S, B> Service<Request<B>> for TraceparentService<S>
where
    S: Service<Request<B>> + Clone + Send + 'static,
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
        let propagator = TraceContextPropagator::new();
        let parent_ctx = propagator.extract(&HeaderExtractor(req.headers()));

        // Attach to the current tracing span so any child spans inherit
        // the parent context. Malformed `traceparent` headers produce an
        // empty/invalid OTel context here — `set_parent` is a no-op in
        // that case, so the layer is non-rejecting by design.
        Span::current().set_parent(parent_ctx);

        let mut inner = self.inner.clone();
        Box::pin(async move { inner.call(req).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Request, Response, StatusCode};
    use tower::ServiceExt;

    async fn ok_handler(_req: Request<()>) -> Result<Response<String>, std::convert::Infallible> {
        Ok(Response::builder()
            .status(StatusCode::OK)
            .body(String::new())
            .unwrap())
    }

    #[tokio::test]
    async fn well_formed_traceparent_is_accepted() {
        let svc = TraceparentLayer.layer(tower::service_fn(ok_handler));
        let req = Request::builder()
            .uri("/")
            .header(
                "traceparent",
                "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01",
            )
            .body(())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn malformed_traceparent_does_not_error() {
        let svc = TraceparentLayer.layer(tower::service_fn(ok_handler));
        let req = Request::builder()
            .uri("/")
            .header("traceparent", "garbage")
            .body(())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_traceparent_is_accepted() {
        let svc = TraceparentLayer.layer(tower::service_fn(ok_handler));
        let req = Request::builder().uri("/").body(()).unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
