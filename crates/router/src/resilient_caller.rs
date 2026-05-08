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
}

impl ResilientCaller {
    pub fn new(providers: Arc<dyn ProviderLookup>) -> Self {
        Self { providers }
    }

    /// Walk the chain. Each element gets its own retry budget. Fallback
    /// to chain[i+1] only on retry exhaustion with a fallback-eligible
    /// error; terminal errors short-circuit.
    pub async fn complete(
        &self,
        chain: Vec<BackendTarget>,
        req: CanonicalRequest,
        policies: Vec<RetryPolicy>,
    ) -> Result<CanonicalStream, ResilienceError> {
        debug_assert_eq!(chain.len(), policies.len(), "one policy per chain element");
        let mut tried: Vec<TriedUpstream> = Vec::new();
        let mut last_error: Option<ProviderError> = None;

        for (i, target) in chain.iter().enumerate() {
            let policy = &policies[i];
            let provider = match self.providers.get(&target.provider) {
                Some(p) => p,
                None => {
                    let e = ProviderError::UnknownProvider(target.provider.clone());
                    return Err(ResilienceError::TerminalError { error: e, tried });
                }
            };

            let started = Instant::now();
            // retry_with_policy returns Err on retry exhaustion OR terminal.
            let result = retry_with_policy(provider, target.clone(), req.clone(), policy).await;
            let elapsed_ms = started.elapsed().as_millis() as u64;

            match result {
                Ok(stream) => {
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
                    let eligibility = fallback_eligibility_with_overrides(&e, &policy.retry_on);
                    let tag = crate::fallback::error_tag(&e);
                    tried.push(TriedUpstream {
                        provider: target.provider.clone(),
                        model: target.model.clone(),
                        attempts: policy.max_attempts, // upper bound; real count
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

        Err(ResilienceError::NoUpstreamSucceeded {
            tried,
            last_error: last_error.expect("loop only exits with last_error set on Err path"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        request::RequestMetadata, ContentBlock, ExtensionMap, FrontendInfo, FrontendKind,
        FrontendModel, GenerationOptions, Message, RequestId, ResolvedPolicy,
    };
    use agent_shim_providers::ProviderCapabilities;
    use async_trait::async_trait;
    use futures::stream;
    use std::collections::HashMap;
    use std::sync::Mutex;

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
        let caller = ResilientCaller::new(providers);
        let chain = vec![target("a")];
        let policies = vec![fast_policy(2)];
        let result = caller.complete(chain, dummy_request(), policies).await;
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
        let caller = ResilientCaller::new(providers);
        let chain = vec![target("a"), target("b")];
        let policies = vec![fast_policy(2), fast_policy(2)];
        let result = caller.complete(chain, dummy_request(), policies).await;
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
        let caller = ResilientCaller::new(providers);
        let chain = vec![target("a"), target("b")];
        let policies = vec![fast_policy(2), fast_policy(2)];
        let result = caller.complete(chain, dummy_request(), policies).await;
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
        let caller = ResilientCaller::new(providers);
        let chain = vec![target("a"), target("b")];
        let policies = vec![fast_policy(2), fast_policy(2)];
        let result = caller.complete(chain, dummy_request(), policies).await;
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
}
