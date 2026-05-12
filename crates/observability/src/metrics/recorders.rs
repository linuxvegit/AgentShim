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
