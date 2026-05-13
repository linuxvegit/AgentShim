//! OpenTelemetry tracing layer. Plan 03 P03 T2.
//!
//! When `OtelConfig::endpoint` is set, builds an OTLP/gRPC exporter and
//! a `tracing-opentelemetry` layer that translates `tracing::span!`s into
//! OTel spans. When unset, returns `Ok(None)` and skips exporter init
//! entirely — spans still exist for the fmt layer.

pub mod extract;
pub use extract::{extract_context_from_headers, inject_context_into_headers};

use agent_shim_config::schema::OtelConfig;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    runtime,
    trace::{self, Sampler, TracerProvider},
    Resource,
};

/// Holds resources that must outlive a successful `init` call: the
/// `TracerProvider` (so its async batch exporter can drain on shutdown).
/// Drop or call [`OtelHandle::shutdown`] to flush.
pub struct OtelHandle {
    provider: TracerProvider,
}

impl OtelHandle {
    /// Block until pending spans are drained, then drop the provider.
    /// Production callers should run this before exit so the OTLP buffer
    /// is flushed.
    pub fn shutdown(self) {
        // Explicit shutdown drains the batch exporter; preferred over Drop
        // because the Tokio runtime may already be shutting down by then.
        let _ = self.provider.shutdown();
    }
}

/// Build the optional `tracing-opentelemetry` layer. When `cfg.endpoint`
/// is `None`, returns `Ok(None)` and the subscriber stack runs without
/// the OTel layer.
///
/// Returns the layer plus an [`OtelHandle`] whose lifetime must extend to
/// process shutdown so the OTLP batch exporter has a chance to drain.
pub fn build_layer<S>(
    cfg: &OtelConfig,
) -> anyhow::Result<
    Option<(
        tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>,
        OtelHandle,
    )>,
>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    let endpoint = match &cfg.endpoint {
        Some(e) => e,
        None => return Ok(None),
    };

    let exporter = opentelemetry_otlp::new_exporter()
        .tonic()
        .with_endpoint(endpoint)
        .with_timeout(std::time::Duration::from_secs(5))
        .build_span_exporter()?;

    let mut resource_attrs = vec![KeyValue::new("service.name", cfg.service_name.clone())];
    if let Some(v) = &cfg.service_version {
        resource_attrs.push(KeyValue::new("service.version", v.clone()));
    }
    for (k, v) in &cfg.resource_attrs {
        resource_attrs.push(KeyValue::new(k.clone(), v.clone()));
    }
    let resource = Resource::new(resource_attrs);

    let inner_sampler = if cfg.sample_ratio >= 1.0 {
        Sampler::AlwaysOn
    } else if cfg.sample_ratio <= 0.0 {
        Sampler::AlwaysOff
    } else {
        Sampler::TraceIdRatioBased(cfg.sample_ratio)
    };
    let sampler = Sampler::ParentBased(Box::new(inner_sampler));

    let provider = TracerProvider::builder()
        .with_batch_exporter(exporter, runtime::Tokio)
        .with_config(
            trace::Config::default()
                .with_resource(resource)
                .with_sampler(sampler),
        )
        .build();

    let tracer = provider.tracer("agent-shim");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    Ok(Some((layer, OtelHandle { provider })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_layer_returns_none_when_endpoint_unset() {
        let cfg = OtelConfig::default();
        let result = build_layer::<tracing_subscriber::Registry>(&cfg);
        assert!(matches!(result, Ok(None)));
    }

    // We can't easily test the Some(...) path here without an OTLP
    // collector or a runtime — those tests live in T3+ where the OTel
    // layer is actually wired into a tracing Subscriber.
}
