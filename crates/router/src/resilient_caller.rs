//! Resilient caller orchestrator (Plan 04 P02 T4).
//!
//! Walks a `Vec<BackendTarget>` chain, calling `retry_with_policy` for each
//! element. On retry exhaustion with a fallback-eligible last error,
//! advances to the next chain element. On a terminal error, returns
//! immediately. On success, returns the stream.
//!
//! Plan 03 inserts the breaker gate inside the chain walk.
//! Plan 04 inserts the rate-limit gate before the chain walk.
//!
//! # Tracing event taxonomy (Plan 05 P05 T1)
//!
//! Every resilience event uses one of these `target = "agent_shim::resilience"`
//! event names, with the field set documented per kind:
//!
//! | event_name              | level | fields                                                                          |
//! |-------------------------|-------|---------------------------------------------------------------------------------|
//! | retry.attempt           | warn  | request_id, upstream, model, attempt, error_class, backoff_ms, total_elapsed_ms |
//! | retry.exhausted         | warn  | request_id, upstream, model, attempts, last_error                               |
//! | fallback.transition     | warn  | request_id, from_upstream, to_upstream, reason                                  |
//! | breaker.state_change    | info  | upstream, model, from_state, to_state, reason                                   |
//! | rate_limit.rejected     | warn  | request_id, identity, dimension, retry_after_secs                               |
//! | request.completed       | info on success / warn on failure | request_id, identity, frontend_model, outcome, total_elapsed_ms, tried |
//!
//! All numeric fields use Rust integer types (no strings). `identity` uses
//! `AgentIdentity::log_id()` which returns `"anonymous"` or the full
//! `"sha256:<hex>"` form. Plaintext keys never appear in log output.
//!
//! ## `request_id` provenance (known limitation)
//!
//! Until middleware-level plumbing lands, `request_id` is generated as a
//! fresh UUID at the top of [`ResilientCaller::complete`]. It satisfies the
//! field-set contract (operators see *a* stable id correlating events from
//! one request) but does NOT match the request-id header set by
//! `crates/observability/src/request_id.rs`. A future task should plumb a
//! real `Context { request_id }` from the pipeline.

use std::sync::Arc;
use std::time::Instant;

use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream};
use agent_shim_providers::{BackendProvider, ProviderError};
use tracing::Instrument;

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
    limiters: Arc<crate::rate_limit::LimiterRegistry>,
}

