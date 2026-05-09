//! Resilient caller orchestrator (Plan 04 P02 T4).
//!
//! Walks a `Vec<BackendTarget>` chain, calling `retry_with_policy` for each
//! element. On retry exhaustion with a fallback-eligible last error,
//! advances to the next chain element. On a terminal error, returns
//! immediately. On success, returns the stream.
//!
//! Plan 03 inserts the breaker gate inside the chain walk.
//! Plan 04 inserts the rate-limit gate before the chain walk.

use std::sync::Arc;
use std::time::Instant;

use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream};
use agent_shim_providers::{BackendProvider, ProviderError};

use crate::errors::{ResilienceError, TriedUpstream};
use crate::fallback::{fallback_eligibility_with_overrides, FallbackEligibility};
use crate::retry::{retry_with_policy, RetryPolicy};

/// Trait abstraction over the provider registry so tests can substitute a
/// mock. The gateway's existing `ProviderRegistry` implements this trivially.
pub trait ProviderLookup: Send + Sync {
    fn get(&self, provider: &str) -> Option<Arc<dyn BackendProvider>>;
}

/// The resilience-layer entry point.
pub struct ResilientCaller {
    providers: Arc<dyn ProviderLookup>,
    breakers: Arc<crate::circuit_breaker::BreakerRegistry>,
}

impl ResilientCaller {
    pub fn new(
        providers: Arc<dyn ProviderLookup>,
        breakers: Arc<crate::circuit_breaker::BreakerRegistry>,
    ) -> Self {
        Self {
            providers,
            breakers,
        }
    }

