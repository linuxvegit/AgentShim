//! Retry policy and exponential-backoff math (Plan 04 P01).
//!
//! `compute_backoff` is pure (deterministic given a seeded RNG) so it can be
//! property-tested. The retry loop itself lives in [`retry_with_policy`],
//! which drives a `BackendProvider::complete()` call up to
//! `policy.max_attempts` times within `policy.total_budget_ms`.

use std::sync::Arc;
use std::time::Duration;

use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream};
use agent_shim_providers::{BackendProvider, ProviderError};
use rand::rngs::SmallRng;
use rand::Rng;
use rand::SeedableRng;

use crate::fallback::{fallback_eligibility_with_overrides, FallbackEligibility};

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
    assert!(attempt >= 1, "attempt is 1-indexed");
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

/// Drive a single `provider.complete()` call under the supplied retry policy.
///
/// On success: returns Ok(stream) on the first attempt that succeeds.
/// On retryable error: sleeps `compute_backoff(attempt, ...)`, increments
/// the attempt counter, and tries again — until `max_attempts` is reached
/// OR the next backoff would exceed `total_budget_ms`.
/// On terminal error (per `retry_on` override of the default classifier):
/// returns the error immediately without further attempts.
///
/// This helper is **single-upstream**. Plan 02's `ResilientCaller` walks
/// the chain across multiple upstreams.
pub async fn retry_with_policy(
    provider: Arc<dyn BackendProvider>,
    target: BackendTarget,
    req: CanonicalRequest,
    policy: &RetryPolicy,
) -> Result<CanonicalStream, ProviderError> {
    let mut rng = SmallRng::from_entropy();
    let mut attempt: u32 = 1;
    let mut total_elapsed_ms: u64 = 0;
    // The initial `None` is overwritten before being read on every code path
    // that breaks the loop; the `expect` after the loop captures the invariant.
    #[allow(unused_assignments)]
    let mut last_err: Option<ProviderError> = None;

    loop {
        let result = provider.complete(req.clone(), target.clone()).await;
        match result {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                let eligibility = fallback_eligibility_with_overrides(&e, &policy.retry_on);
                if eligibility == FallbackEligibility::Terminal {
                    return Err(e);
                }
                last_err = Some(e);
                if attempt >= policy.max_attempts {
                    break;
                }
                let backoff = compute_backoff(attempt, policy, &mut rng);
                let backoff_ms = backoff.as_millis() as u64;
                if total_elapsed_ms + backoff_ms > policy.total_budget_ms {
                    break;
                }
                tracing::info!(
                    attempt,
                    backoff_ms,
                    total_elapsed_ms,
                    "retrying provider call after eligible error"
                );
                tokio::time::sleep(backoff).await;
                total_elapsed_ms += backoff_ms;
                attempt += 1;
            }
        }
    }

    Err(last_err.expect("loop only breaks after recording an error"))
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

    /// Pin actual jitter variation: the property test only confirms outputs
    /// stay in `[lo, hi]`. A buggy implementation that always returned `base_ms`
    /// (no jitter applied) would still pass the proptest. This guards against
    /// that regression by demanding observable variation across seeds.
    #[test]
    fn compute_backoff_jitter_actually_varies_output() {
        use std::collections::HashSet;
        let policy = default_policy(); // jitter_pct = 25.0
        let outputs: HashSet<u64> = (0..16u64)
            .map(|s| {
                compute_backoff(3, &policy, &mut SmallRng::seed_from_u64(s)).as_millis() as u64
            })
            .collect();
        assert!(
            outputs.len() > 1,
            "jitter produced identical results across 16 seeds: {outputs:?}"
        );
    }

    // ---------------------------------------------------------------------
    // retry_with_policy tests (P01 T4)
    // ---------------------------------------------------------------------
    //
    // `ProviderError` does not derive `Clone` (and `crates/providers/` is
    // frozen for Phase 4 P01), so the mock can't store pre-built error
    // values. Instead we script outcomes as a clone-able shadow enum
    // (`MockOutcome`) and rebuild the `ProviderError` on each call.

    use agent_shim_core::{
        request::RequestMetadata, ContentBlock, ExtensionMap, FrontendInfo, FrontendKind,
        FrontendModel, GenerationOptions, Message, RequestId, ResolvedPolicy,
    };
    use agent_shim_providers::{BackendProvider, ProviderCapabilities, ProviderError};
    use async_trait::async_trait;
    use futures::stream;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Clone)]
    enum MockOutcome {
        Ok,
        Upstream { status: u16, body: &'static str },
    }

    impl MockOutcome {
        fn upstream(status: u16, body: &'static str) -> Self {
            MockOutcome::Upstream { status, body }
        }

        fn into_result(self) -> Result<CanonicalStream, ProviderError> {
            match self {
                MockOutcome::Ok => Ok(Box::pin(stream::iter(vec![]))),
                MockOutcome::Upstream { status, body } => Err(ProviderError::Upstream {
                    status,
                    body: body.to_string(),
                }),
            }
        }
    }

    /// Mock provider that returns scripted outcomes in order, recording the
    /// total call count via an atomic cursor.
    struct MockProvider {
        results: Arc<Vec<MockOutcome>>,
        cursor: Arc<AtomicUsize>,
        capabilities: ProviderCapabilities,
    }

    impl MockProvider {
        fn new(scripted: Vec<MockOutcome>) -> Self {
            Self {
                results: Arc::new(scripted),
                cursor: Arc::new(AtomicUsize::new(0)),
                capabilities: ProviderCapabilities {
                    streaming: true,
                    tool_use: false,
                    vision: false,
                    json_mode: false,
                },
            }
        }

        fn calls(&self) -> usize {
            self.cursor.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl BackendProvider for MockProvider {
        fn name(&self) -> &'static str {
            "mock"
        }
        fn capabilities(&self) -> &ProviderCapabilities {
            &self.capabilities
        }
        async fn complete(
            &self,
            _req: CanonicalRequest,
            _target: BackendTarget,
        ) -> Result<CanonicalStream, ProviderError> {
            let idx = self.cursor.fetch_add(1, Ordering::SeqCst);
            let outcome = self
                .results
                .get(idx)
                .cloned()
                .ok_or_else(|| ProviderError::Network("mock script exhausted".into()))?;
            outcome.into_result()
        }
    }

    fn dummy_request() -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::OpenAiChat,
                requested_model: FrontendModel::from("gpt-4o"),
            },
            model: FrontendModel::from("gpt-4o"),
            system: vec![],
            messages: vec![Message::user(vec![ContentBlock::text("hi")])],
            tools: vec![],
            tool_choice: Default::default(),
            generation: GenerationOptions::default(),
            response_format: None,
            stream: false,
            metadata: RequestMetadata::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: ResolvedPolicy::default(),
            extensions: ExtensionMap::new(),
        }
    }

    fn dummy_target() -> BackendTarget {
        BackendTarget {
            provider: "mock".into(),
            model: "gpt-4o".into(),
            policy: Default::default(),
        }
    }

    fn fast_policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            initial_backoff_ms: 1, // tests must be fast
            multiplier: 2.0,
            jitter_pct: 0.0, // deterministic
            total_budget_ms: 1000,
            retry_on: vec![
                "network".into(),
                "upstream_5xx".into(),
                "upstream_429".into(),
            ],
        }
    }

    #[tokio::test]
    async fn retry_with_policy_succeeds_on_first_attempt() {
        let provider = Arc::new(MockProvider::new(vec![MockOutcome::Ok]));
        let result = retry_with_policy(
            provider.clone(),
            dummy_target(),
            dummy_request(),
            &fast_policy(3),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(provider.calls(), 1);
    }

    #[tokio::test]
    async fn retry_with_policy_retries_eligible_error_then_succeeds() {
        let provider = Arc::new(MockProvider::new(vec![
            MockOutcome::upstream(502, "bad"),
            MockOutcome::Ok,
        ]));
        let result = retry_with_policy(
            provider.clone(),
            dummy_target(),
            dummy_request(),
            &fast_policy(3),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(provider.calls(), 2);
    }

    #[tokio::test]
    async fn retry_with_policy_does_not_retry_terminal_error() {
        let provider = Arc::new(MockProvider::new(vec![
            MockOutcome::upstream(401, "auth"),
            MockOutcome::Ok, // would-be "never reached"
        ]));
        let result = retry_with_policy(
            provider.clone(),
            dummy_target(),
            dummy_request(),
            &fast_policy(3),
        )
        .await;
        assert!(matches!(
            result,
            Err(ProviderError::Upstream { status: 401, .. })
        ));
        assert_eq!(provider.calls(), 1);
    }

    #[tokio::test]
    async fn retry_with_policy_exhausts_max_attempts() {
        let provider = Arc::new(MockProvider::new(vec![
            MockOutcome::upstream(502, "bad"),
            MockOutcome::upstream(502, "bad"),
            MockOutcome::upstream(502, "bad"),
        ]));
        let result = retry_with_policy(
            provider.clone(),
            dummy_target(),
            dummy_request(),
            &fast_policy(3),
        )
        .await;
        assert!(matches!(
            result,
            Err(ProviderError::Upstream { status: 502, .. })
        ));
        assert_eq!(provider.calls(), 3);
    }

    #[tokio::test]
    async fn retry_with_policy_respects_total_budget() {
        // Budget 50ms, initial backoff 100ms → second attempt's sleep would
        // overshoot; should give up after the first failure (1 call).
        let policy = RetryPolicy {
            max_attempts: 5,
            initial_backoff_ms: 100,
            multiplier: 2.0,
            jitter_pct: 0.0,
            total_budget_ms: 50,
            retry_on: vec!["upstream_5xx".into()],
        };
        let provider = Arc::new(MockProvider::new(vec![
            MockOutcome::upstream(502, "bad"),
            MockOutcome::upstream(502, "bad"),
        ]));
        let result =
            retry_with_policy(provider.clone(), dummy_target(), dummy_request(), &policy).await;
        assert!(matches!(
            result,
            Err(ProviderError::Upstream { status: 502, .. })
        ));
        assert_eq!(provider.calls(), 1);
    }
}
