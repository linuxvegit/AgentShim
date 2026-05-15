// Phase 4 T1: failing tests for the service-mode logging.file fallback
// helper. The module under test does not yet exist; this file is
// `#[cfg(windows)]`-gated so Linux/macOS builds simply skip it.

#![cfg(windows)]

use agent_shim_config::schema::{FileLoggingConfig, GatewayConfig, LogFormat, RotationPolicy};
use agent_shim_gateway::commands::service::log_fallback::{
    apply_service_log_fallback, default_service_log_path,
};

fn empty_config() -> GatewayConfig {
    use agent_shim_config::schema::{LoggingConfig, ServerConfig};
    use std::collections::BTreeMap;
    GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams: BTreeMap::new(),
        routes: vec![],
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
    }
}

#[test]
fn default_log_path_uses_programdata() {
    let path = default_service_log_path();
    let s = path.to_string_lossy();
    // PROGRAMDATA is set in any sane Windows env. Even on a CI runner.
    assert!(
        s.contains("agent-shim") && s.ends_with("agent-shim.log"),
        "expected agent-shim/logs/agent-shim.log under PROGRAMDATA; got {s:?}"
    );
}

#[test]
fn apply_fallback_injects_when_file_is_none() {
    let mut cfg = empty_config();
    assert!(cfg.logging.file.is_none());
    apply_service_log_fallback(&mut cfg);
    let file = cfg
        .logging
        .file
        .expect("fallback must populate logging.file");
    assert_eq!(file.format, LogFormat::Json);
    assert_eq!(file.rotation, RotationPolicy::Daily);
    assert_eq!(file.max_files, 7);
    assert!(file.path.is_absolute(), "fallback path must be absolute");
}

#[test]
fn apply_fallback_preserves_user_provided_file_config() {
    let mut cfg = empty_config();
    let user_path = std::path::PathBuf::from(r"C:\custom\logs\app.log");
    cfg.logging.file = Some(FileLoggingConfig {
        path: user_path.clone(),
        format: LogFormat::Pretty,
        rotation: RotationPolicy::Hourly,
        max_files: 24,
    });
    apply_service_log_fallback(&mut cfg);
    let file = cfg.logging.file.unwrap();
    assert_eq!(file.path, user_path);
    assert_eq!(file.format, LogFormat::Pretty);
    assert_eq!(file.rotation, RotationPolicy::Hourly);
    assert_eq!(file.max_files, 24);
}
