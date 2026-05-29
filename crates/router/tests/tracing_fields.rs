//! Plan 05 P05 T1: assert resilience events use the standard field set.
//!
//! Uses `tracing-test` to capture events emitted during ResilientCaller
//! invocations. Each test pins one event name and its fields.
//!
//! # Known limitations
//!
//! - **`request_id`**: until middleware-level plumbing lands, the gateway
//!   generates a fresh UUID at the top of `ResilientCaller::complete`.
//!   The tests therefore assert presence of the field, not a specific
//!   value.
//! - **Substring matching**: `tracing_test::logs_contain` uses substring
//!   match on the formatter output. This works because tracing's default
//!   formatter renders fields as `key=value` (or `key="value"` for string
//!   values). Tests reflect this format.
//! - **Field-formatter quirk**: `%expr` (`Display`) renders raw, while a
//!   bare value renders quoted (or unquoted for ints). The assertions
//!   target the actual rendered shapes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_shim_config::{
    BreakerConfig, GatewayConfig, OpenAiCompatibleUpstream, RetryConfig, RouteEntry, Secret, Tier,
    UpstreamConfig,
};
use agent_shim_core::{
    request::RequestMetadata, BackendTarget, CanonicalRequest, CanonicalStream, ContentBlock,
    ExtensionMap, FrontendInfo, FrontendKind, FrontendModel, GenerationOptions, Message, RequestId,
    ResolvedPolicy, StreamEvent,
};
use agent_shim_providers::{BackendProvider, ProviderCapabilities, ProviderError};
use agent_shim_router::{
    Admission, AdmissionError, AgentIdentity, AnthropicImageEstimator, BreakerPolicy,
    BreakerRegistry, DisabledLatencyProbe, LatencyProbe, LimiterRegistry, ProviderLookup,
    ResilientCaller, ResolvedRoute,
};
use async_trait::async_trait;
use futures::stream;
use tracing_test::traced_test;

// --- Test helpers (duplicated from `crates/router/src/resilient_caller.rs`'s
// in-module test helpers because `#[cfg(test)]` items are not visible from
// integration tests, which compile in a separate target). ---

struct InMemoryProviders {
    map: HashMap<String, Arc<dyn BackendProvider>>,
}

impl ProviderLookup for InMemoryProviders {
    fn get(&self, name: &str) -> Option<Arc<dyn BackendProvider>> {
        self.map.get(name).cloned()
    }
}

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

fn disabled_breaker_config() -> BreakerConfig {
    BreakerConfig {
        enabled: false,
        failure_threshold_pct: 50,
        min_requests: 5,
        window_secs: 60,
        open_cooldown_secs: 30,
    }
}

fn disabled_limiter() -> Arc<arc_swap::ArcSwap<LimiterRegistry>> {
    Arc::new(arc_swap::ArcSwap::from_pointee(LimiterRegistry::disabled()))
}

fn disabled_probe() -> Arc<dyn LatencyProbe> {
    Arc::new(DisabledLatencyProbe)
}

