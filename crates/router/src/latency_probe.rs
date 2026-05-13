//! Latency probe trait + test mock. Plan 06 P04 T1.
//!
//! Cost-aware routing's latency axis asks: "what's the recent p95
//! latency for upstream X?" The probe abstracts the data source so
//! router can stay independent of observability (boundary rule from
//! v0.5 D11). Gateway provides the Prometheus-backed implementation
//! that reads the `agent_shim_upstream_duration_seconds` histogram.

use std::collections::HashMap;
use std::sync::Mutex;

/// Source of recent p95 latency data per upstream. The router uses
/// this to decide whether an upstream's `p95_latency_budget_ms` is
/// being met. `None` means "no sample data yet" — the cost filter
/// treats that as "let it through".
pub trait LatencyProbe: Send + Sync {
    /// Recent p95 latency for `upstream` in milliseconds, or `None` if
    /// no samples are available.
    fn recent_p95_ms(&self, upstream: &str) -> Option<u64>;
}

/// Test-only mock with hard-coded per-upstream values.
#[derive(Default)]
pub struct MockLatencyProbe {
    values: Mutex<HashMap<String, u64>>,
}

impl MockLatencyProbe {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(values: impl IntoIterator<Item = (&'static str, u64)>) -> Self {
        let m = values
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        Self {
            values: Mutex::new(m),
        }
    }

    pub fn set(&self, upstream: &str, ms: u64) {
        self.values.lock().unwrap().insert(upstream.to_string(), ms);
    }
}

impl LatencyProbe for MockLatencyProbe {
    fn recent_p95_ms(&self, upstream: &str) -> Option<u64> {
        self.values.lock().unwrap().get(upstream).copied()
    }
}

/// Always-`None` probe — every query returns `None`. Useful as the
/// default in code paths that don't need latency filtering (e.g. unit
/// tests for unrelated resilience behaviour, or operators who set no
/// `p95_latency_budget_ms` anywhere).
pub struct DisabledLatencyProbe;

impl LatencyProbe for DisabledLatencyProbe {
    fn recent_p95_ms(&self, _: &str) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_returns_set_values() {
        let p = MockLatencyProbe::with([("a", 100), ("b", 500)]);
        assert_eq!(p.recent_p95_ms("a"), Some(100));
        assert_eq!(p.recent_p95_ms("b"), Some(500));
        assert_eq!(p.recent_p95_ms("c"), None);
    }

    #[test]
    fn disabled_always_returns_none() {
        let p = DisabledLatencyProbe;
        assert_eq!(p.recent_p95_ms("anything"), None);
    }
}
