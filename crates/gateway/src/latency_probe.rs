//! Prometheus-backed latency probe. Plan 06 P04 T5.
//!
//! Reads the recent p95 of `agent_shim_upstream_duration_seconds` from
//! the metrics-exporter-prometheus snapshot handle and converts it to
//! milliseconds. Used by `ResilientCaller`'s cost filter when evaluating
//! per-upstream `p95_latency_budget_ms`.
//!
//! The implementation parses the text snapshot via `prometheus-parse`.
//! For a tighter integration (avoiding text parse) the alternative is to
//! clone the underlying recorder and snapshot its histogram registry
//! directly. The text-parse approach keeps this implementation isolated
//! from `metrics-rs` internals and is fast enough — a single scrape
//! body is on the order of 10KB and parses in well under a millisecond.

use std::sync::Arc;

use agent_shim_router::LatencyProbe;

/// `LatencyProbe` impl that derives p95 latency from the gateway's
/// live Prometheus exposition. Held inside `AppCore` and handed to
/// `ResilientCaller::new` at startup.
pub struct PrometheusLatencyProbe {
    handle: Arc<agent_shim_observability::MetricsHandle>,
}

impl PrometheusLatencyProbe {
    pub fn new(handle: Arc<agent_shim_observability::MetricsHandle>) -> Self {
        Self { handle }
    }
}

impl LatencyProbe for PrometheusLatencyProbe {
    fn recent_p95_ms(&self, upstream: &str) -> Option<u64> {
        let body = self.handle.render();
        compute_p95_from_scrape(&body, upstream)
    }
}

/// Compute the p95 of `agent_shim_upstream_duration_seconds` for the
/// requested upstream from a Prometheus text snapshot.
///
/// Returns `None` if no buckets exist for the upstream (e.g. it hasn't
/// served traffic yet — warm-up period) or if total observations are
/// zero. Operators read `None` as "no signal; defer to the cost
/// filter's `LatencyUnknown` note".
fn compute_p95_from_scrape(body: &str, upstream: &str) -> Option<u64> {
    use prometheus_parse::{Scrape, Value};
    let scrape = Scrape::parse(body.lines().map(|l| Ok(l.to_string()))).ok()?;
    // Collect bucket entries for this upstream's duration histogram.
    let mut buckets: Vec<(f64, u64)> = scrape
        .samples
        .iter()
        .filter(|s| s.metric == "agent_shim_upstream_duration_seconds_bucket")
        .filter(|s| s.labels.get("upstream").is_some_and(|v| v == upstream))
        .filter_map(|s| {
            let le = s.labels.get("le")?.parse::<f64>().ok()?;
            let v = match s.value {
                Value::Counter(c) | Value::Untyped(c) => c as u64,
                _ => return None,
            };
            Some((le, v))
        })
        .collect();
    if buckets.is_empty() {
        return None;
    }
    buckets.sort_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap());
    // The +Inf bucket count = total observations.
    let total = buckets.last()?.1;
    if total == 0 {
        return None;
    }
    let target = (0.95_f64 * total as f64).ceil() as u64;
    let p95 = buckets
        .iter()
        .find(|(_, count)| *count >= target)
        .map(|(le, _)| le)?;
    // p95 is in seconds; convert to ms.
    Some((p95 * 1000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_p95_from_simple_scrape() {
        let scrape = "\
agent_shim_upstream_duration_seconds_bucket{upstream=\"m\",le=\"0.1\"} 5
agent_shim_upstream_duration_seconds_bucket{upstream=\"m\",le=\"0.5\"} 10
agent_shim_upstream_duration_seconds_bucket{upstream=\"m\",le=\"1.0\"} 20
agent_shim_upstream_duration_seconds_bucket{upstream=\"m\",le=\"+Inf\"} 20
";
        let p95 = compute_p95_from_scrape(scrape, "m");
        assert_eq!(p95, Some(1000));
    }

    #[test]
    fn missing_upstream_returns_none() {
        let scrape = "agent_shim_other_metric 1\n";
        assert_eq!(compute_p95_from_scrape(scrape, "m"), None);
    }
}
