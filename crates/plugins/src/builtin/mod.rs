//! Built-in plugin kinds. P06a ships `usage_recorder` behind a Cargo feature.

#[cfg(feature = "usage_recorder")]
pub mod usage_recorder;

use std::sync::Arc;

use crate::PluginFactory;

/// Returns all built-in plugin factories that are compiled in.
/// Called by `AppState::new` (gateway) to seed `PluginRegistry::build`.
pub fn builtin_plugins() -> Vec<Arc<dyn PluginFactory>> {
    let mut factories: Vec<Arc<dyn PluginFactory>> = Vec::new();
    #[cfg(feature = "usage_recorder")]
    factories.push(Arc::new(usage_recorder::UsageRecorderFactory));
    factories
}
