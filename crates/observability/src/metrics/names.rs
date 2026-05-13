//! Single source of truth for every metric name AgentShim emits.
//!
//! All names use the `agent_shim_` prefix. Suffixes follow Prometheus
//! conventions: `_total` for counters, `_seconds` for time histograms,
//! `_bytes` for byte histograms.
//!
//! Backwards-compatibility module: each `pub const NAME` here delegates
//! to the corresponding marker struct's `NAME` const in `catalog`. New
//! metrics should declare a marker struct in `catalog.rs` and either
//! call sites use `MarkerStruct::NAME` directly, OR a new `pub const`
//! is added here for symmetry. Plan v0.6.1 P02 (M-6).

use super::catalog::*;

// --- Request lifecycle ---
pub const REQUESTS_TOTAL: &str = RequestsTotal::NAME;
pub const REQUEST_DURATION_SECONDS: &str = RequestDurationSeconds::NAME;
pub const REQUEST_BODY_BYTES: &str = RequestBodyBytes::NAME;
pub const IN_FLIGHT_REQUESTS: &str = InFlightRequests::NAME;

// --- Resilience layer (mirrors v0.4 tracing taxonomy) ---
pub const RETRY_ATTEMPTS_TOTAL: &str = RetryAttemptsTotal::NAME;
pub const RETRY_EXHAUSTED_TOTAL: &str = RetryExhaustedTotal::NAME;
pub const FALLBACK_TRANSITIONS_TOTAL: &str = FallbackTransitionsTotal::NAME;
pub const BREAKER_STATE_CHANGES_TOTAL: &str = BreakerStateChangesTotal::NAME;
pub const RATE_LIMIT_REJECTED_TOTAL: &str = RateLimitRejectedTotal::NAME;

// --- Upstream call ---
pub const UPSTREAM_DURATION_SECONDS: &str = UpstreamDurationSeconds::NAME;
pub const UPSTREAM_ERRORS_TOTAL: &str = UpstreamErrorsTotal::NAME;

// --- Token accounting ---
pub const TOKENS_INPUT_TOTAL: &str = TokensInputTotal::NAME;
pub const TOKENS_OUTPUT_TOTAL: &str = TokensOutputTotal::NAME;

// --- Reload (used by Plan 04) ---
pub const CONFIG_RELOADS_TOTAL: &str = ConfigReloadsTotal::NAME;

// --- Cost-aware routing (Phase 6 P04) ---
pub const COST_FILTERED_TOTAL: &str = CostFilteredTotal::NAME;

#[cfg(test)]
mod tests {
    use super::super::catalog;

    /// Every catalog entry must have a distinct name. Replaces the v0.6.0
    /// hand-maintained `all_unique` array — now a single loop over the
    /// catalog. M-6 closure.
    #[test]
    fn all_unique() {
        let descriptors = catalog::iter_descriptors();
        let set: std::collections::HashSet<&str> = descriptors.iter().map(|d| d.name).collect();
        assert_eq!(
            set.len(),
            descriptors.len(),
            "duplicate metric name detected"
        );
    }

    /// Every catalog entry must use the `agent_shim_` prefix. Replaces
    /// the v0.6.0 hand-maintained `all_prefixed` array — now a single
    /// loop over the catalog. M-6 closure.
    #[test]
    fn all_prefixed() {
        for d in catalog::iter_descriptors() {
            assert!(
                d.name.starts_with("agent_shim_"),
                "{} missing agent_shim_ prefix",
                d.name
            );
        }
    }
}
