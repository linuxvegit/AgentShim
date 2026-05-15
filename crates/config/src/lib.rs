#![forbid(unsafe_code)]
pub mod loader;
pub mod plugins;
pub mod schema;
pub mod secrets;
pub mod upstream_accessors;
pub mod validation;

pub use loader::{load_from_path, ConfigError};
pub use plugins::{OnErrorYaml, PluginEntry, RoutePluginsBlock, TimeoutMs};
pub use schema::*;
pub use secrets::Secret;
pub use upstream_accessors::{upstream_cost, upstream_latency_budget, upstream_tier};
pub use validation::{
    validate, validate_for_reload, validate_plugins, validate_routes, ReloadBaseline, ReloadDiff,
    ReloadValidationError, ValidationError,
};
