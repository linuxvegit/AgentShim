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

use std::sync::{Arc, OnceLock};

use agent_shim_config::MetricsConfig;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

pub mod catalog;
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

/// Global recorder install slot. `metrics-exporter-prometheus` panics on a
/// second `install_recorder()` in the same process, so we cache the very
/// first handle here and clone it on every subsequent call. In production
/// the second branch is unreachable (install fires once at `AppState::build`);
/// in tests it lets each integration test that hand-builds an `AppCore`
/// share the same underlying recorder. Plan 02 P02 T3.
static INSTALLED: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the global metrics recorder. Returns a [`MetricsHandle`] for
/// rendering. Production callers invoke this exactly once during startup.
///
/// **Idempotent across calls.** The module-level [`INSTALLED`] `OnceLock`
/// caches the underlying `PrometheusHandle` so multiple invocations (e.g.
/// parallel tests, or any future second `install` call) do not panic the
/// `metrics-exporter-prometheus` "second install" guard. The returned
/// `MetricsHandle` wraps a clone of the cached handle and renders against
/// the same underlying recorder.
///
/// **First-config-wins.** Only the FIRST call's [`MetricsConfig`] is
/// honored — its `histogram_buckets` overrides are baked into the cached
/// recorder. Subsequent calls receive a `MetricsHandle` over the existing
/// recorder; their `histogram_buckets` overrides are silently ignored.
/// This is acceptable for production (one process, one install) and for
/// tests (each test binary is its own process). Tests that need
/// per-test bucket configuration must run in separate test binaries.
pub fn install(cfg: &MetricsConfig) -> Arc<MetricsHandle> {
    let handle = INSTALLED.get_or_init(|| {
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

        let h = builder
            .install_recorder()
            .expect("failed to install Prometheus recorder");

        describe_metrics();
        h
    });

    Arc::new(MetricsHandle {
        handle: handle.clone(),
    })
}

/// Register descriptions for every metric so /metrics returns text even
/// when no observation has been made. Without this, freshly-started
/// gateways serve an empty body and operators wonder if scrape works.
///
/// Iterates every descriptor in `catalog::iter_descriptors()` and emits
/// a `describe_*!` for each. Adding a new metric is a one-site edit in
/// `catalog.rs` (struct with `#[derive(Metric)]`). Plan v0.6.1 P02 (M-6).
fn describe_metrics() {
    use catalog::MetricKind;
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    for d in catalog::iter_descriptors() {
        match d.kind {
            MetricKind::Counter => describe_counter!(d.name, d.help),
            MetricKind::Histogram => describe_histogram!(d.name, d.help),
            MetricKind::Gauge => describe_gauge!(d.name, d.help),
        }
    }
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
