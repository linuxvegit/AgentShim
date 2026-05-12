#![forbid(unsafe_code)]

pub mod auth;
pub mod circuit_breaker;
pub mod errors;
pub mod fallback;
pub mod latency_probe;
pub mod model_index;
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
}

use thiserror::Error;

use agent_shim_config::RetryConfig;
use agent_shim_core::{BackendTarget, FrontendKind};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RouteError {
    #[error("no route for frontend={frontend:?} model={model}")]
    NoRoute {
        frontend: FrontendKind,
        model: String,
    },
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

    /// Look up the per-route retry policy. Returns `None` if no route entry
    /// matched (callers should fall back to `RetryConfig::default()`).
    ///
    /// Default impl returns `None` so existing test routers (and any future
    /// dynamic router impl) keep compiling without retry support; the static
    /// router overrides this with a real lookup against the config table.
    fn find_retry_policy(&self, _frontend: FrontendKind, _model: &str) -> Option<RetryConfig> {
        None
    }

    /// Look up the per-route breaker policy. Returns `None` if no route entry
    /// matched (callers should fall back to `BreakerConfig::default()`).
    ///
    /// Default impl returns `None` for parity with `find_retry_policy` —
    /// keeps test routers compiling while `StaticRouter` overrides with a
    /// real lookup.
    fn find_breaker_policy(
        &self,
        _frontend: FrontendKind,
        _model: &str,
    ) -> Option<agent_shim_config::BreakerConfig> {
        None
    }
}

pub use auth::{extract_identity_from_headers, hash_key, AgentIdentity};
pub use circuit_breaker::{BreakerDecision, BreakerPolicy, BreakerRegistry, Clock, SystemClock};
pub use errors::{RateLimitDimension, ResilienceError, TriedUpstream};
pub use fallback::{
    fallback_eligibility, fallback_eligibility_with_overrides, FallbackEligibility,
};
pub use latency_probe::{DisabledLatencyProbe, LatencyProbe, MockLatencyProbe};
pub use rate_limit::{BucketConfig, LimitOutcome, LimiterRegistry};
pub use resilient_caller::{ProviderLookup, ResilientCaller};
pub use resolver::ModelResolver;
pub use retry::{compute_backoff, retry_with_policy, RetryOutcome, RetryPolicy};
pub use static_routes::StaticRouter;
