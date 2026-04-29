#![forbid(unsafe_code)]

pub mod static_routes;
pub mod fallback;
pub mod rate_limit;
pub mod circuit_breaker;
pub mod model_index;

use thiserror::Error;

use agent_shim_core::{BackendTarget, FrontendKind};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RouteError {
    #[error("no route for frontend={frontend:?} model={model}")]
    NoRoute { frontend: FrontendKind, model: String },
}

pub trait Router: Send + Sync {
    fn resolve(&self, frontend: FrontendKind, model: &str) -> Result<BackendTarget, RouteError>;
}

pub use static_routes::StaticRouter;
