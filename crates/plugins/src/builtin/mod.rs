//! Built-in plugin kinds. P02 ships only this module declaration;
//! P06 adds `prompt_compressor`, `pii_scrubber`, and `usage_recorder`
//! behind individual Cargo features.

// Placeholder. P06 adds:
//   #[cfg(feature = "plugin-prompt-compressor")]
//   pub mod prompt_compressor;
// etc., and a `register_builtin_plugins(factories: &mut Vec<...>)` fn.
