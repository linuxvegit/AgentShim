//! Declarative metric catalog. Plan v0.6.1 P02 (M-6).
//!
//! Each emitted metric is declared as a zero-sized marker struct with
//! `#[derive(Metric)]` + a `#[metric(name = ..., kind = ..., help = ...)]`
//! attribute. The derive emits:
//!
//! - Three `pub const`s on the struct (`NAME`, `KIND`, `HELP`),
//! - A `MetricDescriptor` entry in the `METRIC_DESCRIPTORS`
//!   distributed slice, picked up at link time.
//!
//! Consumers iterate descriptors via [`iter_descriptors`] — used by
//! `describe_metrics()` (to register Prometheus HELP/TYPE lines on
//! startup) and by the structural-parity test.
//!
//! # Unit annotations
//!
//! v0.6.0's hand-written `describe_metrics()` passed `metrics::Unit::Seconds`
//! to time histograms (`*_duration_seconds`) and `metrics::Unit::Bytes` to
//! `request_body_bytes`. The catalog drops those annotations: `MetricKind`
//! has no unit field, so `describe_metrics()` calls the 2-arg `(name, help)`
//! form for every metric. Unit semantics are conveyed by the standard
//! Prometheus name-suffix convention (`_seconds`, `_bytes`, `_total`) which
//! every metric in the catalog already follows; /metrics scrape output is
//! unchanged because the metrics-rs facade does not surface units in the
//! text format. If a future metric needs explicit `Unit` metadata at
//! registration time, extend `MetricDescriptor` + the derive's
//! `#[metric(unit = "...")]` attribute and update this note.
//!
//! # Safety / `unsafe_code`
//!
//! `linkme`'s `#[distributed_slice]` macro expands into `#[link_section]`
//! attributes on statics. Rustc considers `link_section` unsafe (the
//! linker may produce surprising results if section names collide). We
//! locally `allow(unsafe_code)` for this module because:
//!
//! 1. `linkme` is a well-vetted abstraction (Dtolnay-authored, used by
//!    `inventory` and the wider Rust ecosystem),
//! 2. The section names it picks are unique per slice and do not collide
//!    with anything else in the binary,
//! 3. The alternative (manual registration in `describe_metrics()`) is
//!    exactly the boilerplate this catalog module is designed to delete.
#![allow(unsafe_code)]

use agent_shim_observability_derive::Metric;
use linkme::distributed_slice;

/// Kind of a Prometheus metric. Determines which `metrics::describe_*!`
/// macro the runtime registration loop uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricKind {
    Counter,
    Histogram,
    Gauge,
}

/// Descriptor emitted by `#[derive(Metric)]` and collected at link time.
#[derive(Debug, Clone, Copy)]
pub struct MetricDescriptor {
    pub name: &'static str,
    pub kind: MetricKind,
    pub help: &'static str,
}

/// All metrics declared with `#[derive(Metric)]`, collected at link time.
#[distributed_slice]
pub static METRIC_DESCRIPTORS: [MetricDescriptor] = [..];

/// Iterate every registered metric descriptor in name-sorted order.
pub fn iter_descriptors() -> Vec<&'static MetricDescriptor> {
    let mut all: Vec<&'static MetricDescriptor> = METRIC_DESCRIPTORS.iter().collect();
    all.sort_by_key(|d| d.name);
    all
}

// --- Marker structs: one per emitted metric ---------------------------
//
// Order mirrors `describe_metrics()` in `mod.rs` for easy parity review.
// Sub-section dividers below group by domain.

// --- Request lifecycle ---

#[derive(Metric)]
#[metric(
    name = "agent_shim_requests_total",
    kind = "counter",
    help = "Total HTTP requests by frontend, route, status_class"
)]
pub struct RequestsTotal;

#[derive(Metric)]
#[metric(
    name = "agent_shim_request_duration_seconds",
    kind = "histogram",
    help = "End-to-end request duration"
)]
pub struct RequestDurationSeconds;

#[derive(Metric)]
#[metric(
    name = "agent_shim_request_body_bytes",
    kind = "histogram",
    help = "Inbound request body size"
)]
pub struct RequestBodyBytes;

#[derive(Metric)]
#[metric(
    name = "agent_shim_in_flight_requests",
    kind = "gauge",
    help = "Currently-in-flight requests"
)]
pub struct InFlightRequests;

// --- Resilience layer (mirrors v0.4 tracing taxonomy) ---

#[derive(Metric)]
#[metric(
    name = "agent_shim_retry_attempts_total",
    kind = "counter",
    help = "Retry attempts by route, upstream, attempt number"
)]
pub struct RetryAttemptsTotal;

