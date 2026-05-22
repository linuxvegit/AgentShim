//! Reload trigger plumbing. Plan 04 P04 T2.
//!
//! All reload triggers (SIGHUP, POST /admin/reload) feed into this
//! mpsc channel; one task on the receiving end performs the swap. This
//! ensures reload requests are serialized — no two swaps race for the
//! ArcSwap.
//!
//! T3 (handler) and T4 (applier + SIGHUP listener) construct
//! `ReloadRequest` and pattern-match `ReloadOutcome`; until then
//! everything in this module appears unused so we silence the warnings
//! at the module level to keep `-D warnings` clean.

#![allow(dead_code)]

use std::path::PathBuf;
use tokio::sync::oneshot;

/// One reload request. The handler awaits `respond_to` for the outcome
/// before returning HTTP 200/4xx/5xx.
pub struct ReloadRequest {
    pub source: ReloadSource,
    pub respond_to: oneshot::Sender<ReloadOutcome>,
}

#[derive(Debug, Clone)]
pub enum ReloadSource {
    /// Re-read config from disk at the given path.
    Path(PathBuf),
    /// Use the YAML body directly (no disk).
    Yaml(String),
    /// SIGHUP handler — re-read from the original `--config` path.
    Sighup,
}

#[derive(Debug)]
pub enum ReloadOutcome {
    Ok(agent_shim_config::ReloadDiff),
    ValidationError(String),
    ImmutableField(String),
    Io(String),
    Parse(String),
    /// Plan 07 P07: plugin section failed Layer-B validation (unknown
    /// kind / factory parse error / hook subscription mismatch). The
    /// entire reload is rejected; old plugin registry remains active.
    PluginValidation(String),
}