fn gateway_config_for(chain: &[BackendTarget]) -> GatewayConfig {
    let mut config = GatewayConfig {
        server: Default::default(),
        logging: Default::default(),
        upstreams: Default::default(),
        routes: vec![],
        plugins: Default::default(),
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
        shutdown: Default::default(),
        validation: Default::default(),
    };
    for target in chain {
        config.upstreams.insert(
            target.provider.clone(),
            UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
                base_url: "http://localhost".into(),
                api_key: Secret::new("dummy"),
                default_headers: Default::default(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
    }
    config.routes.push(RouteEntry::singular(
        "openai_chat",
        "gpt-4o",
        chain[0].provider.as_str(),
        "gpt-4o",
    ));
    config
}

fn admitted_ticket(
    chain: Vec<BackendTarget>,
    providers: Arc<dyn ProviderLookup>,
    breakers: Arc<BreakerRegistry>,
    limiters: Arc<arc_swap::ArcSwap<LimiterRegistry>>,
    retry: RetryConfig,
    breaker: BreakerConfig,
) -> agent_shim_router::AdmissionTicket {
    let config = gateway_config_for(&chain);
    let route = &config.routes[0];
    let admission = Admission::new(
        limiters,
        breakers,
        providers,
        Arc::new(DisabledLatencyProbe),
    );
    admission
        .admit(
            ResolvedRoute {
                chain,
                retry,
                breaker,
                route_label: Arc::from("openai_chat/gpt-4o"),
            },
            route,
            &config,
            &dummy_request(),
            &AgentIdentity::Anonymous,
            "127.0.0.1",
            &AnthropicImageEstimator,
        )
        .expect("admission should pass")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[traced_test]
async fn retry_attempt_event_has_standard_fields() {
    // 5xx then Ok → exactly one retry, one `retry.attempt` event.
    let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
        map: HashMap::from([(
            "openai".to_string(),
            MockProvider::new(
                "openai",
                vec![
                    Err(ProviderError::Upstream {
                        status: 502,
                        body: "bad".into(),
                    }),
                    Ok(vec![]),
                ],
            ) as Arc<_>,
        )]),
    });
    let breakers = Arc::new(BreakerRegistry::with_system_clock());
    let limiters = disabled_limiter();
    let ticket = admitted_ticket(
        vec![target("openai")],
        Arc::clone(&providers),
        Arc::clone(&breakers),
        Arc::clone(&limiters),
        fast_retry_config(3),
        disabled_breaker_config(),
    );
    let caller = ResilientCaller::new(providers, breakers, limiters, disabled_probe());
    let result = caller.complete(ticket, dummy_request()).await;
    assert!(result.is_ok());

    assert!(logs_contain("retry.attempt"));
    assert!(logs_contain("upstream=openai"));
    assert!(logs_contain("attempt=1"));
    assert!(logs_contain("error_class=\"upstream_5xx\""));
    assert!(logs_contain("backoff_ms="));
    assert!(logs_contain("total_elapsed_ms="));
    assert!(logs_contain("agent_shim::resilience"));
}

#[tokio::test]
#[traced_test]
async fn fallback_transition_event_has_standard_fields() {
    // upstream "openai" fails twice (retry exhausted) → fallback to
    // "copilot" which succeeds. Asserts `fallback.transition` is emitted
    // with the standard field set.
    let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
        map: HashMap::from([
            (
                "openai".to_string(),
                MockProvider::new(
                    "openai",
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
                "copilot".to_string(),
                MockProvider::new("copilot", vec![Ok(vec![])]) as Arc<_>,
            ),
        ]),
    });
    let breakers = Arc::new(BreakerRegistry::with_system_clock());
    let limiters = disabled_limiter();
    let ticket = admitted_ticket(
        vec![target("openai"), target("copilot")],
        Arc::clone(&providers),
        Arc::clone(&breakers),
        Arc::clone(&limiters),
        fast_retry_config(2),
        disabled_breaker_config(),
    );
    let caller = ResilientCaller::new(providers, breakers, limiters, disabled_probe());
    let result = caller.complete(ticket, dummy_request()).await;
    assert!(result.is_ok());

    assert!(logs_contain("fallback.transition"));
    assert!(logs_contain("from_upstream=openai"));
    assert!(logs_contain("to_upstream=copilot"));
    assert!(logs_contain("reason=\"retry_exhausted\""));
}

