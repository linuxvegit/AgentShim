//! Single source of truth for every metric name AgentShim emits.
//!
//! All names use the `agent_shim_` prefix. Suffixes follow Prometheus
//! conventions: `_total` for counters, `_seconds` for time histograms,
//! `_bytes` for byte histograms.
//!
//! Spec §3.1.

// --- Request lifecycle ---
pub const REQUESTS_TOTAL: &str = "agent_shim_requests_total";
pub const REQUEST_DURATION_SECONDS: &str = "agent_shim_request_duration_seconds";
pub const REQUEST_BODY_BYTES: &str = "agent_shim_request_body_bytes";
pub const IN_FLIGHT_REQUESTS: &str = "agent_shim_in_flight_requests";

// --- Resilience layer (mirrors v0.4 tracing taxonomy) ---
pub const RETRY_ATTEMPTS_TOTAL: &str = "agent_shim_retry_attempts_total";
pub const RETRY_EXHAUSTED_TOTAL: &str = "agent_shim_retry_exhausted_total";
pub const FALLBACK_TRANSITIONS_TOTAL: &str = "agent_shim_fallback_transitions_total";
pub const BREAKER_STATE_CHANGES_TOTAL: &str = "agent_shim_breaker_state_changes_total";
pub const RATE_LIMIT_REJECTED_TOTAL: &str = "agent_shim_rate_limit_rejected_total";

// --- Upstream call ---
pub const UPSTREAM_DURATION_SECONDS: &str = "agent_shim_upstream_duration_seconds";
pub const UPSTREAM_ERRORS_TOTAL: &str = "agent_shim_upstream_errors_total";

// --- Token accounting ---
pub const TOKENS_INPUT_TOTAL: &str = "agent_shim_tokens_input_total";
pub const TOKENS_OUTPUT_TOTAL: &str = "agent_shim_tokens_output_total";

// --- Reload (used by Plan 04) ---
pub const CONFIG_RELOADS_TOTAL: &str = "agent_shim_config_reloads_total";

// --- Cost-aware routing (Phase 6 P04) ---
/// Plan 06 P04 T4: per-axis cost-filter skip/note counter.
/// Labels: reason ∈ {tier, latency, cap, latency_unknown, tiktoken_fallback},
///         upstream, route.
pub const COST_FILTERED_TOTAL: &str = "agent_shim_cost_filtered_total";

#[cfg(test)]
mod tests {
    use super::*;

    /// Every constant must hold a distinct value so we can't accidentally
    /// shadow one metric with another.
    #[test]
    fn all_unique() {
        let all = [
            REQUESTS_TOTAL,
            REQUEST_DURATION_SECONDS,
            REQUEST_BODY_BYTES,
            IN_FLIGHT_REQUESTS,
            RETRY_ATTEMPTS_TOTAL,
            RETRY_EXHAUSTED_TOTAL,
            FALLBACK_TRANSITIONS_TOTAL,
            BREAKER_STATE_CHANGES_TOTAL,
            RATE_LIMIT_REJECTED_TOTAL,
            UPSTREAM_DURATION_SECONDS,
            UPSTREAM_ERRORS_TOTAL,
            TOKENS_INPUT_TOTAL,
            TOKENS_OUTPUT_TOTAL,
            CONFIG_RELOADS_TOTAL,
            COST_FILTERED_TOTAL,
        ];
        let set: std::collections::HashSet<&str> = all.iter().copied().collect();
        assert_eq!(set.len(), all.len(), "duplicate metric name");
    }

    /// All names use the agent_shim_ prefix.
    #[test]
    fn all_prefixed() {
        let all = [
            REQUESTS_TOTAL,
            REQUEST_DURATION_SECONDS,
            REQUEST_BODY_BYTES,
            IN_FLIGHT_REQUESTS,
            RETRY_ATTEMPTS_TOTAL,
            RETRY_EXHAUSTED_TOTAL,
            FALLBACK_TRANSITIONS_TOTAL,
            BREAKER_STATE_CHANGES_TOTAL,
            RATE_LIMIT_REJECTED_TOTAL,
            UPSTREAM_DURATION_SECONDS,
            UPSTREAM_ERRORS_TOTAL,
            TOKENS_INPUT_TOTAL,
            TOKENS_OUTPUT_TOTAL,
            CONFIG_RELOADS_TOTAL,
            COST_FILTERED_TOTAL,
        ];
        for name in all {
            assert!(name.starts_with("agent_shim_"), "{name} missing prefix");
        }
    }
}
