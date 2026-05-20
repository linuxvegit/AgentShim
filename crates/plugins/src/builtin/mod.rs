//! Built-in plugin kinds. P06a ships `usage_recorder` behind a Cargo feature.

#[cfg(feature = "usage_recorder")]
pub mod usage_recorder;

use crate::PluginFactory;

/// Returns all built-in plugin factories that are compiled in.
/// Called by `AppState::new` (gateway) to seed `PluginRegistry::build`.
#[allow(clippy::vec_init_then_push)] // P06b will add more push() calls behind #[cfg(...)]
pub fn builtin_plugins() -> Vec<Box<dyn PluginFactory>> {
    #[allow(unused_mut)]
    let mut factories: Vec<Box<dyn PluginFactory>> = Vec::new();
    #[cfg(feature = "usage_recorder")]
    factories.push(Box::new(usage_recorder::UsageRecorderFactory));
    factories
}
