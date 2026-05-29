//! Admission gate for canonical requests.
//!
//! `Admission::admit` composes rate-limit checks, the cost filter, capability
//! gating, and breaker holds into one `AdmissionTicket`. PR 2 of ADR-0009 has
//! an intentional implementation asymmetry: `BreakerHold` is true RAII, while
//! `RateLimitReservation` is observation-only because the current governor
//! limiter consumes on check and cannot refund.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agent_shim_config::{GatewayConfig, RouteEntry};
use agent_shim_core::cost::ImageTokenEstimator;
use agent_shim_core::{BackendTarget, CanonicalRequest, ContentBlock};
use agent_shim_providers::ProviderError;
use thiserror::Error;

use crate::auth::AgentIdentity;
use crate::circuit_breaker::{BreakerHold, BreakerPolicy, BreakerRegistry};
use crate::cost_filter::{self, Skip};
use crate::errors::RateLimitDimension;
use crate::latency_probe::LatencyProbe;
use crate::rate_limit::{LimiterRegistry, RateLimitReservation};
use crate::resilient_caller::ProviderLookup;
use crate::ResolvedRoute;

/// RAII ticket proving a canonical request passed admission.
///
/// Breaker holds are true RAII in PR 2: consuming a chain index records that
/// index's outcome, while dropping unconsumed holds records abandoned probes.
/// Rate-limit reservations are present only as observation-only handles until
/// PR 4 replaces governor with reservation-aware buckets.
pub struct AdmissionTicket {
    filtered_chain: Vec<BackendTarget>,
    resolved: Arc<ResolvedRoute>,
    breaker_holds: Vec<Option<BreakerHold>>,
    rate_limit_reservations: Vec<RateLimitReservation>,
    consumed: AtomicBool,
}

impl AdmissionTicket {
    pub fn chain(&self) -> &[BackendTarget] {
        &self.filtered_chain
    }

    pub fn resolved(&self) -> &ResolvedRoute {
        &self.resolved
    }

    pub(crate) fn breaker_allowed(&self, chain_index: usize) -> bool {
        self.breaker_holds
            .get(chain_index)
            .and_then(Option::as_ref)
            .is_some()
    }