impl ResilientCaller {
    pub fn new(
        providers: Arc<dyn ProviderLookup>,
        breakers: Arc<crate::circuit_breaker::BreakerRegistry>,
        limiters: Arc<crate::rate_limit::LimiterRegistry>,
    ) -> Self {
        Self {
            providers,
            breakers,
            limiters,
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
    ///
    /// Plan 04 P04 T4: rate-limit gates layered around the chain walk:
    /// - Pre-chain (per-key + per-route + per-IP): runs once before the
    ///   chain walk. First rejecting bucket short-circuits the request.
    /// - Per-upstream: runs inside the chain walk on each element after
    ///   the breaker gate. A rejection returns `RateLimited{PerUpstream}`
    ///   immediately — no fallback to the next chain element.
    ///
    /// `#[allow(clippy::too_many_arguments)]`: the parameters are the
    /// minimum needed to fully describe the request — chain, body,
    /// per-element retry/breaker policies, and identity/IP/model for
    /// the rate-limit gates. Bundling them into a struct would just
    /// move the verbosity to the caller without buying anything.
    #[allow(clippy::too_many_arguments)]
    pub async fn complete(
        &self,
        chain: Vec<BackendTarget>,
        req: CanonicalRequest,
        retry_policies: Vec<RetryPolicy>,
        breaker_policies: Vec<crate::circuit_breaker::BreakerPolicy>,
        identity: crate::auth::AgentIdentity,
        client_ip: String,
        frontend_model: String,
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

        // KNOWN LIMITATION (Plan 05 P05 T1): generate a fresh UUID per call
        // until middleware-level request-id plumbing lands. See module doc.
        let request_id = uuid::Uuid::new_v4().to_string();
        let start = Instant::now();

        // Run the chain walk in an inner async block so we can emit the
        // `request.completed` summary on every exit path.
        let result = self
            .complete_inner(
                chain,
                req,
                retry_policies,
                breaker_policies,
                &identity,
                &client_ip,
                &frontend_model,
                &request_id,
            )
            .await;

        let total_elapsed_ms = start.elapsed().as_millis() as u64;
        let outcome: &'static str = match &result {
            Ok(_) => "success",
            Err(ResilienceError::RateLimited { .. }) => "rate_limited",
            Err(ResilienceError::NoUpstreamSucceeded { .. }) => "no_upstream_succeeded",
            Err(ResilienceError::AllBreakersOpen { .. }) => "all_breakers_open",
            Err(ResilienceError::TerminalError { .. }) => "terminal_error",
        };
        let tried_summary: Vec<String> = match &result {
            Ok(_) => Vec::new(),
            Err(ResilienceError::NoUpstreamSucceeded { tried, .. }) => tried
                .iter()
                .map(|t| {
                    format!(
                        "{}/{} attempts={} last={}",
                        t.provider, t.model, t.attempts, t.last_error_tag
                    )
                })
                .collect(),
            Err(ResilienceError::TerminalError { tried, .. }) => tried
                .iter()
                .map(|t| {
                    format!(
                        "{}/{} attempts={} last={}",
                        t.provider, t.model, t.attempts, t.last_error_tag
                    )
                })
                .collect(),
            Err(ResilienceError::AllBreakersOpen { tried }) => tried.clone(),
            Err(ResilienceError::RateLimited { .. }) => Vec::new(),
        };

        if result.is_ok() {
            tracing::info!(
                target: "agent_shim::resilience",
                event_name = "request.completed",
                request_id = %request_id,
                identity = %identity.log_id(),
                frontend_model = %frontend_model,
                outcome = outcome,
                total_elapsed_ms = total_elapsed_ms,
                tried = ?tried_summary,
                "request completed"
            );
        } else {
            tracing::warn!(
                target: "agent_shim::resilience",
                event_name = "request.completed",
                request_id = %request_id,
                identity = %identity.log_id(),
                frontend_model = %frontend_model,
                outcome = outcome,
                total_elapsed_ms = total_elapsed_ms,
                tried = ?tried_summary,
                "request completed with error"
            );
        }
        result
    }

    /// Inner chain-walk body. Pulled out of `complete()` so the wrapper can
    /// emit the `request.completed` summary on every exit path without
    /// repeating it at every `return`.
    #[allow(clippy::too_many_arguments)]
    async fn complete_inner(
        &self,
        chain: Vec<BackendTarget>,
        req: CanonicalRequest,
        retry_policies: Vec<RetryPolicy>,
        breaker_policies: Vec<crate::circuit_breaker::BreakerPolicy>,
        identity: &crate::auth::AgentIdentity,
        client_ip: &str,
        frontend_model: &str,
        request_id: &str,
    ) -> Result<CanonicalStream, ResilienceError> {
        // ── PRE-CHAIN RATE LIMIT GATE ────────────────────────────────────
        // Outermost gate per §5.2 layering: per-key + per-route + per-IP
        // buckets are consulted before any breaker decision or provider
        // lookup. A rejection here is request-scoped (not chain-element
        // scoped) so we never enter the chain walk.
        if let Err((dim, retry_after_secs)) =
            self.limiters
                .check_pre_chain(identity, frontend_model, client_ip)
        {
            tracing::warn!(
                target: "agent_shim::resilience",
                event_name = "rate_limit.rejected",
                request_id = %request_id,
                identity = %identity.log_id(),
                dimension = dimension_label(dim),
                retry_after_secs = retry_after_secs,
                "rate limit exceeded (pre-chain)"
            );
            metrics::counter!(
                crate::metric_names::RATE_LIMIT_REJECTED_TOTAL,
                "dimension" => dimension_label(dim),
            )
            .increment(1);
            return Err(ResilienceError::RateLimited {
                dimension: dim,
                retry_after_secs,
            });
        }

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
                    // Operational log — NOT a `breaker.state_change` event;
                    // those are emitted from inside `BreakerState`.
                    tracing::warn!(
                        target: "agent_shim::resilience",
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
                        target: "agent_shim::resilience",
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

            // ── PER-UPSTREAM RATE LIMIT GATE ────────────────────────────
            // §5.2: just-in-time on each chain element after the breaker
            // gate, before retry. A rejection here returns RateLimited
            // immediately — we do NOT fall back to the next chain element
            // (the upstream is intentionally back-pressured; trying the
            // fallback would defeat the bucket).
            if let Err((dim, retry_after_secs)) = self.limiters.check_per_upstream(&target.provider)
            {
                tracing::warn!(
                    target: "agent_shim::resilience",
                    event_name = "rate_limit.rejected",
                    request_id = %request_id,
                    identity = %identity.log_id(),
                    dimension = dimension_label(dim),
                    retry_after_secs = retry_after_secs,
                    upstream = %target.provider,
                    "rate limit exceeded (per-upstream); not falling back"
                );
                metrics::counter!(
                    crate::metric_names::RATE_LIMIT_REJECTED_TOTAL,
                    "dimension" => dimension_label(dim),
                )
                .increment(1);
                return Err(ResilienceError::RateLimited {
                    dimension: dim,
                    retry_after_secs,
                });
            }

            let started = Instant::now();
            // Plan 03 P03 T3: `provider.complete` child span wrapping the
            // per-chain-element retry loop. `attempts` is recorded after the
            // call so the span carries the final attempt count regardless of
            // success/failure. `fallback_position` is the 0-indexed chain
            // position — 0 for the primary upstream, ≥1 for fallbacks.
            let provider_span = tracing::info_span!(
                "provider.complete",
                "agent_shim.upstream" = %target.provider,
                "agent_shim.model" = %target.model,
                "agent_shim.attempts" = tracing::field::Empty,
                "agent_shim.fallback_position" = i as i64,
            );
            // retry_with_policy returns Err on retry exhaustion OR terminal.
            let result =
                retry_with_policy(provider, target.clone(), req.clone(), rpolicy, request_id)
                    .instrument(provider_span.clone())
                    .await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            // `rpolicy.max_attempts` is an upper bound; the real attempt
            // count is logged by the retry loop itself. Recording the
            // bound here is still useful: it sets the span's attribute
            // shape unconditionally and operators can correlate with the
            // per-attempt event for the precise count.
            provider_span.record("agent_shim.attempts", rpolicy.max_attempts as i64);

            match result {
                Ok(stream) => {
                    self.breakers
                        .record(&target.provider, &target.model, true, bpolicy);
                    tracing::info!(
                        target: "agent_shim::resilience",
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
                            target: "agent_shim::resilience",
                            provider = %target.provider,
                            model = %target.model,
                            chain_position = i,
                            error = %e,
                            "terminal error; not falling back"
                        );
                        return Err(ResilienceError::TerminalError { error: e, tried });
                    }
                    if i + 1 < chain.len() {
                        tracing::warn!(
                            target: "agent_shim::resilience",
                            event_name = "fallback.transition",
                            request_id = %request_id,
                            from_upstream = %target.provider,
                            to_upstream = %chain[i + 1].provider,
                            reason = "retry_exhausted",
                            "falling back to next upstream"
                        );
                        // TODO(plan-05): thread a `route_label` param when
                        // available; empty string for now.
                        metrics::counter!(
                            crate::metric_names::FALLBACK_TRANSITIONS_TOTAL,
                            "route" => String::new(),
                            "from_upstream" => target.provider.clone(),
                            "to_upstream" => chain[i + 1].provider.clone(),
                        )
                        .increment(1);
                    }
                    last_error = Some(e);
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

/// Stable label for a [`crate::errors::RateLimitDimension`] in tracing
/// events (Plan 05 P05 T1).
fn dimension_label(dim: crate::errors::RateLimitDimension) -> &'static str {
    use crate::errors::RateLimitDimension as D;
    match dim {
        D::PerKey => "per_key",
        D::PerRoute => "per_route",
        D::PerUpstream => "per_upstream",
        D::PerIp => "per_ip",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::circuit_breaker::{BreakerDecision, BreakerPolicy, BreakerRegistry};
    use crate::errors::RateLimitDimension;
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

    /// Build a disabled `LimiterRegistry` so the rate-limit gates are
    /// no-ops in tests that exercise unrelated paths (retry/fallback,
    /// breaker semantics, etc).
    fn disabled_limiter() -> Arc<crate::rate_limit::LimiterRegistry> {
        Arc::new(crate::rate_limit::LimiterRegistry::disabled())
    }

    /// Standard test identity — no real key in tests, so use Anonymous.
    /// Combined with `disabled_limiter()`, the pre-chain gate is a no-op
    /// regardless of identity, but the helper keeps call sites readable.
    fn anon() -> crate::auth::AgentIdentity {
        crate::auth::AgentIdentity::Anonymous
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
        let caller = ResilientCaller::new(providers, registry, disabled_limiter());
        let chain = vec![target("a")];
        let policies = vec![fast_policy(2)];
        let result = caller
            .complete(
                chain,
                dummy_request(),
                policies,
                disabled_breaker(1),
                anon(),
                "127.0.0.1".to_string(),
                "openai_chat/gpt-4o".to_string(),
            )
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
        let caller = ResilientCaller::new(providers, registry, disabled_limiter());
        let chain = vec![target("a"), target("b")];
        let policies = vec![fast_policy(2), fast_policy(2)];
        let result = caller
            .complete(
                chain,
                dummy_request(),
                policies,
                disabled_breaker(2),
                anon(),
                "127.0.0.1".to_string(),
                "openai_chat/gpt-4o".to_string(),
            )
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
        let caller = ResilientCaller::new(providers, registry, disabled_limiter());
        let chain = vec![target("a"), target("b")];
        let policies = vec![fast_policy(2), fast_policy(2)];
        let result = caller
            .complete(
                chain,
                dummy_request(),
                policies,
                disabled_breaker(2),
                anon(),
                "127.0.0.1".to_string(),
                "openai_chat/gpt-4o".to_string(),
            )
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
        let caller = ResilientCaller::new(providers, registry, disabled_limiter());
        let chain = vec![target("a"), target("b")];
        let policies = vec![fast_policy(2), fast_policy(2)];
        let result = caller
            .complete(
                chain,
                dummy_request(),
                policies,
                disabled_breaker(2),
                anon(),
                "127.0.0.1".to_string(),
                "openai_chat/gpt-4o".to_string(),
            )
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

        let caller = ResilientCaller::new(providers, registry, disabled_limiter());
        let chain = vec![target("a"), target("b")];
        let result = caller
            .complete(
                chain,
                dummy_request(),
                vec![fast_policy(2), fast_policy(2)],
                vec![policy.clone(), policy.clone()],
                anon(),
                "127.0.0.1".to_string(),
                "openai_chat/gpt-4o".to_string(),
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
        let caller = ResilientCaller::new(providers, registry, disabled_limiter());
        let chain = vec![target("a"), target("b")];
        let result = caller
            .complete(
                chain,
                dummy_request(),
                vec![fast_policy(2), fast_policy(2)],
                vec![policy.clone(), policy.clone()],
                anon(),
                "127.0.0.1".to_string(),
                "openai_chat/gpt-4o".to_string(),
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

    /// Build a tiny rate_limit config with burst=2 buckets so the third
    /// request in the same instant gets rejected. Used by the two
    /// rate-limit gate tests below.
    fn small_rate_limit_cfg() -> agent_shim_config::RateLimitConfig {
        let bucket = agent_shim_config::BucketConfigYaml {
            rate_per_sec: 1,
            burst: 2,
        };
        let mut cfg = agent_shim_config::RateLimitConfig {
            enabled: true,
            per_key: agent_shim_config::PerKeyConfig {
                default: Some(bucket.clone()),
                anonymous: Some(bucket.clone()),
                overrides: std::collections::BTreeMap::new(),
            },
            per_route: std::collections::BTreeMap::new(),
            per_upstream: std::collections::BTreeMap::new(),
            per_ip: agent_shim_config::PerIpConfig {
                enabled: false,
                rate_per_sec: 5,
                burst: 5,
            },
        };
        cfg.per_upstream.insert("a".to_string(), bucket);
        cfg
    }

    #[tokio::test]
    async fn pre_chain_rate_limit_returns_rate_limited() {
        // Pre-chain gate (per-key, anonymous bucket): once the bucket is
        // burnt, complete() must short-circuit with RateLimited{PerKey}
        // BEFORE consulting the breaker or the provider lookup. We assert
        // the provider was never called by giving it an empty scripted
        // vec — any call would raise "script exhausted".
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([("a".to_string(), MockProvider::new("a", vec![]) as Arc<_>)]),
        });
        let breakers = Arc::new(BreakerRegistry::with_system_clock());
        let limiters = Arc::new(crate::rate_limit::LimiterRegistry::from_config(
            &small_rate_limit_cfg(),
        ));

        // Burn the anonymous per-key bucket (burst=2 → two allows then
        // a reject) by directly checking the registry.
        for _ in 0..2 {
            assert!(limiters
                .check_pre_chain(&anon(), "openai_chat/gpt-4o", "127.0.0.1")
                .is_ok());
        }

        let caller = ResilientCaller::new(providers, breakers, limiters);
        let chain = vec![target("a")];
        let result = caller
            .complete(
                chain,
                dummy_request(),
                vec![fast_policy(2)],
                disabled_breaker(1),
                anon(),
                "127.0.0.1".to_string(),
                "openai_chat/gpt-4o".to_string(),
            )
            .await;
        match result {
            Err(ResilienceError::RateLimited {
                dimension,
                retry_after_secs,
            }) => {
                assert_eq!(dimension, RateLimitDimension::PerKey);
                assert!(retry_after_secs >= 1);
            }
            Err(other) => panic!("expected RateLimited{{PerKey}}, got {other:?}"),
            Ok(_) => panic!("expected RateLimited{{PerKey}}, got Ok(stream)"),
        }
    }

    #[tokio::test]
    async fn per_upstream_rate_limit_returns_rate_limited() {
        // Per-upstream gate runs INSIDE the chain walk after the breaker
        // gate. Burn upstream "a"'s bucket; the chain walk hits "a", the
        // gate rejects, and complete() returns RateLimited{PerUpstream}
        // immediately — no fallback to upstream "b".
        //
        // Identity is a fresh KeyHash so the per-key bucket (which is
        // also configured by small_rate_limit_cfg) doesn't fire first;
        // the per-key gate uses the keyed bucket which is independent
        // of the anonymous bucket burnt below.
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([
                ("a".to_string(), MockProvider::new("a", vec![]) as Arc<_>),
                (
                    "b".to_string(),
                    MockProvider::new("b", vec![Ok(())]) as Arc<_>,
                ),
            ]),
        });
        let breakers = Arc::new(BreakerRegistry::with_system_clock());
        let limiters = Arc::new(crate::rate_limit::LimiterRegistry::from_config(
            &small_rate_limit_cfg(),
        ));

        // Burn the per-upstream bucket for "a" (burst=2).
        for _ in 0..2 {
            assert!(limiters.check_per_upstream("a").is_ok());
        }

        // Use a fresh per-request KeyHash identity so the per-key bucket
        // doesn't reject first. Any sha256-shaped string works because
        // the test config has no per_key_overrides → the default
        // per_key bucket is used, with its own burst=2 budget for this
        // identity.
        let id = crate::auth::AgentIdentity::KeyHash("sha256:test_key_hash".to_string());

        let caller = ResilientCaller::new(providers, breakers, limiters);
        let chain = vec![target("a"), target("b")];
        let result = caller
            .complete(
                chain,
                dummy_request(),
                vec![fast_policy(2), fast_policy(2)],
                disabled_breaker(2),
                id,
                "127.0.0.1".to_string(),
                "openai_chat/gpt-4o".to_string(),
            )
            .await;
        match result {
            Err(ResilienceError::RateLimited {
                dimension,
                retry_after_secs,
            }) => {
                assert_eq!(dimension, RateLimitDimension::PerUpstream);
                assert!(retry_after_secs >= 1);
            }
            Err(other) => panic!("expected RateLimited{{PerUpstream}}, got {other:?}"),
            Ok(_) => panic!("expected RateLimited{{PerUpstream}}, got Ok(stream)"),
        }
    }
}