#[derive(Metric)]
#[metric(
    name = "agent_shim_retry_exhausted_total",
    kind = "counter",
    help = "Routes that exhausted all retry attempts"
)]
pub struct RetryExhaustedTotal;

#[derive(Metric)]
#[metric(
    name = "agent_shim_fallback_transitions_total",
    kind = "counter",
    help = "Fallback chain element transitions"
)]
pub struct FallbackTransitionsTotal;

#[derive(Metric)]
#[metric(
    name = "agent_shim_breaker_state_changes_total",
    kind = "counter",
    help = "Circuit breaker state transitions"
)]
pub struct BreakerStateChangesTotal;

#[derive(Metric)]
#[metric(
    name = "agent_shim_rate_limit_rejected_total",
    kind = "counter",
    help = "Rate-limit rejections by dimension"
)]
pub struct RateLimitRejectedTotal;

// --- Upstream call ---

#[derive(Metric)]
#[metric(
    name = "agent_shim_upstream_duration_seconds",
    kind = "histogram",
    help = "Upstream call duration"
)]
pub struct UpstreamDurationSeconds;

#[derive(Metric)]
#[metric(
    name = "agent_shim_upstream_errors_total",
    kind = "counter",
    help = "Upstream errors by class"
)]
pub struct UpstreamErrorsTotal;

// --- Token accounting ---

#[derive(Metric)]
#[metric(
    name = "agent_shim_tokens_input_total",
    kind = "counter",
    help = "Input tokens consumed"
)]
pub struct TokensInputTotal;

#[derive(Metric)]
#[metric(
    name = "agent_shim_tokens_output_total",
    kind = "counter",
    help = "Output tokens produced"
)]
pub struct TokensOutputTotal;

// --- Reload (used by Plan 04) ---

#[derive(Metric)]
#[metric(
    name = "agent_shim_config_reloads_total",
    kind = "counter",
    help = "Config reload attempts by result"
)]
pub struct ConfigReloadsTotal;

// --- Cost-aware routing (Phase 6 P04) ---

#[derive(Metric)]
#[metric(
    name = "agent_shim_cost_filtered_total",
    kind = "counter",
    help = "Cost-filter skip/note counts by reason, upstream, route (Plan 06 P04)"
)]
pub struct CostFilteredTotal;

// --- Plugin observability (Phase 7 P05) ---

#[derive(Metric)]
#[metric(
    name = "agent_shim_plugin_invocations_total",
    kind = "counter",
    help = "Plugin hook invocations by kind, name, hook, outcome (Plan 07 P05)"
)]
pub struct PluginInvocationsTotal;

#[derive(Metric)]
#[metric(
    name = "agent_shim_plugin_duration_seconds",
    kind = "histogram",
    help = "Plugin hook duration by kind, name, hook (Plan 07 P05)"
)]
pub struct PluginDurationSeconds;

#[derive(Metric)]
#[metric(
    name = "agent_shim_plugin_h7_dropped_at_shutdown_total",
    kind = "counter",
    help = "H7 plugin tasks dropped at shutdown by plugin_name (Plan 07 P05)"
)]
pub struct PluginH7DroppedTotal;

// --- Built-in plugins (Phase 7 P06b) ---

#[derive(Metric)]
#[metric(
    name = "agent_shim_plugin_pii_scrubber_matches_total",
    kind = "counter",
    help = "Total PII scrub rule matches by rule and direction (inbound|outbound) (Plan 07 P06b1)"
)]
pub struct PluginPiiScrubberMatchesTotal;

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the linkme slice picked up every marker struct in this
    /// module. T6/T7 will add the parity gate that ties this count to
    /// `describe_metrics()`; for now we just confirm the link-section
    /// machinery works.
    #[test]
    fn distributed_slice_collects_all_markers() {
        let descriptors = iter_descriptors();
        assert_eq!(
            descriptors.len(),
            19,
            "expected 19 metric descriptors in catalog, got {}",
            descriptors.len()
        );
    }

    /// Names must be unique — duplicate registrations would silently
    /// overwrite Prometheus HELP/TYPE lines.
    #[test]
    fn descriptor_names_are_unique() {
        let descriptors = iter_descriptors();
        let mut names: Vec<&str> = descriptors.iter().map(|d| d.name).collect();
        names.sort_unstable();
        let original_len = names.len();
        names.dedup();
        assert_eq!(
            names.len(),
            original_len,
            "duplicate metric name in catalog"
        );
    }
}
