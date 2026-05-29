#![forbid(unsafe_code)]

pub mod auth;
pub mod circuit_breaker;
pub mod cost_estimate;
pub mod cost_filter;
pub mod errors;
pub mod fallback;
pub mod image_estimators;
pub mod latency_probe;
pub mod model_index;
mod policy_vec;
pub mod rate_limit;
pub mod resilient_caller;
pub mod resolver;
pub mod retry;
pub mod static_routes;

/// Metric name constants the router crate emits via the `metrics` facade.
/// Mirrors `agent_shim_observability::metrics::names` — duplicated here
/// because the router crate doesn't depend on observability (it's a
/// lower layer). Plan 02 P02 T4.
pub(crate) mod metric_names {
    pub const RETRY_ATTEMPTS_TOTAL: &str = "agent_shim_retry_attempts_total";
    pub const RETRY_EXHAUSTED_TOTAL: &str = "agent_shim_retry_exhausted_total";
    pub const FALLBACK_TRANSITIONS_TOTAL: &str = "agent_shim_fallback_transitions_total";
    pub const BREAKER_STATE_CHANGES_TOTAL: &str = "agent_shim_breaker_state_changes_total";
    pub const RATE_LIMIT_REJECTED_TOTAL: &str = "agent_shim_rate_limit_rejected_total";
    /// Plan 06 P04 T4: per-axis cost-filter skip/note counter.
    pub const COST_FILTERED_TOTAL: &str = "agent_shim_cost_filtered_total";
}

use std::sync::Arc;

use thiserror::Error;

use agent_shim_config::{BreakerConfig, RetryConfig};
use agent_shim_core::{BackendTarget, FrontendKind};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RouteError {
    #[error("no route for frontend={frontend:?} model={model}")]
    NoRoute {
        frontend: FrontendKind,
        model: String,
    },
}

#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    pub chain: Vec<BackendTarget>,
    pub retry: RetryConfig,
    pub breaker: BreakerConfig,
    pub route_label: Arc<str>,
}

pub trait Router: Send + Sync {
    /// Resolve `(frontend, model)` to the full fallback chain.
    ///
    /// Returns a vec of `BackendTarget`s in the order they should be
    /// attempted. A v0.3 singular `RouteEntry` produces a 1-element vec;
    /// a v0.4 array-form route produces N elements in configured order.
    /// Callers (e.g. `ResilientCaller`) walk the chain head-to-tail,
    /// failing over from element[i] to element[i+1] on eligible errors.
    fn resolve(
        &self,
        frontend: FrontendKind,
        model: &str,
    ) -> Result<Vec<BackendTarget>, RouteError>;

    /// Enumerate explicit `(frontend, alias)` pairs for the catalog
    /// endpoints. Wildcard routes (`model: "*"`) are excluded — they don't
    /// enumerate a concrete alias.
    ///
    /// Default impl returns an empty vec so existing dynamic / test routers
    /// keep compiling without a catalog. `StaticRouter` overrides this with
    /// the explicit list of configured aliases.
    fn list_routes(&self) -> Vec<(FrontendKind, String)> {
        Vec::new()
    }
}

pub use auth::{extract_identity_from_headers, hash_key, AgentIdentity};
pub use circuit_breaker::{BreakerDecision, BreakerPolicy, BreakerRegistry, Clock, SystemClock};
pub use cost_estimate::{estimate_request_cost, CostEstimate};
pub use cost_filter::{filter_chain, FilterOutcome, FilterReason, Note, Skip};
pub use errors::{RateLimitDimension, ResilienceError, TriedUpstream};
pub use fallback::{
    fallback_eligibility, fallback_eligibility_with_overrides, FallbackEligibility,
};
pub use image_estimators::{
    AnthropicImageEstimator, OpenAiImageEstimator, ResponsesImageEstimator,
};
pub use latency_probe::{DisabledLatencyProbe, LatencyProbe, MockLatencyProbe};
pub use rate_limit::{BucketConfig, LimitOutcome, LimiterRegistry};
pub use resilient_caller::{CostFilterInputs, ProviderLookup, ResilientCaller};
pub use resolver::ModelResolver;
pub use retry::{compute_backoff, retry_with_policy, RetryOutcome, RetryPolicy};
pub use static_routes::StaticRouter;
