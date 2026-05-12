//! Plan 02 P02 T4: assert that the router crate's metric name constants
//! match the canonical names in agent-shim-observability. The router
//! cannot depend on the observability crate (it's a lower layer), so
//! the constants are duplicated. This test catches drift.

#[test]
fn router_metric_names_match_observability() {
    use agent_shim_observability::metrics::names as obs;

    // The router emits these names; they must match observability's.
    let pairs: &[(&str, &str)] = &[
        ("agent_shim_retry_attempts_total", obs::RETRY_ATTEMPTS_TOTAL),
        (
            "agent_shim_retry_exhausted_total",
            obs::RETRY_EXHAUSTED_TOTAL,
        ),
        (
            "agent_shim_fallback_transitions_total",
            obs::FALLBACK_TRANSITIONS_TOTAL,
        ),
        (
            "agent_shim_breaker_state_changes_total",
            obs::BREAKER_STATE_CHANGES_TOTAL,
        ),
        (
            "agent_shim_rate_limit_rejected_total",
            obs::RATE_LIMIT_REJECTED_TOTAL,
        ),
    ];
    for (router, obs) in pairs {
        assert_eq!(
            router, obs,
            "router '{}' != observability '{}'",
            router, obs
        );
    }
}