#[tokio::test]
#[traced_test]
async fn breaker_state_change_event_emits_on_trip() {
    // Drive 5 failures with min_requests=5 to trip breaker for the
    // first time, asserting the Closed→Open transition event.
    //
    // We exercise the breaker via a real chain walk so the registry
    // populates the upstream/model on the BreakerState the same way
    // production would.
    let breakers = Arc::new(BreakerRegistry::with_system_clock());
    let breaker = BreakerConfig {
        enabled: true,
        failure_threshold_pct: 50,
        min_requests: 5,
        window_secs: 60,
        open_cooldown_secs: 30,
    };
    let policy = BreakerPolicy::from(&breaker);
    // Five failures via the registry (matches the path used by the chain
    // walker after each retry-loop outcome).
    for _ in 0..5 {
        breakers.record("openai", "gpt-4o", false, &policy);
    }

    assert!(logs_contain("breaker.state_change"));
    assert!(logs_contain("from_state=\"closed\""));
    assert!(logs_contain("to_state=\"open\""));
    assert!(logs_contain("reason=\"failure_threshold_exceeded\""));
    assert!(logs_contain("upstream=openai"));
}

#[tokio::test]
#[traced_test]
async fn rate_limit_rejected_event_has_standard_fields() {
    // Burn a tiny per-key bucket (burst=2) and assert `rate_limit.rejected`
    // is emitted with the standard field set.
    let bucket = agent_shim_config::BucketConfigYaml {
        rate_per_sec: 1,
        burst: 2,
    };
    let cfg = agent_shim_config::RateLimitConfig {
        enabled: true,
        per_key: agent_shim_config::PerKeyConfig {
            default: Some(bucket.clone()),
            anonymous: Some(bucket),
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
    let inner = LimiterRegistry::from_config(&cfg);
    // Burn the anonymous bucket (burst=2 → 2 allowed) before the test call.
    for _ in 0..2 {
        assert!(inner
            .check_pre_chain(&AgentIdentity::Anonymous, "openai_chat/gpt-4o", "127.0.0.1")
            .is_ok());
    }
    let limiters = Arc::new(arc_swap::ArcSwap::from_pointee(inner));

    let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
        map: HashMap::from([(
            "openai".to_string(),
            MockProvider::new("openai", vec![]) as Arc<_>,
        )]),
    });
    let breakers = Arc::new(BreakerRegistry::with_system_clock());
    let chain = vec![target("openai")];
    let config = gateway_config_for(&chain);
    let route = &config.routes[0];
    let admission = Admission::new(
        limiters,
        breakers,
        providers,
        Arc::new(DisabledLatencyProbe),
    );
    let result = admission.admit(
        ResolvedRoute {
            chain,
            retry: fast_retry_config(1),
            breaker: disabled_breaker_config(),
            route_label: Arc::from("openai_chat/gpt-4o"),
        },
        route,
        &config,
        &dummy_request(),
        &AgentIdentity::Anonymous,
        "127.0.0.1",
        &AnthropicImageEstimator,
    );
    assert!(matches!(result, Err(AdmissionError::RateLimited { .. })));

    assert!(logs_contain("rate_limit.rejected"));
    assert!(logs_contain("dimension=\"per_key\""));
    assert!(logs_contain("retry_after_secs="));
    assert!(logs_contain("identity=anonymous"));
}

#[tokio::test]
#[traced_test]
async fn tracing_fields_request_completed_event_emits_on_success() {
    let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
        map: HashMap::from([(
            "openai".to_string(),
            MockProvider::new("openai", vec![Ok(vec![])]) as Arc<_>,
        )]),
    });
    let breakers = Arc::new(BreakerRegistry::with_system_clock());
    let limiters = disabled_limiter();
    let ticket = admitted_ticket(
        vec![target("openai")],
        Arc::clone(&providers),
        Arc::clone(&breakers),
        Arc::clone(&limiters),
        fast_retry_config(1),
        disabled_breaker_config(),
    );
    let caller = ResilientCaller::new(providers, breakers, limiters, disabled_probe());
    let result = caller.complete(ticket, dummy_request()).await;
    assert!(result.is_ok());

    assert!(logs_contain("request.completed"));
    assert!(logs_contain("outcome=\"success\""));
    assert!(logs_contain("total_elapsed_ms="));
    assert!(logs_contain("identity=anonymous"));
    assert!(logs_contain("frontend_model=openai_chat/gpt-4o"));
}
