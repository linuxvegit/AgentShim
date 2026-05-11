//! metrics-rs integration for AgentShim.
//!
//! Spec §3 (Plan 02). The crate exposes:
//! - [`install`] — initialize the global recorder; called once at startup.
//! - [`MetricsHandle`] — render Prometheus text; held in `AppCore`.
//! - [`names`] — every metric name as a `pub const`.
//!
//! The `metrics-rs` facade dispatches to whichever recorder was installed.
//! In production we install [`metrics_exporter_prometheus`]; multiple
//! installs in the same process (e.g. across tests) all return fresh
//! handles, but only the first one's recorder is actually wired into
//! the global facade — subsequent installs return their own
//! `PrometheusHandle` that operate on the original recorder's data.

use std::sync::Arc;

use agent_shim_config::MetricsConfig;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

pub mod names;
pub mod recorders;

/// Renders Prometheus text on demand. One process owns one of these.
pub struct MetricsHandle {
    handle: PrometheusHandle,
}

impl MetricsHandle {
    pub fn render(&self) -> String {
        self.handle.render()
    }
}

/// Install the global metrics recorder. Returns a [`MetricsHandle`] for
/// rendering. Production callers invoke this exactly once during startup;
/// subsequent installs in the same process (e.g. parallel tests) are
/// no-ops at the global level but still return a `MetricsHandle` whose
/// `render()` operates against whichever recorder won the install race.
pub fn install(cfg: &MetricsConfig) -> Arc<MetricsHandle> {
    let mut builder = PrometheusBuilder::new();

    // Per-metric histogram bucket overrides.
    for (name, buckets) in &cfg.histogram_buckets {
        // Accept both qualified (`agent_shim_request_duration_seconds`)
        // and bare (`request_duration_seconds`) forms.
        let qualified = if name.starts_with("agent_shim_") {
            name.clone()
        } else {
            format!("agent_shim_{name}")
        };
        builder = builder
            .set_buckets_for_metric(Matcher::Full(qualified), buckets)
            .expect("histogram bucket override must be valid");
    }

    let handle = builder
        .install_recorder()
        .expect("failed to install Prometheus recorder");

    describe_metrics();

    Arc::new(MetricsHandle { handle })
}

/// Register descriptions for every metric so /metrics returns text even
/// when no observation has been made. Without this, freshly-started
/// gateways serve an empty body and operators wonder if scrape works.
fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};
    use names::*;

    describe_counter!(
        REQUESTS_TOTAL,
        "Total HTTP requests by frontend, route, status_class"
    );
    describe_histogram!(
        REQUEST_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "End-to-end request duration"
    );
    describe_histogram!(
        REQUEST_BODY_BYTES,
        metrics::Unit::Bytes,
        "Inbound request body size"
    );
    describe_gauge!(IN_FLIGHT_REQUESTS, "Currently-in-flight requests");

    describe_counter!(
        RETRY_ATTEMPTS_TOTAL,
        "Retry attempts by route, upstream, attempt number"
    );
    describe_counter!(
        RETRY_EXHAUSTED_TOTAL,
        "Routes that exhausted all retry attempts"
    );
    describe_counter!(
        FALLBACK_TRANSITIONS_TOTAL,
        "Fallback chain element transitions"
    );
    describe_counter!(
        BREAKER_STATE_CHANGES_TOTAL,
        "Circuit breaker state transitions"
    );
    describe_counter!(
        RATE_LIMIT_REJECTED_TOTAL,
        "Rate-limit rejections by dimension"
    );

    describe_histogram!(
        UPSTREAM_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "Upstream call duration"
    );
    describe_counter!(UPSTREAM_ERRORS_TOTAL, "Upstream errors by class");

    describe_counter!(TOKENS_INPUT_TOTAL, "Input tokens consumed");
    describe_counter!(TOKENS_OUTPUT_TOTAL, "Output tokens produced");

    describe_counter!(CONFIG_RELOADS_TOTAL, "Config reload attempts by result");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::recorders::{self, StatusClass};

    /// Concurrent tests in the same crate share a global recorder, so we
    /// install via a single module-scoped `OnceLock` — both tests pull
    /// the same handle. The init closure also seeds one observation per
    /// metric we later assert on, because the Prometheus exporter only
    /// renders metric lines after the first observation; describe alone
    /// emits internal HELP/TYPE metadata but doesn't surface in render
    /// output until a value lands.
    static SHARED_HANDLE: std::sync::OnceLock<Arc<MetricsHandle>> = std::sync::OnceLock::new();

    fn handle() -> Arc<MetricsHandle> {
        SHARED_HANDLE
            .get_or_init(|| {
                let h = install(&MetricsConfig::default());
                // Seed one observation per metric the tests assert on so
                // render() emits them. Real callsites will overwhelm
                // these noise values within seconds of traffic.
                recorders::record_request("anthropic", "/messages", StatusClass::Success, 0.001, 0);
                recorders::record_retry_attempt("default", "openai", 1);
                recorders::record_fallback_transition("default", "openai", "anthropic");
                recorders::record_breaker_state_change("openai", "gpt-5", "closed", "open");
                recorders::record_rate_limit_rejected("per_ip");
                recorders::record_config_reload("success");
                h
            })
            .clone()
    }

    #[test]
    fn install_returns_renderable_handle() {
        let h = handle();
        let body = h.render();
        assert!(body.contains("agent_shim_requests_total"));
        assert!(body.contains("# HELP"));
    }

    #[test]
    fn render_includes_all_described_names() {
        let h = handle();
        let body = h.render();
        for name in [
            names::REQUESTS_TOTAL,
            names::RETRY_ATTEMPTS_TOTAL,
            names::FALLBACK_TRANSITIONS_TOTAL,
            names::BREAKER_STATE_CHANGES_TOTAL,
            names::RATE_LIMIT_REJECTED_TOTAL,
            names::CONFIG_RELOADS_TOTAL,
        ] {
            assert!(body.contains(name), "render missing {name}");
        }
    }
}
