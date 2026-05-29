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

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use agent_shim_core::{CanonicalRequest, CanonicalStream, StreamError, StreamEvent};
use agent_shim_providers::{BackendProvider, ProviderError};
use futures_core::Stream;
use tracing::Instrument;

use crate::admission::AdmissionTicket;
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
    _breakers: Arc<crate::circuit_breaker::BreakerRegistry>,
    _limiters: Arc<arc_swap::ArcSwap<crate::rate_limit::LimiterRegistry>>,
    _probe: Arc<dyn crate::latency_probe::LatencyProbe>,
}

impl ResilientCaller {
    pub fn new(
        providers: Arc<dyn ProviderLookup>,
        breakers: Arc<crate::circuit_breaker::BreakerRegistry>,
        limiters: Arc<arc_swap::ArcSwap<crate::rate_limit::LimiterRegistry>>,
        probe: Arc<dyn crate::latency_probe::LatencyProbe>,
    ) -> Self {
        Self {
            providers,
            _breakers: breakers,
            _limiters: limiters,
            _probe: probe,
        }
    }

    /// Walk the admitted chain. Each element gets its own retry budget.
    /// Fallback to chain[i+1] only on retry exhaustion with a fallback-
    /// eligible last error; terminal errors short-circuit.
    pub async fn complete(
        &self,
        ticket: AdmissionTicket,
        req: CanonicalRequest,
    ) -> Result<CanonicalStream, ResilienceError> {
        self.complete_inner_summary(ticket, req).await
    }

    /// Wrapper that emits the `request.completed` summary on every exit
    /// path.
    async fn complete_inner_summary(
        &self,
        ticket: AdmissionTicket,
        req: CanonicalRequest,
    ) -> Result<CanonicalStream, ResilienceError> {
        // KNOWN LIMITATION (Plan 05 P05 T1): generate a fresh UUID per call
        // until middleware-level request-id plumbing lands. See module doc.
        let request_id = uuid::Uuid::new_v4().to_string();
        let start = Instant::now();
        let frontend_model = ticket.resolved().route_label.to_string();
        let identity = ticket.identity().log_id().to_string();

        // Run the chain walk in an inner async block so we can emit the
        // `request.completed` summary on every exit path.
        let result = self.complete_inner(ticket, req, &request_id).await;

        let total_elapsed_ms = start.elapsed().as_millis() as u64;
        let outcome: &'static str = match &result {
            Ok(_) => "success",
            Err(ResilienceError::RateLimited { .. }) => "rate_limited",
            Err(ResilienceError::NoUpstreamSucceeded { .. }) => "no_upstream_succeeded",
            Err(ResilienceError::AllBreakersOpen { .. }) => "all_breakers_open",
            Err(ResilienceError::TerminalError { .. }) => "terminal_error",
            Err(ResilienceError::NoEligibleUpstream { .. }) => "no_eligible_upstream",
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
            Err(ResilienceError::NoEligibleUpstream { .. }) => Vec::new(),
        };

