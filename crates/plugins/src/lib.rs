//! In-process Rust plugin system for AgentShim. See the design spec at
//! `docs/superpowers/specs/2026-05-14-plugin-system-design.md` and the
//! decision record in `docs/adr/0008-plugin-system.md`.
//!
//! # Stability
//!
//! The public trait surface (`Plugin`, `PluginFactory`, `PluginContext`,
//! `HookSet`, `PluginError`) is **not** semver-stable until AgentShim
//! reaches v1.0. Treat it as you would any other AgentShim public type
//! pre-1.0: contributions welcome, but breaking changes can happen in
//! minor releases.

#![forbid(unsafe_code)]

mod context;
mod error;
mod invoke;
mod registry;
mod trait_def;

pub mod builtin;

pub use context::{PluginContext, ResponseSummary, UpstreamStatus};
pub use error::{OnError, PluginConfigError, PluginError, PluginResult};
pub use registry::{HookTimeouts, PluginEntry, PluginRegistry, RegistryBuildError};
pub use trait_def::{Hook, HookSet, Plugin, PluginFactory};
