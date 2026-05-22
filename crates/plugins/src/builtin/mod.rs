//! Built-in plugin kinds. Each kind is feature-gated.
//!
//! Wire-up: `builtin_plugins()` returns the compiled-in built-in
//! factories. Gateway calls this once during `AppCore::build` before
//! invoking `PluginRegistry::build`.

#[cfg(feature = "pii_scrubber")]
pub mod pii_scrubber;
#[cfg(feature = "prompt_compressor")]
pub mod prompt_compressor;
#[cfg(feature = "usage_recorder")]
pub mod usage_recorder;

use crate::PluginFactory;

/// Returns all built-in plugin factories that are compiled in.
/// Called by `AppState::new` (gateway) to seed `PluginRegistry::build`.
///
/// Factories are pushed in alphabetical order by `kind_name()` so the
/// `RegistryBuildError::UnknownKind` diagnostic list stays deterministic.
#[allow(clippy::vec_init_then_push)] // P06c may add user-supplied factories
pub fn builtin_plugins() -> Vec<Box<dyn PluginFactory>> {
    #[allow(unused_mut)]
    let mut factories: Vec<Box<dyn PluginFactory>> = Vec::new();
    #[cfg(feature = "pii_scrubber")]
    factories.push(Box::new(pii_scrubber::PiiScrubberFactory));
    #[cfg(feature = "prompt_compressor")]
    factories.push(Box::new(prompt_compressor::PromptCompressorFactory));
    #[cfg(feature = "usage_recorder")]
    factories.push(Box::new(usage_recorder::UsageRecorderFactory));
    factories
}