    /// Commit first-byte-from-upstream for a specific chain index.
    ///
    /// The call is idempotent. The selected breaker hold records `succeeded`;
    /// all other breaker holds are dropped as abandoned probes. Rate-limit
    /// reservation consume is a PR 2 no-op, but the caller-facing lifecycle is
    /// already shaped for PR 4.
    pub fn consume(&mut self, chain_index: usize, succeeded: bool) {
        if self.consumed.swap(true, Ordering::AcqRel) {
            return;
        }

        let holds = std::mem::take(&mut self.breaker_holds);
        for (i, hold) in holds.into_iter().enumerate() {
            if let Some(hold) = hold {
                if i == chain_index {
                    hold.consume(succeeded);
                }
            }
        }

        for reservation in std::mem::take(&mut self.rate_limit_reservations) {
            reservation.consume();
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        filtered_chain: Vec<BackendTarget>,
        resolved: ResolvedRoute,
        breaker_holds: Vec<Option<BreakerHold>>,
    ) -> Self {
        Self {
            filtered_chain,
            resolved: Arc::new(resolved),
            breaker_holds,
            rate_limit_reservations: Vec::new(),
            consumed: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    pub(crate) fn breaker_hold_count_for_test(&self) -> usize {
        self.breaker_holds
            .iter()
            .filter(|hold| hold.is_some())
            .count()
    }
}

#[derive(Debug, Error)]
pub enum AdmissionError {
    #[error("rate limited on {dimension:?}; retry after {retry_after_secs}s")]
    RateLimited {
        dimension: RateLimitDimension,
        retry_after_secs: u32,
    },
    #[error("no eligible upstream after cost filter")]
    NoEligibleUpstream { filtered: Vec<Skip> },
    #[error("capability mismatch: {0}")]
    CapabilityMismatch(String),
    #[error("all {} upstreams have open circuit breakers", tried.len())]
    AllBreakersOpen { tried: Vec<String> },
    #[error("provider error: {0}")]
    Provider(ProviderError),
}

pub struct Admission {
    limiter: Arc<arc_swap::ArcSwap<LimiterRegistry>>,
    breaker: Arc<BreakerRegistry>,
    providers: Arc<dyn ProviderLookup>,
    probe: Arc<dyn LatencyProbe>,
}

impl Admission {
    pub fn new(
        limiter: Arc<arc_swap::ArcSwap<LimiterRegistry>>,
        breaker: Arc<BreakerRegistry>,
        providers: Arc<dyn ProviderLookup>,
        probe: Arc<dyn LatencyProbe>,
    ) -> Self {
        Self {
            limiter,
            breaker,
            providers,
            probe,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn admit(
        &self,
        resolved: ResolvedRoute,
        route_entry: &RouteEntry,
        config: &GatewayConfig,
        request: &CanonicalRequest,
        identity: &AgentIdentity,
        client_ip: &str,
        image_estimator: &dyn ImageTokenEstimator,
    ) -> Result<AdmissionTicket, AdmissionError> {
        let limiter = self.limiter.load_full();
        let route_label = resolved.route_label.to_string();

        limiter
            .check_pre_chain(identity, &route_label, client_ip)
            .map_err(|(dimension, retry_after_secs)| {
                emit_rate_limit_rejected(dimension, retry_after_secs, identity.log_id());
                AdmissionError::RateLimited {
                    dimension,
                    retry_after_secs,
                }
            })?;
        let mut rate_limit_reservations = vec![RateLimitReservation {}];

        let filter_outcome = cost_filter::filter_chain(
            resolved.chain.clone(),
            route_entry,
            request,
            config,
            self.probe.as_ref(),
            image_estimator,
        );
        for skip in &filter_outcome.skipped {
            metrics::counter!(
                crate::metric_names::COST_FILTERED_TOTAL,
                "reason" => skip.reason.as_str(),
                "upstream" => skip.upstream.clone(),
                "route" => route_label.clone(),
            )
            .increment(1);
        }
        for note in &filter_outcome.notes {
            metrics::counter!(
                crate::metric_names::COST_FILTERED_TOTAL,
                "reason" => note.reason.as_str(),
                "upstream" => note.upstream.clone(),
                "route" => route_label.clone(),
            )
            .increment(1);
        }
        if filter_outcome.survivors.is_empty() {
            return Err(AdmissionError::NoEligibleUpstream {
                filtered: filter_outcome.skipped,
            });
        }
        let filtered_chain = filter_outcome.survivors;

        for target in &filtered_chain {
            limiter.check_per_upstream(&target.provider).map_err(
                |(dimension, retry_after_secs)| {
                    emit_rate_limit_rejected(dimension, retry_after_secs, identity.log_id());
                    AdmissionError::RateLimited {
                        dimension,
                        retry_after_secs,
                    }
                },
            )?;
            rate_limit_reservations.push(RateLimitReservation {});
        }

        self.check_capability(&filtered_chain[0], request)?;

        let breaker_policy = BreakerPolicy::from(&resolved.breaker);
        let mut breaker_holds = Vec::with_capacity(filtered_chain.len());
        let mut open_breakers = Vec::new();
        for target in &filtered_chain {
            match self
                .breaker
                .try_hold(&target.provider, &target.model, &breaker_policy)
            {
                Some(hold) => breaker_holds.push(Some(hold)),
                None => {
                    open_breakers.push(target.provider.clone());
                    breaker_holds.push(None);
                }
            }
        }

        if breaker_holds.iter().all(Option::is_none) {
            return Err(AdmissionError::AllBreakersOpen {
                tried: open_breakers,
            });
        }

        Ok(AdmissionTicket {
            filtered_chain,
            resolved: Arc::new(resolved),
            breaker_holds,
            rate_limit_reservations,
            consumed: AtomicBool::new(false),
        })
    }

    fn check_capability(
        &self,
        target: &BackendTarget,
        request: &CanonicalRequest,
    ) -> Result<(), AdmissionError> {
        let provider = self.providers.get(&target.provider).ok_or_else(|| {
            AdmissionError::Provider(ProviderError::UnknownProvider(target.provider.clone()))
        })?;
        if request_has_image(request) && !provider.capabilities().vision {
            return Err(AdmissionError::CapabilityMismatch(
                "target provider does not support vision (image blocks present in request)"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

fn request_has_image(request: &CanonicalRequest) -> bool {
    request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .any(|block| matches!(block, ContentBlock::Image(_)))
}

fn emit_rate_limit_rejected(dimension: RateLimitDimension, retry_after_secs: u32, identity: &str) {
    tracing::warn!(
        target: "agent_shim::resilience",
        event_name = "rate_limit.rejected",
        identity = %identity,
        dimension = dimension_label(dimension),
        retry_after_secs = retry_after_secs,
        "rate limit exceeded during admission"
    );
    metrics::counter!(
        crate::metric_names::RATE_LIMIT_REJECTED_TOTAL,
        "dimension" => dimension_label(dimension),
    )
    .increment(1);
}

fn dimension_label(dimension: RateLimitDimension) -> &'static str {
    match dimension {
        RateLimitDimension::PerKey => "per_key",
        RateLimitDimension::PerRoute => "per_route",
        RateLimitDimension::PerUpstream => "per_upstream",
        RateLimitDimension::PerIp => "per_ip",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::circuit_breaker::BreakerDecision;
    use agent_shim_config::{
        BreakerConfig, BucketConfigYaml, OpenAiCompatibleUpstream, PerIpConfig, PerKeyConfig,
        RateLimitConfig, Secret, Tier, UpstreamConfig,
    };
    use agent_shim_core::{
        media::BinarySource, request::RequestMetadata, CanonicalStream, ExtensionMap, FrontendInfo,
        FrontendKind, FrontendModel, GenerationOptions, ImageBlock, Message, RequestId,
        ResolvedPolicy,
    };
    use agent_shim_providers::{BackendProvider, ProviderCapabilities};
    use async_trait::async_trait;
    use std::collections::{BTreeMap, HashMap};

    struct InMemoryProviders {
        map: HashMap<String, Arc<dyn BackendProvider>>,
    }

    impl ProviderLookup for InMemoryProviders {
        fn get(&self, provider: &str) -> Option<Arc<dyn BackendProvider>> {
            self.map.get(provider).cloned()
        }
    }

    struct MockProvider {
        capabilities: ProviderCapabilities,
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
            panic!("admission tests should not call providers")
        }
    }

    fn provider_lookup(
        entries: impl IntoIterator<Item = (&'static str, ProviderCapabilities)>,
    ) -> Arc<dyn ProviderLookup> {
        Arc::new(InMemoryProviders {
            map: entries
                .into_iter()
                .map(|(name, capabilities)| {
                    (
                        name.to_string(),
                        Arc::new(MockProvider { capabilities }) as Arc<dyn BackendProvider>,
                    )
                })
                .collect(),
        })
    }

    fn cfg_with_route_and_upstreams(
        route: RouteEntry,
        upstreams: impl IntoIterator<Item = (&'static str, Tier)>,
    ) -> GatewayConfig {
        let mut config = GatewayConfig {
            server: Default::default(),
            logging: Default::default(),
            upstreams: Default::default(),
            routes: vec![route],
            plugins: BTreeMap::new(),
            auth: Default::default(),
            rate_limit: Default::default(),
            copilot: None,
            admin: None,
            metrics: Default::default(),
            otel: None,
            shutdown: Default::default(),
            validation: Default::default(),
        };
        for (name, tier) in upstreams {
            config.upstreams.insert(
                name.to_string(),
                UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
                    base_url: "http://localhost".into(),
                    api_key: Secret::new("dummy"),
                    default_headers: Default::default(),
                    request_timeout_secs: 30,
                    tier,
                    cost: None,
                    p95_latency_budget_ms: None,
                }),
            );
        }
        config
    }

    fn resolved_route(chain: Vec<BackendTarget>, breaker: BreakerConfig) -> ResolvedRoute {
        ResolvedRoute {
            chain,
            retry: Default::default(),
            breaker,
            route_label: Arc::from("openai_chat/gpt-4o"),
        }
    }

    fn target(provider: &str) -> BackendTarget {
        BackendTarget {
            provider: provider.into(),
            model: "gpt-4o".into(),
            policy: Default::default(),
        }
    }

    fn text_request() -> CanonicalRequest {
        request_with_content(vec![ContentBlock::text("hi")])
    }

    fn image_request() -> CanonicalRequest {
        request_with_content(vec![ContentBlock::Image(ImageBlock {
            source: BinarySource::Url {
                url: "https://example.test/image.png".into(),
            },
            extensions: Default::default(),
        })])
    }

    fn request_with_content(content: Vec<ContentBlock>) -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::OpenAiChat,
                requested_model: FrontendModel::from("gpt-4o"),
            },
            model: FrontendModel::from("gpt-4o"),
            system: vec![],
            messages: vec![Message::user(content)],
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

    fn disabled_limiters() -> Arc<arc_swap::ArcSwap<LimiterRegistry>> {
        Arc::new(arc_swap::ArcSwap::from_pointee(LimiterRegistry::disabled()))
    }

    fn one_token_limiters() -> Arc<arc_swap::ArcSwap<LimiterRegistry>> {
        let bucket = BucketConfigYaml {
            rate_per_sec: 1,
            burst: 1,
        };
        let cfg = RateLimitConfig {
            enabled: true,
            per_key: PerKeyConfig {
                default: Some(bucket.clone()),
                anonymous: Some(bucket),
                overrides: BTreeMap::new(),
            },
            per_route: BTreeMap::new(),
            per_upstream: BTreeMap::new(),
            per_ip: PerIpConfig {
                enabled: false,
                rate_per_sec: 1,
                burst: 1,
            },
        };
        Arc::new(arc_swap::ArcSwap::from_pointee(
            LimiterRegistry::from_config(&cfg),
        ))
    }

    fn one_sample_breaker_config() -> BreakerConfig {
        BreakerConfig {
            enabled: true,
            failure_threshold_pct: 50,
            min_requests: 1,
            window_secs: 60,
            open_cooldown_secs: 30,
        }
    }

    fn admission(
        limiters: Arc<arc_swap::ArcSwap<LimiterRegistry>>,
        breakers: Arc<BreakerRegistry>,
        providers: Arc<dyn ProviderLookup>,
    ) -> Admission {
        Admission::new(
            limiters,
            breakers,
            providers,
            Arc::new(crate::latency_probe::DisabledLatencyProbe),
        )
    }

    #[test]
    fn admit_succeeds_returns_ticket_with_filtered_chain() {
        let mut route = RouteEntry::singular("openai_chat", "gpt-4o", "economy", "gpt-4o");
        route.min_tier = Some(Tier::Standard);
        let config = cfg_with_route_and_upstreams(
            route,
            [
                ("economy", Tier::Economy),
                ("standard", Tier::Standard),
                ("premium", Tier::Premium),
            ],
        );
        let providers = provider_lookup([
            ("standard", ProviderCapabilities::default()),
            ("premium", ProviderCapabilities::default()),
        ]);
        let breakers = Arc::new(BreakerRegistry::with_system_clock());

        let ticket = admission(disabled_limiters(), Arc::clone(&breakers), providers)
            .admit(
                resolved_route(
                    vec![target("economy"), target("standard"), target("premium")],
                    one_sample_breaker_config(),
                ),
                &config.routes[0],
                &config,
                &text_request(),
                &AgentIdentity::Anonymous,
                "127.0.0.1",
                &crate::image_estimators::AnthropicImageEstimator,
            )
            .expect("admission should pass");

        let providers: Vec<&str> = ticket.chain().iter().map(|t| t.provider.as_str()).collect();
        assert_eq!(providers, vec!["standard", "premium"]);
        assert_eq!(ticket.breaker_hold_count_for_test(), 2);
    }

    #[test]
    fn admit_rejects_when_rate_limited_pre_chain() {
        let route = RouteEntry::singular("openai_chat", "gpt-4o", "openai", "gpt-4o");
        let config = cfg_with_route_and_upstreams(route, [("openai", Tier::Standard)]);
        let limiters = one_token_limiters();
        assert!(limiters
            .load_full()
            .check_pre_chain(&AgentIdentity::Anonymous, "openai_chat/gpt-4o", "127.0.0.1")
            .is_ok());

        let err = admission(
            limiters,
            Arc::new(BreakerRegistry::with_system_clock()),
            provider_lookup([("openai", ProviderCapabilities::default())]),
        )
        .admit(
            resolved_route(vec![target("openai")], one_sample_breaker_config()),
            &config.routes[0],
            &config,
            &text_request(),
            &AgentIdentity::Anonymous,
            "127.0.0.1",
            &crate::image_estimators::AnthropicImageEstimator,
        )
        .err()
        .expect("admission should reject rate-limited request");

        assert!(matches!(err, AdmissionError::RateLimited { .. }));
    }

    #[test]
    fn admit_rejects_when_cost_filter_empties_chain() {
        let mut route = RouteEntry::singular("openai_chat", "gpt-4o", "a", "gpt-4o");
        route.min_tier = Some(Tier::Premium);
        let config =
            cfg_with_route_and_upstreams(route, [("a", Tier::Economy), ("b", Tier::Standard)]);

        let err = admission(
            disabled_limiters(),
            Arc::new(BreakerRegistry::with_system_clock()),
            provider_lookup([
                ("a", ProviderCapabilities::default()),
                ("b", ProviderCapabilities::default()),
            ]),
        )
        .admit(
            resolved_route(vec![target("a"), target("b")], one_sample_breaker_config()),
            &config.routes[0],
            &config,
            &text_request(),
            &AgentIdentity::Anonymous,
            "127.0.0.1",
            &crate::image_estimators::AnthropicImageEstimator,
        )
        .err()
        .expect("admission should reject empty cost-filter result");

        assert!(matches!(err, AdmissionError::NoEligibleUpstream { .. }));
    }

    #[test]
    fn admit_rejects_on_capability_mismatch() {
        let route = RouteEntry::singular("openai_chat", "gpt-4o", "openai", "gpt-4o");
        let config = cfg_with_route_and_upstreams(route, [("openai", Tier::Standard)]);

        let err = admission(
            disabled_limiters(),
            Arc::new(BreakerRegistry::with_system_clock()),
            provider_lookup([("openai", ProviderCapabilities::default())]),
        )
        .admit(
            resolved_route(vec![target("openai")], one_sample_breaker_config()),
            &config.routes[0],
            &config,
            &image_request(),
            &AgentIdentity::Anonymous,
            "127.0.0.1",
            &crate::image_estimators::AnthropicImageEstimator,
        )
        .err()
        .expect("admission should reject capability mismatch");

        assert!(matches!(err, AdmissionError::CapabilityMismatch(_)));
    }

    #[test]
    fn admit_rejects_when_all_breakers_open() {
        let route = RouteEntry::singular("openai_chat", "gpt-4o", "a", "gpt-4o");
        let config =
            cfg_with_route_and_upstreams(route, [("a", Tier::Standard), ("b", Tier::Standard)]);
        let breakers = Arc::new(BreakerRegistry::with_system_clock());
        let breaker = one_sample_breaker_config();
        let policy = BreakerPolicy::from(&breaker);
        breakers.record("a", "gpt-4o", false, &policy);
        breakers.record("b", "gpt-4o", false, &policy);

        let err = admission(
            disabled_limiters(),
            breakers,
            provider_lookup([
                ("a", ProviderCapabilities::default()),
                ("b", ProviderCapabilities::default()),
            ]),
        )
        .admit(
            resolved_route(vec![target("a"), target("b")], breaker),
            &config.routes[0],
            &config,
            &text_request(),
            &AgentIdentity::Anonymous,
            "127.0.0.1",
            &crate::image_estimators::AnthropicImageEstimator,
        )
        .err()
        .expect("admission should reject all open breakers");

        assert!(matches!(err, AdmissionError::AllBreakersOpen { .. }));
    }

    #[test]
    fn ticket_consume_idempotent() {
        let route = RouteEntry::singular("openai_chat", "gpt-4o", "a", "gpt-4o");
        let config = cfg_with_route_and_upstreams(route, [("a", Tier::Standard)]);
        let breakers = Arc::new(BreakerRegistry::with_system_clock());
        let breaker = one_sample_breaker_config();
        let policy = BreakerPolicy::from(&breaker);
        let mut ticket = admission(
            disabled_limiters(),
            Arc::clone(&breakers),
            provider_lookup([("a", ProviderCapabilities::default())]),
        )
        .admit(
            resolved_route(vec![target("a")], breaker),
            &config.routes[0],
            &config,
            &text_request(),
            &AgentIdentity::Anonymous,
            "127.0.0.1",
            &crate::image_estimators::AnthropicImageEstimator,
        )
        .unwrap();

        ticket.consume(0, true);
        ticket.consume(0, true);
        drop(ticket);

        assert_eq!(
            breakers.decision("a", "gpt-4o", &policy),
            BreakerDecision::Allow
        );
    }

    #[test]
    fn ticket_drop_without_consume_records_abandoned_probes() {
        let route = RouteEntry::singular("openai_chat", "gpt-4o", "a", "gpt-4o");
        let config =
            cfg_with_route_and_upstreams(route, [("a", Tier::Standard), ("b", Tier::Standard)]);
        let breakers = Arc::new(BreakerRegistry::with_system_clock());
        let breaker = one_sample_breaker_config();
        let policy = BreakerPolicy::from(&breaker);
        let ticket = admission(
            disabled_limiters(),
            Arc::clone(&breakers),
            provider_lookup([
                ("a", ProviderCapabilities::default()),
                ("b", ProviderCapabilities::default()),
            ]),
        )
        .admit(
            resolved_route(vec![target("a"), target("b")], breaker),
            &config.routes[0],
            &config,
            &text_request(),
            &AgentIdentity::Anonymous,
            "127.0.0.1",
            &crate::image_estimators::AnthropicImageEstimator,
        )
        .unwrap();

        drop(ticket);

        assert_eq!(
            breakers.decision("a", "gpt-4o", &policy),
            BreakerDecision::Skip
        );
        assert_eq!(
            breakers.decision("b", "gpt-4o", &policy),
            BreakerDecision::Skip
        );
    }

    #[test]
    fn ticket_consume_records_chosen_index_and_abandons_others() {
        let route = RouteEntry::singular("openai_chat", "gpt-4o", "a", "gpt-4o");
        let config = cfg_with_route_and_upstreams(
            route,
            [
                ("a", Tier::Standard),
                ("b", Tier::Standard),
                ("c", Tier::Standard),
            ],
        );
        let breakers = Arc::new(BreakerRegistry::with_system_clock());
        let breaker = one_sample_breaker_config();
        let policy = BreakerPolicy::from(&breaker);
        let mut ticket = admission(
            disabled_limiters(),
            Arc::clone(&breakers),
            provider_lookup([
                ("a", ProviderCapabilities::default()),
                ("b", ProviderCapabilities::default()),
                ("c", ProviderCapabilities::default()),
            ]),
        )
        .admit(
            resolved_route(vec![target("a"), target("b"), target("c")], breaker),
            &config.routes[0],
            &config,
            &text_request(),
            &AgentIdentity::Anonymous,
            "127.0.0.1",
            &crate::image_estimators::AnthropicImageEstimator,
        )
        .unwrap();

        ticket.consume(1, true);
        drop(ticket);

        assert_eq!(
            breakers.decision("a", "gpt-4o", &policy),
            BreakerDecision::Skip
        );
        assert_eq!(
            breakers.decision("b", "gpt-4o", &policy),
            BreakerDecision::Allow
        );
        assert_eq!(
            breakers.decision("c", "gpt-4o", &policy),
            BreakerDecision::Skip
        );
    }
}
