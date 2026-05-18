//! Typed wrappers over the `metrics-rs` macros that enforce label sets
//! at compile time. Callsites should prefer these helpers over raw
//! `counter!()` / `histogram!()` so renaming a label is one diff in
//! one file.

use crate::metrics::names;

/// Status class of an HTTP response.
#[derive(Debug, Clone, Copy)]
pub enum StatusClass {
    Success,   // 2xx
    ClientErr, // 4xx
    ServerErr, // 5xx
    Cancelled, // client disconnect
}

impl StatusClass {
    pub fn label(self) -> &'static str {
        match self {
            Self::Success => "2xx",
            Self::ClientErr => "4xx",
            Self::ServerErr => "5xx",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn from_status(status: u16) -> Self {
        match status {
            200..=299 => Self::Success,
            400..=499 => Self::ClientErr,
            500..=599 => Self::ServerErr,
            _ => Self::ClientErr,
        }
    }
}

pub fn record_request(
    frontend: &'static str,
    route: &str,
    status: StatusClass,
    dur_secs: f64,
    body_bytes: usize,
) {
    metrics::counter!(
        names::REQUESTS_TOTAL,
        "frontend" => frontend,
        "route" => route.to_string(),
        "status_class" => status.label(),
    )
    .increment(1);
    metrics::histogram!(
        names::REQUEST_DURATION_SECONDS,
        "frontend" => frontend,
        "route" => route.to_string(),
        "status_class" => status.label(),
    )
    .record(dur_secs);
    metrics::histogram!(
        names::REQUEST_BODY_BYTES,
        "frontend" => frontend,
        "route" => route.to_string(),
    )
    .record(body_bytes as f64);
}

pub fn record_retry_attempt(route: &str, upstream: &str, attempt: u32) {
    metrics::counter!(
        names::RETRY_ATTEMPTS_TOTAL,
        "route" => route.to_string(),
        "upstream" => upstream.to_string(),
        "attempt" => attempt.to_string(),
    )
    .increment(1);
}

pub fn record_retry_exhausted(route: &str, upstream: &str) {
    metrics::counter!(
        names::RETRY_EXHAUSTED_TOTAL,
        "route" => route.to_string(),
        "upstream" => upstream.to_string(),
    )
    .increment(1);
}

pub fn record_fallback_transition(route: &str, from: &str, to: &str) {
    metrics::counter!(
        names::FALLBACK_TRANSITIONS_TOTAL,
        "route" => route.to_string(),
        "from_upstream" => from.to_string(),
        "to_upstream" => to.to_string(),
    )
    .increment(1);
}

pub fn record_breaker_state_change(upstream: &str, model: &str, from: &str, to: &str) {
    metrics::counter!(
        names::BREAKER_STATE_CHANGES_TOTAL,
        "upstream" => upstream.to_string(),
        "model" => model.to_string(),
        "from" => from.to_string(),
        "to" => to.to_string(),
    )
    .increment(1);
}

pub fn record_rate_limit_rejected(dimension: &'static str) {
    metrics::counter!(
        names::RATE_LIMIT_REJECTED_TOTAL,
        "dimension" => dimension,
    )
    .increment(1);
}

pub fn record_upstream_call(upstream: &str, model: &str, status: StatusClass, dur_secs: f64) {
    metrics::histogram!(
        names::UPSTREAM_DURATION_SECONDS,
        "upstream" => upstream.to_string(),
        "model" => model.to_string(),
        "status_class" => status.label(),
    )
    .record(dur_secs);
}

pub fn record_upstream_error(upstream: &str, model: &str, error_class: &'static str) {
    metrics::counter!(
        names::UPSTREAM_ERRORS_TOTAL,
        "upstream" => upstream.to_string(),
        "model" => model.to_string(),
        "error_class" => error_class,
    )
    .increment(1);
}

pub fn record_config_reload(result: &'static str) {
    metrics::counter!(names::CONFIG_RELOADS_TOTAL, "result" => result).increment(1);
}

/// Record one plugin hook invocation. Counter increments by 1; histogram
/// records `duration_secs`. §7.4 of the plugin design spec.
///
/// `plugin_kind` and `hook` are `&'static str` (zero-alloc). `plugin_name`
/// is `&str`, allocated via `.to_string()` per label per call (consistent
/// with `record_request` / `record_retry_attempt` pattern). For the H5
/// hook this is on the hot path (~2000 alloc / streaming response in the
/// worst case), but small-string allocator overhead is < 10 us total on
/// a multi-100ms LLM stream — acceptable. Profile and refactor to
/// `Cow<'a, str>` if benchmarks ever justify.
pub fn record_plugin_invocation(
    plugin_kind: &'static str,
    plugin_name: &str,
    hook: &'static str,
    outcome: &'static str,
    duration_secs: f64,
) {
    metrics::counter!(
        names::PLUGIN_INVOCATIONS_TOTAL,
        "plugin_kind" => plugin_kind,
        "plugin_name" => plugin_name.to_string(),
        "hook" => hook,
        "outcome" => outcome,
    )
    .increment(1);
    metrics::histogram!(
        names::PLUGIN_DURATION_SECONDS,
        "plugin_kind" => plugin_kind,
        "plugin_name" => plugin_name.to_string(),
        "hook" => hook,
    )
    .record(duration_secs);
}

/// Record N H7 tasks dropped at shutdown for a given plugin. Called once
/// per plugin_name by the gateway shutdown hook with the aggregated count
/// from `PluginSupervisor::flush_pending_h7`.
pub fn record_h7_dropped(plugin_name: &str, count: u64) {
    metrics::counter!(
        names::PLUGIN_H7_DROPPED_TOTAL,
        "plugin_name" => plugin_name.to_string(),
    )
    .increment(count);
}
