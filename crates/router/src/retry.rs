//! Retry policy and exponential-backoff math (Plan 04 P01).
//!
//! `compute_backoff` is pure (deterministic given a seeded RNG) so it can be
//! property-tested. The retry loop itself lives in `retry_with_policy`, which
//! drives a `BackendProvider::complete()` call up to `policy.max_attempts`
//! times within `policy.total_budget_ms`.

use std::time::Duration;

use rand::Rng;

/// Effective per-route retry policy, derived from `agent_shim_config::RetryConfig`.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_backoff_ms: u64,
    pub multiplier: f64,
    pub jitter_pct: f64,
    pub total_budget_ms: u64,
    pub retry_on: Vec<String>,
}

impl From<&agent_shim_config::RetryConfig> for RetryPolicy {
    fn from(c: &agent_shim_config::RetryConfig) -> Self {
        Self {
            max_attempts: c.max_attempts,
            initial_backoff_ms: c.initial_backoff_ms,
            multiplier: c.multiplier,
            jitter_pct: c.jitter_pct,
            total_budget_ms: c.total_budget_ms,
            retry_on: c.retry_on.clone(),
        }
    }
}

/// Compute the backoff duration for a 1-indexed retry attempt under the
/// configured policy, applying jitter via the supplied RNG. Pure function;
/// deterministic given the same `rng` state.
///
/// Output is bounded to `[base × (1 - jitter%/100), base × (1 + jitter%/100)]`
/// where `base = initial_backoff_ms × multiplier^(attempt-1)`.
pub fn compute_backoff<R: Rng>(attempt: u32, policy: &RetryPolicy, rng: &mut R) -> Duration {
    debug_assert!(attempt >= 1, "attempt is 1-indexed");
    let exponent = (attempt - 1) as i32;
    let base_ms = policy.initial_backoff_ms as f64 * policy.multiplier.powi(exponent);
    let jitter_factor = if policy.jitter_pct == 0.0 {
        1.0
    } else {
        let span = policy.jitter_pct / 100.0;
        1.0 + rng.gen_range(-span..=span)
    };
    let final_ms = (base_ms * jitter_factor).round().max(0.0) as u64;
    Duration::from_millis(final_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rand::rngs::SmallRng;
    use rand::SeedableRng;

    fn default_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 2,
            initial_backoff_ms: 100,
            multiplier: 2.0,
            jitter_pct: 25.0,
            total_budget_ms: 5000,
            retry_on: vec![
                "network".into(),
                "upstream_5xx".into(),
                "upstream_429".into(),
            ],
        }
    }

    #[test]
    fn compute_backoff_with_zero_jitter_is_deterministic() {
        let policy = RetryPolicy {
            jitter_pct: 0.0,
            ..default_policy()
        };
        let mut rng = SmallRng::seed_from_u64(42);
        // attempt=1 → 100 × 2^0 = 100ms
        // attempt=2 → 100 × 2^1 = 200ms
        // attempt=3 → 100 × 2^2 = 400ms
        assert_eq!(
            compute_backoff(1, &policy, &mut rng),
            Duration::from_millis(100)
        );
        assert_eq!(
            compute_backoff(2, &policy, &mut rng),
            Duration::from_millis(200)
        );
        assert_eq!(
            compute_backoff(3, &policy, &mut rng),
            Duration::from_millis(400)
        );
    }

    proptest! {
        #[test]
        fn compute_backoff_jitter_stays_within_bounds(
            attempt in 1u32..=10u32,
            seed in any::<u64>(),
        ) {
            let policy = default_policy();
            let mut rng = SmallRng::seed_from_u64(seed);
            let dur = compute_backoff(attempt, &policy, &mut rng);
            let base_ms = policy.initial_backoff_ms as f64
                * policy.multiplier.powi((attempt - 1) as i32);
            let lo = (base_ms * (1.0 - policy.jitter_pct / 100.0)).floor() as u64;
            let hi = (base_ms * (1.0 + policy.jitter_pct / 100.0)).ceil() as u64;
            let got = dur.as_millis() as u64;
            prop_assert!(
                got >= lo && got <= hi,
                "attempt={attempt} got={got}ms expected [{lo}, {hi}]"
            );
        }
    }
}