        if result.is_ok() {
            tracing::info!(
                target: "agent_shim::resilience",
                event_name = "request.completed",
                request_id = %request_id,
                identity = %identity,
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
                identity = %identity,
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
    async fn complete_inner(
        &self,
        ticket: AdmissionTicket,
        req: CanonicalRequest,
        request_id: &str,
    ) -> Result<CanonicalStream, ResilienceError> {
        let chain = ticket.chain().to_vec();
        let retry_policy = RetryPolicy::from(&ticket.resolved().retry);
        let mut tried: Vec<TriedUpstream> = Vec::new();
        let mut last_error: Option<ProviderError> = None;
        let mut breakers_skipped: Vec<String> = Vec::new();

        for (i, target) in chain.iter().enumerate() {
            if !ticket.breaker_allowed(i) {
                tracing::warn!(
                    target: "agent_shim::resilience",
                    provider = %target.provider,
                    model = %target.model,
                    chain_position = i,
                    "breaker open; skipping chain element"
                );
                breakers_skipped.push(target.provider.clone());
                continue;
            }

            let provider = match self.providers.get(&target.provider) {
                Some(p) => p,
                None => {
                    let e = ProviderError::UnknownProvider(target.provider.clone());
                    return Err(ResilienceError::TerminalError { error: e, tried });
                }
            };

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
            // Plan 03 P03 T3 followup: `RetryOutcome` carries the realized
            // attempt count so the span attribute reflects the actual number
            // of provider calls made, not `policy.max_attempts` (the upper
            // bound).
            let outcome = retry_with_policy(
                provider,
                target.clone(),
                req.clone(),
                &retry_policy,
                request_id,
            )
            .instrument(provider_span.clone())
            .await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            provider_span.record("agent_shim.attempts", outcome.attempts as i64);
            let result = outcome.result;
            let realized_attempts = outcome.attempts;

            match result {
                Ok(stream) => {
                    tracing::info!(
                        target: "agent_shim::resilience",
                        provider = %target.provider,
                        model = %target.model,
                        chain_position = i,
                        elapsed_ms,
                        "chain element succeeded"
                    );
                    return Ok(Box::pin(ConsumeOnFirstEventStream::new(stream, ticket, i)));
                }
                Err(e) => {
                    let eligibility =
                        fallback_eligibility_with_overrides(&e, &retry_policy.retry_on);
                    let tag = crate::fallback::error_tag(&e);
                    tried.push(TriedUpstream {
                        provider: target.provider.clone(),
                        model: target.model.clone(),
                        attempts: realized_attempts,
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

struct ConsumeOnFirstEventStream {
    inner: CanonicalStream,
    ticket: Option<AdmissionTicket>,
    chain_index: usize,
}

impl ConsumeOnFirstEventStream {
    fn new(inner: CanonicalStream, ticket: AdmissionTicket, chain_index: usize) -> Self {
        Self {
            inner,
            ticket: Some(ticket),
            chain_index,
        }
    }
}

impl Stream for ConsumeOnFirstEventStream {
    type Item = Result<StreamEvent, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let poll = self.inner.as_mut().poll_next(cx);
        let first_event_succeeded = match &poll {
            Poll::Ready(Some(Ok(StreamEvent::Error { .. }))) => Some(false),
            Poll::Ready(Some(Ok(_))) => Some(true),
            Poll::Ready(Some(Err(_))) => Some(false),
            _ => None,
        };
        if let Some(succeeded) = first_event_succeeded {
            if let Some(mut ticket) = self.ticket.take() {
                ticket.consume(self.chain_index, succeeded);
            }
        }
        poll
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::circuit_breaker::{BreakerDecision, BreakerPolicy, BreakerRegistry};
    use agent_shim_config::{BreakerConfig, RetryConfig};
    use agent_shim_core::{
        request::RequestMetadata, BackendTarget, ContentBlock, ExtensionMap, FrontendInfo,
        FrontendKind, FrontendModel, GenerationOptions, Message, RequestId, ResolvedPolicy,
        ResponseId, StreamEvent,
    };
    use agent_shim_providers::ProviderCapabilities;
    use async_trait::async_trait;
    use futures::{stream, StreamExt};
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn disabled_breaker_config() -> BreakerConfig {
        BreakerConfig {
            enabled: false,
            failure_threshold_pct: 50,
            min_requests: 5,
            window_secs: 60,
            open_cooldown_secs: 30,
        }
    }

    fn open_on_single_failure_breaker_config() -> BreakerConfig {
        BreakerConfig {
            enabled: true,
            failure_threshold_pct: 100,
            min_requests: 1,
            window_secs: 60,
            open_cooldown_secs: 30,
        }
    }

    /// Build a disabled `LimiterRegistry` so the rate-limit gates are
    /// no-ops in tests that exercise unrelated paths (retry/fallback,
    /// breaker semantics, etc).
    fn disabled_limiter() -> Arc<arc_swap::ArcSwap<crate::rate_limit::LimiterRegistry>> {
        Arc::new(arc_swap::ArcSwap::from_pointee(
            crate::rate_limit::LimiterRegistry::disabled(),
        ))
    }

    /// Plan 06 P04 T4: a probe handle for tests that don't exercise the
    /// cost-filter latency axis. `complete()` skips the filter entirely,
    /// so the probe is never consulted — but the new field still has to
    /// be supplied to `ResilientCaller::new`.
    fn disabled_probe() -> Arc<dyn crate::latency_probe::LatencyProbe> {
        Arc::new(crate::latency_probe::DisabledLatencyProbe)
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
        scripted: Mutex<Vec<Result<Vec<StreamEvent>, ProviderError>>>,
        capabilities: ProviderCapabilities,
    }

    impl MockProvider {
        fn new(
            name: &'static str,
            scripted: Vec<Result<Vec<StreamEvent>, ProviderError>>,
        ) -> Arc<Self> {
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
                Ok(events) => Ok(Box::pin(stream::iter(events.into_iter().map(Ok)))),
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

    fn fast_retry_config(max_attempts: u32) -> RetryConfig {
        RetryConfig {
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

    fn ticket_for(
        chain: Vec<BackendTarget>,
        registry: &Arc<BreakerRegistry>,
        retry: RetryConfig,
        breaker: BreakerConfig,
    ) -> AdmissionTicket {
        let policy = BreakerPolicy::from(&breaker);
        let holds = chain
            .iter()
            .map(|target| registry.try_hold(&target.provider, &target.model, &policy))
            .collect();
        AdmissionTicket::for_test(
            chain.clone(),
            crate::ResolvedRoute {
                chain,
                retry,
                breaker,
                route_label: Arc::from("openai_chat/gpt-4o"),
            },
            holds,
        )
    }

    fn response_start() -> StreamEvent {
        StreamEvent::ResponseStart {
            id: ResponseId::new(),
            model: "gpt-4o".into(),
            created_at_unix: 0,
        }
    }

    #[tokio::test]
    async fn returns_first_chain_element_on_success() {
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([(
                "a".to_string(),
                MockProvider::new("a", vec![Ok(vec![])]) as Arc<_>,
            )]),
        });
        let registry = Arc::new(BreakerRegistry::with_system_clock());
        let chain = vec![target("a")];
        let ticket = ticket_for(
            chain,
            &registry,
            fast_retry_config(2),
            disabled_breaker_config(),
        );
        let caller = ResilientCaller::new(
            providers,
            Arc::clone(&registry),
            disabled_limiter(),
            disabled_probe(),
        );
        let result = caller.complete(ticket, dummy_request()).await;
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
                    MockProvider::new("b", vec![Ok(vec![])]) as Arc<_>,
                ),
            ]),
        });
        let registry = Arc::new(BreakerRegistry::with_system_clock());
        let chain = vec![target("a"), target("b")];
        let ticket = ticket_for(
            chain,
            &registry,
            fast_retry_config(2),
            disabled_breaker_config(),
        );
        let caller = ResilientCaller::new(
            providers,
            Arc::clone(&registry),
            disabled_limiter(),
            disabled_probe(),
        );
        let result = caller.complete(ticket, dummy_request()).await;
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
                    MockProvider::new("b", vec![Ok(vec![])]) as Arc<_>,
                ),
            ]),
        });
        let registry = Arc::new(BreakerRegistry::with_system_clock());
        let chain = vec![target("a"), target("b")];
        let ticket = ticket_for(
            chain,
            &registry,
            fast_retry_config(2),
            disabled_breaker_config(),
        );
        let caller = ResilientCaller::new(
            providers,
            Arc::clone(&registry),
            disabled_limiter(),
            disabled_probe(),
        );
        let result = caller.complete(ticket, dummy_request()).await;
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
        let chain = vec![target("a"), target("b")];
        let ticket = ticket_for(
            chain,
            &registry,
            fast_retry_config(2),
            disabled_breaker_config(),
        );
        let caller = ResilientCaller::new(
            providers,
            Arc::clone(&registry),
            disabled_limiter(),
            disabled_probe(),
        );
        let result = caller.complete(ticket, dummy_request()).await;
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
                    MockProvider::new("b", vec![Ok(vec![])]) as Arc<_>,
                ),
            ]),
        });
        let registry = Arc::new(BreakerRegistry::with_system_clock());

        // Pre-trip A's breaker.
        let breaker = BreakerConfig {
            enabled: true,
            failure_threshold_pct: 50,
            min_requests: 5,
            window_secs: 60,
            open_cooldown_secs: 30,
        };
        let policy = BreakerPolicy::from(&breaker);
        for _ in 0..5 {
            registry.record("a", "gpt-4o", false, &policy);
        }
        assert_eq!(
            registry.decision("a", "gpt-4o", &policy),
            BreakerDecision::Skip
        );

        let chain = vec![target("a"), target("b")];
        let ticket = ticket_for(chain, &registry, fast_retry_config(2), breaker);
        let caller = ResilientCaller::new(
            providers,
            Arc::clone(&registry),
            disabled_limiter(),
            disabled_probe(),
        );
        let result = caller.complete(ticket, dummy_request()).await;
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
        let breaker = BreakerConfig {
            enabled: true,
            failure_threshold_pct: 50,
            min_requests: 5,
            window_secs: 60,
            open_cooldown_secs: 30,
        };
        let policy = BreakerPolicy::from(&breaker);
        for _ in 0..5 {
            registry.record("a", "gpt-4o", false, &policy);
        }
        for _ in 0..5 {
            registry.record("b", "gpt-4o", false, &policy);
        }
        let chain = vec![target("a"), target("b")];
        let ticket = ticket_for(chain, &registry, fast_retry_config(2), breaker);
        let caller = ResilientCaller::new(
            providers,
            Arc::clone(&registry),
            disabled_limiter(),
            disabled_probe(),
        );
        let result = caller.complete(ticket, dummy_request()).await;
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

    #[tokio::test]
    async fn complete_consumes_on_first_event() {
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([(
                "a".to_string(),
                MockProvider::new("a", vec![Ok(vec![response_start()])]) as Arc<_>,
            )]),
        });
        let registry = Arc::new(BreakerRegistry::with_system_clock());
        let breaker = open_on_single_failure_breaker_config();
        let policy = BreakerPolicy::from(&breaker);
        let chain = vec![target("a")];
        let ticket = ticket_for(chain, &registry, fast_retry_config(1), breaker);
        let caller = ResilientCaller::new(
            providers,
            Arc::clone(&registry),
            disabled_limiter(),
            disabled_probe(),
        );

        let mut stream = caller.complete(ticket, dummy_request()).await.unwrap();
        assert!(stream.next().await.unwrap().is_ok());
        drop(stream);

        assert_eq!(
            registry.decision("a", "gpt-4o", &policy),
            BreakerDecision::Allow
        );
    }

    #[tokio::test]
    async fn complete_records_failure_on_first_stream_error_event() {
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([(
                "a".to_string(),
                MockProvider::new(
                    "a",
                    vec![Ok(vec![StreamEvent::Error {
                        message: "upstream error chunk".into(),
                    }])],
                ) as Arc<_>,
            )]),
        });
        let registry = Arc::new(BreakerRegistry::with_system_clock());
        let breaker = open_on_single_failure_breaker_config();
        let policy = BreakerPolicy::from(&breaker);
        let chain = vec![target("a")];
        let ticket = ticket_for(chain, &registry, fast_retry_config(1), breaker);
        let caller = ResilientCaller::new(
            providers,
            Arc::clone(&registry),
            disabled_limiter(),
            disabled_probe(),
        );

        let mut stream = caller.complete(ticket, dummy_request()).await.unwrap();
        assert!(matches!(
            stream.next().await.unwrap(),
            Ok(StreamEvent::Error { .. })
        ));
        drop(stream);

        assert_eq!(
            registry.decision("a", "gpt-4o", &policy),
            BreakerDecision::Skip
        );
    }

    #[tokio::test]
    async fn complete_records_failure_on_empty_stream() {
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([(
                "a".to_string(),
                MockProvider::new("a", vec![Ok(vec![])]) as Arc<_>,
            )]),
        });
        let registry = Arc::new(BreakerRegistry::with_system_clock());
        let breaker = open_on_single_failure_breaker_config();
        let policy = BreakerPolicy::from(&breaker);
        let chain = vec![target("a")];
        let ticket = ticket_for(chain, &registry, fast_retry_config(1), breaker);
        let caller = ResilientCaller::new(
            providers,
            Arc::clone(&registry),
            disabled_limiter(),
            disabled_probe(),
        );

        let mut stream = caller.complete(ticket, dummy_request()).await.unwrap();
        assert!(stream.next().await.is_none());
        drop(stream);

        assert_eq!(
            registry.decision("a", "gpt-4o", &policy),
            BreakerDecision::Skip,
            "empty-stream first-byte case must record as abandoned-probe failure"
        );
    }

    #[tokio::test]
    async fn complete_does_not_consume_on_cancellation_before_first_byte() {
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([(
                "a".to_string(),
                MockProvider::new("a", vec![Ok(vec![response_start()])]) as Arc<_>,
            )]),
        });
        let registry = Arc::new(BreakerRegistry::with_system_clock());
        let breaker = open_on_single_failure_breaker_config();
        let policy = BreakerPolicy::from(&breaker);
        let chain = vec![target("a")];
        let ticket = ticket_for(chain, &registry, fast_retry_config(1), breaker);
        let caller = ResilientCaller::new(
            providers,
            Arc::clone(&registry),
            disabled_limiter(),
            disabled_probe(),
        );

        let stream = caller.complete(ticket, dummy_request()).await.unwrap();
        drop(stream);

        assert_eq!(
            registry.decision("a", "gpt-4o", &policy),
            BreakerDecision::Skip
        );
    }

    #[tokio::test]
    async fn complete_consumes_index_1_on_fallback() {
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([
                (
                    "a".to_string(),
                    MockProvider::new("a", vec![Err(ProviderError::Network("a-down".into()))])
                        as Arc<_>,
                ),
                (
                    "b".to_string(),
                    MockProvider::new("b", vec![Ok(vec![response_start()])]) as Arc<_>,
                ),
            ]),
        });
        let registry = Arc::new(BreakerRegistry::with_system_clock());
        let breaker = open_on_single_failure_breaker_config();
        let policy = BreakerPolicy::from(&breaker);
        let chain = vec![target("a"), target("b")];
        let ticket = ticket_for(chain, &registry, fast_retry_config(1), breaker);
        let caller = ResilientCaller::new(
            providers,
            Arc::clone(&registry),
            disabled_limiter(),
            disabled_probe(),
        );

        let mut stream = caller.complete(ticket, dummy_request()).await.unwrap();
        assert!(stream.next().await.unwrap().is_ok());
        drop(stream);

        assert_eq!(
            registry.decision("a", "gpt-4o", &policy),
            BreakerDecision::Skip
        );
        assert_eq!(
            registry.decision("b", "gpt-4o", &policy),
            BreakerDecision::Allow
        );
    }
}
