#![forbid(unsafe_code)]

pub mod circuit_breaker;
pub mod fallback;
pub mod model_index;
pub mod rate_limit;
pub mod resolver;
pub mod retry;
pub mod static_routes;

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
}

pub use fallback::{
    fallback_eligibility, fallback_eligibility_with_overrides, FallbackEligibility,
};
pub use resolver::ModelResolver;
pub use retry::{compute_backoff, retry_with_policy, RetryPolicy};
pub use static_routes::StaticRouter;
