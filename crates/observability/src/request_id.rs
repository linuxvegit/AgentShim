use http::{header::HeaderName, HeaderValue, Request, Response};
use std::{
    future::Future,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
};
use tower::{Layer, Service};
use uuid::Uuid;

static X_REQUEST_ID: &str = "x-request-id";

/// Tower layer that injects an `x-request-id` header if one is not already present.
#[derive(Clone, Default)]
pub struct RequestIdLayer;

impl<S> Layer<S> for RequestIdLayer {
    type Service = RequestIdService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestIdService { inner }
    }
}

#[derive(Clone)]
pub struct RequestIdService<S> {
    inner: S,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for RequestIdService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
    S::Future: Send + 'static,
    ReqBody: Send + 'static,
    ResBody: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<ReqBody>) -> Self::Future {
        let header_name = HeaderName::from_str(X_REQUEST_ID).unwrap();
        if !req.headers().contains_key(&header_name) {
            let id = Uuid::new_v4().to_string();
            req.headers_mut()
                .insert(header_name, HeaderValue::from_str(&id).unwrap());
        }
        let fut = self.inner.call(req);
        Box::pin(fut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Request, Response};
    use tower::ServiceExt;

    async fn echo_handler(req: Request<()>) -> Result<Response<String>, std::convert::Infallible> {
        let id = req
            .headers()
            .get(X_REQUEST_ID)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        Ok(Response::new(id))
    }

    #[tokio::test]
    async fn injects_request_id_when_missing() {
        let svc = RequestIdLayer.layer(tower::service_fn(echo_handler));
        let req = Request::builder().uri("/").body(()).unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert!(
            !resp.body().is_empty(),
            "expected a request-id to be injected"
        );
    }

    #[tokio::test]
    async fn preserves_existing_request_id() {
        let svc = RequestIdLayer.layer(tower::service_fn(echo_handler));
        let req = Request::builder()
            .uri("/")
            .header(X_REQUEST_ID, "existing-id-123")
            .body(())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.body(), "existing-id-123");
    }
}
