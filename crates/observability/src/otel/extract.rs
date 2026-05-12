//! Inbound `traceparent` extraction. Plan 03 P03 T4.
//!
//! Provides [`extract_context_from_headers`], a free function that parses
//! an inbound `traceparent` header (W3C Trace Context) into an
//! [`opentelemetry::Context`]. Callers apply the returned context to a
//! real `tracing::Span` via [`tracing_opentelemetry::OpenTelemetrySpanExt::set_parent`].
//!
//! ## Why not a tower layer?
//!
//! The natural shape is a `tower::Layer` that calls
//! `Span::current().set_parent(...)` on every request. That doesn't work:
//! at the layer call site no per-request tracing span has been entered
//! yet (`Span::current()` is `Span::none()`), so the parent context never
//! reaches the `gateway.request` span created downstream in the pipeline.
//! Callers that own the root span — i.e. the pipeline `dispatch` — apply
//! the parent context themselves.
//!
//! P03 T4 followup: the previous version exposed a `TraceparentLayer`
//! with that exact bug. The layer has been removed; this module now
//! exposes only the parse helper, and the gateway's `dispatch` calls it
//! directly on `root_span`.

use http::HeaderMap;
use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry_sdk::propagation::TraceContextPropagator;

struct HeaderMapExtractor<'a>(&'a HeaderMap);

impl<'a> Extractor for HeaderMapExtractor<'a> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// Parse the W3C `traceparent` (and optional `tracestate`) headers into an
/// [`opentelemetry::Context`]. When the header is missing or malformed
/// the returned context is the empty/invalid one — `set_parent` on a
/// tracing span is a safe no-op in that case, so this function is
/// non-rejecting by design.
pub fn extract_context_from_headers(headers: &HeaderMap) -> opentelemetry::Context {
    TraceContextPropagator::new().extract(&HeaderMapExtractor(headers))
}

struct HeaderMapInjector<'a>(&'a mut http::HeaderMap);

impl<'a> Injector for HeaderMapInjector<'a> {
    fn set(&mut self, key: &str, value: String) {
        if let Ok(name) = http::header::HeaderName::try_from(key) {
            if let Ok(val) = http::HeaderValue::try_from(&value) {
                self.0.insert(name, val);
            }
        }
        // Malformed key/value pairs are silently dropped — observability
        // is diagnostic, never an admission gate. See spec §6.4.
    }
}

/// Inject the current `tracing::Span`'s OTel context as W3C `traceparent`
/// (and optional `tracestate`) into the given `HeaderMap`. Idempotent;
/// safe to call before every outbound HTTP request.
///
/// When `Span::current()` carries no OTel parent context (e.g. the request
/// arrived without a `traceparent` header and no internal span attached
/// one), the propagator writes nothing and this is a no-op. No 5xx is
/// ever produced from injection failure.
pub fn inject_context_into_headers(headers: &mut http::HeaderMap) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    let cx = tracing::Span::current().context();
    let propagator = TraceContextPropagator::new();
    propagator.inject_context(&cx, &mut HeaderMapInjector(headers));
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    /// Build a `HeaderMap` with a single header pair. Helper.
    fn headers_with(key: &'static str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(key, value.parse().unwrap());
        h
    }

    #[test]
    fn well_formed_traceparent_yields_remote_context() {
        let h = headers_with(
            "traceparent",
            "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01",
        );
        let ctx = extract_context_from_headers(&h);
        assert!(
            ctx.span().span_context().is_valid(),
            "parsed context should carry a valid SpanContext"
        );
    }

    #[test]
    fn malformed_traceparent_yields_empty_context() {
        let h = headers_with("traceparent", "garbage");
        let ctx = extract_context_from_headers(&h);
        // Empty contexts have an invalid SpanContext — set_parent on a
        // tracing span is a no-op for them, which is the desired behavior.
        assert!(
            !ctx.span().span_context().is_valid(),
            "garbage traceparent must not produce a valid SpanContext"
        );
    }

    #[test]
    fn missing_traceparent_yields_empty_context() {
        let h = HeaderMap::new();
        let ctx = extract_context_from_headers(&h);
        assert!(!ctx.span().span_context().is_valid());
    }

    /// Positive evidence (what the T7 review flagged was missing):
    /// the extracted context, applied via `set_parent`, actually shows
    /// up as the parent on a downstream tracing span.
    #[tokio::test]
    async fn extracted_context_parents_a_downstream_span() {
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
        use opentelemetry_sdk::trace::TracerProvider;
        use tracing_subscriber::layer::SubscriberExt;

        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("test");
        let layer = tracing_opentelemetry::layer().with_tracer(tracer);
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let h = headers_with(
            "traceparent",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
        );
        let parent_ctx = extract_context_from_headers(&h);
        {
            let span = tracing::info_span!("downstream");
            span.set_parent(parent_ctx);
            let _e = span.enter();
        }

        let spans = exporter.get_finished_spans().expect("export ok");
        let downstream = spans
            .iter()
            .find(|s| s.name == "downstream")
            .expect("downstream span exported");
        // Trace id must match the inbound header — proof that
        // `set_parent` did adopt the remote context.
        let trace_id = format!("{}", downstream.span_context.trace_id());
        assert_eq!(trace_id, "0af7651916cd43dd8448eb211c80319c");
    }

    #[test]
    fn inject_into_empty_headers_with_no_parent_writes_nothing() {
        let mut h = http::HeaderMap::new();
        inject_context_into_headers(&mut h);
        // With no active span carrying remote context, the propagator
        // writes nothing — the headers stay empty.
        assert!(
            h.is_empty(),
            "expected no injection without parent ctx; got {:?}",
            h
        );
    }

    #[tokio::test]
    async fn inject_writes_traceparent_when_parent_set() {
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
        use opentelemetry_sdk::trace::TracerProvider;
        use tracing_subscriber::layer::SubscriberExt;

        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("test"));
        let subscriber = tracing_subscriber::registry().with(layer);
        let _g = tracing::subscriber::set_default(subscriber);

        // Build an inbound HeaderMap with a known traceparent, parse it,
        // attach the resulting context to a span, then inject from inside
        // that span and verify the trace_id propagates.
        let mut inbound = http::HeaderMap::new();
        inbound.insert(
            "traceparent",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
                .parse()
                .unwrap(),
        );
        let parent_ctx = extract_context_from_headers(&inbound);

        let span = tracing::info_span!("outbound_test");
        span.set_parent(parent_ctx);
        let _e = span.enter();

        let mut outbound = http::HeaderMap::new();
        inject_context_into_headers(&mut outbound);

        let tp = outbound
            .get("traceparent")
            .expect("traceparent must be injected when parent context is active")
            .to_str()
            .unwrap();
        // W3C format: 00-<32 hex trace_id>-<16 hex span_id>-<flags>
        assert!(
            tp.contains("0af7651916cd43dd8448eb211c80319c"),
            "outbound trace_id must match inbound; got: {tp}"
        );
    }
}