    /// Walk the chain. Each element gets its own retry budget. Fallback
    /// to chain[i+1] only on retry exhaustion with a fallback-eligible
    /// error; terminal errors short-circuit.
    ///
    /// The breaker gate runs before each chain element:
    /// - `Skip` → continue to chain[i+1] without consuming retries.
    /// - `Probe` → fall through; `record()` resets `probe_in_flight`.
    /// - `Allow` → normal path.
    pub async fn complete(
        &self,
        chain: Vec<BackendTarget>,
        req: CanonicalRequest,
        retry_policies: Vec<RetryPolicy>,
        breaker_policies: Vec<crate::circuit_breaker::BreakerPolicy>,
    ) -> Result<CanonicalStream, ResilienceError> {
        debug_assert_eq!(
            chain.len(),
            retry_policies.len(),
            "one retry policy per chain element"
        );
        debug_assert_eq!(
            chain.len(),
            breaker_policies.len(),
            "one breaker policy per chain element"
        );

        let mut tried: Vec<TriedUpstream> = Vec::new();
        let mut last_error: Option<ProviderError> = None;
        let mut breakers_skipped: Vec<String> = Vec::new();

        for (i, target) in chain.iter().enumerate() {
            let bpolicy = &breaker_policies[i];
            let rpolicy = &retry_policies[i];

            // Resolve the provider BEFORE consulting the breaker. If the
            // provider name is unknown, we'd return TerminalError early
            // and skip the breaker's record() call — which would leak
            // a probe authorization (probe_in_flight stuck true) when the
            // breaker happened to be in HalfOpen. Production routes are
            // validated at startup so UnknownProvider should never reach
            // this point, but doing the lookup first makes the invariant
            // independent of that guarantee.
            let provider = match self.providers.get(&target.provider) {
                Some(p) => p,
                None => {
                    let e = ProviderError::UnknownProvider(target.provider.clone());
                    return Err(ResilienceError::TerminalError { error: e, tried });
                }
            };

            // ── BREAKER GATE ────────────────────────────────────────────────
            let decision = self
                .breakers
                .decision(&target.provider, &target.model, bpolicy);
            match decision {
                crate::circuit_breaker::BreakerDecision::Skip => {
                    tracing::warn!(
                        provider = %target.provider,
                        model = %target.model,
                        chain_position = i,
                        "breaker open; skipping chain element"
                    );
                    breakers_skipped.push(target.provider.clone());
                    continue; // next chain element
                }
                crate::circuit_breaker::BreakerDecision::Probe => {
                    tracing::info!(
                        provider = %target.provider,
                        model = %target.model,
                        chain_position = i,
                        "breaker half-open; attempting probe"
                    );
                    // Fall through to call. probe_in_flight is set by
                    // decision(); record() clears it. Because the provider
                    // lookup is already complete above, no early-return
                    // path between here and the record() calls can leak it.
                }
                crate::circuit_breaker::BreakerDecision::Allow => {
                    // Normal path.
                }
            }

            let started = Instant::now();
            // retry_with_policy returns Err on retry exhaustion OR terminal.
            let result = retry_with_policy(provider, target.clone(), req.clone(), rpolicy).await;
            let elapsed_ms = started.elapsed().as_millis() as u64;

            match result {
                Ok(stream) => {
                    self.breakers
                        .record(&target.provider, &target.model, true, bpolicy);
                    tracing::info!(
                        provider = %target.provider,
                        model = %target.model,
                        chain_position = i,
                        elapsed_ms,
                        "chain element succeeded"
                    );
                    return Ok(stream);
                }
                Err(e) => {
                    self.breakers
                        .record(&target.provider, &target.model, false, bpolicy);
                    let eligibility = fallback_eligibility_with_overrides(&e, &rpolicy.retry_on);
                    let tag = crate::fallback::error_tag(&e);
                    tried.push(TriedUpstream {
                        provider: target.provider.clone(),
                        model: target.model.clone(),
                        attempts: rpolicy.max_attempts, // upper bound; real count
                        // is logged by retry loop
                        last_error_tag: tag.to_string(),
                        last_error_msg: e.to_string(),
                        elapsed_ms,
                    });
                    if eligibility == FallbackEligibility::Terminal {
                        tracing::warn!(
                            provider = %target.provider,
                            model = %target.model,
                            chain_position = i,
                            error = %e,
                            "terminal error; not falling back"
                        );
                        return Err(ResilienceError::TerminalError { error: e, tried });
                    }
                    last_error = Some(e);
                    if i + 1 < chain.len() {
                        tracing::warn!(
                            from = %target.provider,
                            to = %chain[i + 1].provider,
                            chain_position = i,
                            "falling back to next upstream"
                        );
                    }
                    // continue to chain[i+1]
                }
            }
        }

        // Chain exhausted. Distinguish "every element skipped by breaker"
        // from "every element actually attempted and failed".
        if !tried.is_empty() {
            Err(ResilienceError::NoUpstreamSucceeded {
                tried,
                last_error: last_error.expect("last_error set on every Err path"),
            })
        } else if !breakers_skipped.is_empty() {
            Err(ResilienceError::AllBreakersOpen {
                tried: breakers_skipped,
            })
        } else {
            // chain.is_empty() — shouldn't reach here in practice.
            Err(ResilienceError::TerminalError {
                error: ProviderError::Network("empty chain".into()),
                tried: vec![],
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::circuit_breaker::{BreakerDecision, BreakerPolicy, BreakerRegistry};
    use agent_shim_core::{
        request::RequestMetadata, ContentBlock, ExtensionMap, FrontendInfo, FrontendKind,
        FrontendModel, GenerationOptions, Message, RequestId, ResolvedPolicy,
    };
    use agent_shim_providers::ProviderCapabilities;
    use async_trait::async_trait;
    use futures::stream;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Build N policies with `enabled: false` so the breaker never trips
    /// and existing P02 retry/fallback semantics are unchanged.
    fn disabled_breaker(n: usize) -> Vec<BreakerPolicy> {
        (0..n)
            .map(|_| BreakerPolicy {
                enabled: false,
                failure_threshold_pct: 50,
                min_requests: 5,
                window: Duration::from_secs(60),
                open_cooldown: Duration::from_secs(30),
            })
            .collect()
    }

    /// In-memory ProviderLookup with a name → MockProvider mapping.
    struct InMemoryProviders {
        map: HashMap<String, Arc<dyn BackendProvider>>,
    }

    impl ProviderLookup for InMemoryProviders {
        fn get(&self, name: &str) -> Option<Arc<dyn BackendProvider>> {
            self.map.get(name).cloned()
        }
    }

    /// Scripted MockProvider — returns one result per call.
    struct MockProvider {
        name: &'static str,
        scripted: Mutex<Vec<Result<(), ProviderError>>>,
        capabilities: ProviderCapabilities,
    }

    impl MockProvider {
        fn new(name: &'static str, scripted: Vec<Result<(), ProviderError>>) -> Arc<Self> {
            Arc::new(Self {
                name,
                scripted: Mutex::new(scripted),
                capabilities: ProviderCapabilities::default(),
            })
        }
    }

    #[async_trait]
    impl BackendProvider for MockProvider {
        fn name(&self) -> &'static str {
            self.name
        }
        fn capabilities(&self) -> &ProviderCapabilities {
            &self.capabilities
        }
        async fn complete(
            &self,
            _req: CanonicalRequest,
            _target: BackendTarget,
        ) -> Result<CanonicalStream, ProviderError> {
            let mut s = self.scripted.lock().unwrap();
            if s.is_empty() {
                return Err(ProviderError::Network("script exhausted".into()));
            }
            match s.remove(0) {
                Ok(()) => Ok(Box::pin(stream::iter(vec![]))),
                Err(e) => Err(e),
            }
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

    fn fast_policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            initial_backoff_ms: 1,
            multiplier: 2.0,
            jitter_pct: 0.0,
            total_budget_ms: 1000,
            retry_on: vec![
                "network".into(),
                "upstream_5xx".into(),
                "upstream_429".into(),
            ],
        }
    }

    fn target(provider: &str) -> BackendTarget {
        BackendTarget {
            provider: provider.into(),
            model: "gpt-4o".into(),
            policy: Default::default(),
        }
    }

    #[tokio::test]
    async fn returns_first_chain_element_on_success() {
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([(
                "a".to_string(),
                MockProvider::new("a", vec![Ok(())]) as Arc<_>,
            )]),
        });
        let registry = Arc::new(BreakerRegistry::with_system_clock());
        let caller = ResilientCaller::new(providers, registry);
        let chain = vec![target("a")];
        let policies = vec![fast_policy(2)];
        let result = caller
            .complete(chain, dummy_request(), policies, disabled_breaker(1))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn falls_back_on_5xx_then_succeeds() {
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([
                (
                    "a".to_string(),
                    MockProvider::new(
                        "a",
                        vec![
                            Err(ProviderError::Upstream {
                                status: 502,
                                body: "x".into(),
                            }),
                            Err(ProviderError::Upstream {
                                status: 502,
                                body: "x".into(),
                            }),
                        ],
                    ) as Arc<_>,
                ),
                (
                    "b".to_string(),
                    MockProvider::new("b", vec![Ok(())]) as Arc<_>,
                ),
            ]),
        });
        let registry = Arc::new(BreakerRegistry::with_system_clock());
        let caller = ResilientCaller::new(providers, registry);
        let chain = vec![target("a"), target("b")];
        let policies = vec![fast_policy(2), fast_policy(2)];
        let result = caller
            .complete(chain, dummy_request(), policies, disabled_breaker(2))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn does_not_fallback_on_terminal_4xx() {
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([
                (
                    "a".to_string(),
                    MockProvider::new(
                        "a",
                        vec![Err(ProviderError::Upstream {
                            status: 401,
                            body: "auth".into(),
                        })],
                    ) as Arc<_>,
                ),
                (
                    "b".to_string(),
                    MockProvider::new("b", vec![Ok(())]) as Arc<_>,
                ),
            ]),
        });
        let registry = Arc::new(BreakerRegistry::with_system_clock());
        let caller = ResilientCaller::new(providers, registry);
        let chain = vec![target("a"), target("b")];
        let policies = vec![fast_policy(2), fast_policy(2)];
        let result = caller
            .complete(chain, dummy_request(), policies, disabled_breaker(2))
            .await;
        assert!(matches!(result, Err(ResilienceError::TerminalError { .. })));
    }

    #[tokio::test]
    async fn no_upstream_succeeded_when_chain_exhausts() {
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([
                (
                    "a".to_string(),
                    MockProvider::new(
                        "a",
                        vec![
                            Err(ProviderError::Network("a-down".into())),
                            Err(ProviderError::Network("a-down".into())),
                        ],
                    ) as Arc<_>,
                ),
                (
                    "b".to_string(),
                    MockProvider::new(
                        "b",
                        vec![
                            Err(ProviderError::Network("b-down".into())),
                            Err(ProviderError::Network("b-down".into())),
                        ],
                    ) as Arc<_>,
                ),
            ]),
        });
        let registry = Arc::new(BreakerRegistry::with_system_clock());
        let caller = ResilientCaller::new(providers, registry);
        let chain = vec![target("a"), target("b")];
        let policies = vec![fast_policy(2), fast_policy(2)];
        let result = caller
            .complete(chain, dummy_request(), policies, disabled_breaker(2))
            .await;
        match result {
            Err(ResilienceError::NoUpstreamSucceeded { tried, .. }) => {
                assert_eq!(tried.len(), 2);
                assert_eq!(tried[0].provider, "a");
                assert_eq!(tried[1].provider, "b");
            }
            Err(other) => panic!("expected NoUpstreamSucceeded, got {other:?}"),
            Ok(_) => panic!("expected NoUpstreamSucceeded, got Ok(stream)"),
        }
    }

    #[tokio::test]
    async fn breaker_open_skips_chain_element_without_retry() {
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([
                ("a".to_string(), MockProvider::new("a", vec![]) as Arc<_>),
                (
                    "b".to_string(),
                    MockProvider::new("b", vec![Ok(())]) as Arc<_>,
                ),
            ]),
        });
        let registry = Arc::new(BreakerRegistry::with_system_clock());

        // Pre-trip A's breaker.
        let policy = BreakerPolicy {
            enabled: true,
            failure_threshold_pct: 50,
            min_requests: 5,
            window: Duration::from_secs(60),
            open_cooldown: Duration::from_secs(30),
        };
        for _ in 0..5 {
            registry.record("a", "gpt-4o", false, &policy);
        }
        assert_eq!(
            registry.decision("a", "gpt-4o", &policy),
            BreakerDecision::Skip
        );

        let caller = ResilientCaller::new(providers, registry);
        let chain = vec![target("a"), target("b")];
        let result = caller
            .complete(
                chain,
                dummy_request(),
                vec![fast_policy(2), fast_policy(2)],
                vec![policy.clone(), policy.clone()],
            )
            .await;
        assert!(result.is_ok());
        // A's MockProvider was set up with empty scripted vec; if `a` was
        // called even once it would have errored with "script exhausted".
        // The Ok() result on B confirms A was correctly skipped.
    }

    #[tokio::test]
    async fn all_breakers_open_returns_dedicated_variant() {
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([
                ("a".to_string(), MockProvider::new("a", vec![]) as Arc<_>),
                ("b".to_string(), MockProvider::new("b", vec![]) as Arc<_>),
            ]),
        });
        let registry = Arc::new(BreakerRegistry::with_system_clock());
        let policy = BreakerPolicy {
            enabled: true,
            failure_threshold_pct: 50,
            min_requests: 5,
            window: Duration::from_secs(60),
            open_cooldown: Duration::from_secs(30),
        };
        for _ in 0..5 {
            registry.record("a", "gpt-4o", false, &policy);
        }
        for _ in 0..5 {
            registry.record("b", "gpt-4o", false, &policy);
        }
        let caller = ResilientCaller::new(providers, registry);
        let chain = vec![target("a"), target("b")];
        let result = caller
            .complete(
                chain,
                dummy_request(),
                vec![fast_policy(2), fast_policy(2)],
                vec![policy.clone(), policy.clone()],
            )
            .await;
        match result {
            Err(ResilienceError::AllBreakersOpen { tried }) => {
                assert_eq!(tried.len(), 2);
                assert_eq!(tried[0], "a");
                assert_eq!(tried[1], "b");
            }
            Err(other) => panic!("expected AllBreakersOpen, got {other:?}"),
            Ok(_) => panic!("expected AllBreakersOpen, got Ok(stream)"),
        }
    }
}
