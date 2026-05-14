//! Service-mode logging fallback. The SCM-spawned process has no console,
//! so without a file sink the operator has nowhere to read logs. We inject
//! a sane default — `%PROGRAMDATA%\agent-shim\logs\agent-shim.log`,
//! daily rotation, 7-file retention, JSON format — only when the loaded
//! config did not specify `logging.file`. User-provided settings always win.
//!
//! Spec section 5.4 (windows-service-and-file-logging-design).

use agent_shim_config::schema::{FileLoggingConfig, GatewayConfig, LogFormat, RotationPolicy};
use std::path::PathBuf;

/// Resolve `%PROGRAMDATA%\agent-shim\logs\agent-shim.log`, falling back
/// to `C:\ProgramData\` if the env var is somehow unset (it's defined
/// on every supported Windows version, but we don't panic on the off chance).
#[allow(dead_code)] // Wired into `service::run` in Phase 4 T3.
pub fn default_service_log_path() -> PathBuf {
    let base = std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("agent-shim").join("logs").join("agent-shim.log")
}

/// Mutate the in-memory `GatewayConfig` to add a default file logging
/// configuration if and only if the user did not specify one.
///
/// Must be called BEFORE `commands::serve::run_core`, since tracing
/// initialization inside `run_core` reads `cfg.logging.file`.
#[allow(dead_code)] // Wired into `service::run` in Phase 4 T3.
pub fn apply_service_log_fallback(cfg: &mut GatewayConfig) {
    if cfg.logging.file.is_some() {
        return;
    }
    cfg.logging.file = Some(FileLoggingConfig {
        path: default_service_log_path(),
        format: LogFormat::Json,
        rotation: RotationPolicy::Daily,
        max_files: 7,
    });
}
