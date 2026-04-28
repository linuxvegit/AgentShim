use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};
use crate::secrets::Secret;

/// Top-level gateway configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub upstreams: BTreeMap<String, UpstreamConfig>,
    #[serde(default)]
    pub routes: Vec<RouteEntry>,
    pub copilot: Option<CopilotConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_keepalive")]
    pub keepalive_secs: u64,
}

fn default_bind() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8787
}

fn default_keepalive() -> u64 {
    15
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            port: default_port(),
            keepalive_secs: default_keepalive(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    #[default]
    Pretty,
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    #[serde(default)]
    pub format: LogFormat,
    #[serde(default = "default_filter")]
    pub filter: String,
}

fn default_filter() -> String {
    "info".to_string()
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            format: LogFormat::default(),
            filter: default_filter(),
        }
    }
}

/// Tagged upstream enum.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum UpstreamConfig {
    OpenAiCompatible(OpenAiCompatibleUpstream),
    GithubCopilot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiCompatibleUpstream {
    pub base_url: String,
    pub api_key: Secret,
    #[serde(default)]
    pub default_headers: BTreeMap<String, String>,
    #[serde(default = "default_timeout")]
    pub request_timeout_secs: u64,
}

fn default_timeout() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopilotConfig {
    pub credential_path: String,
}

/// A single route mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteEntry {
    pub frontend: String,
    pub model: String,
    pub upstream: String,
    pub upstream_model: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_defaults() {
        let cfg: ServerConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.port, 8787);
        assert_eq!(cfg.keepalive_secs, 15);
        assert_eq!(cfg.bind, "127.0.0.1");
    }

    #[test]
    fn unknown_fields_rejected() {
        let result: Result<ServerConfig, _> =
            serde_json::from_str(r#"{"port": 9000, "unknown_field": true}"#);
        assert!(result.is_err());
    }

    #[test]
    fn gateway_config_defaults() {
        let cfg: GatewayConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.server.port, 8787);
        assert!(cfg.upstreams.is_empty());
        assert!(cfg.routes.is_empty());
        assert!(cfg.copilot.is_none());
    }

    #[test]
    fn log_format_pretty_default() {
        let cfg: LoggingConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.format, LogFormat::Pretty);
    }
}
