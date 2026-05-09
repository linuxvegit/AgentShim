use crate::secrets::Secret;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    pub copilot: Option<CopilotConfig>,
    /// Optional admin listener. When `None`, no admin endpoints are
    /// exposed and no second listener is bound. Plan 01 P01.
    #[serde(default)]
    pub admin: Option<AdminConfig>,
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

/// Admin/operator HTTP listener — distinct from the request-serving listener
/// (`server.*`). Hosts /metrics, /healthz, /readyz, and /admin/reload.
///
/// **Absent → admin listener is disabled entirely.** No default values are
/// applied; operators opt in by including the `admin:` block in their
/// gateway YAML. This is the conservative default: a fresh v0.5 binary
/// upgraded from v0.4 has no exposed admin surface unless the operator
/// adds the block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    #[serde(default = "default_admin_bind")]
    pub bind: String,
    #[serde(default = "default_admin_port")]
    pub port: u16,
}

fn default_admin_bind() -> String {
    "127.0.0.1".to_string()
}

fn default_admin_port() -> u16 {
    9100
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            bind: default_admin_bind(),
            port: default_admin_port(),
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
    Anthropic(AnthropicUpstream),
    Deepseek(DeepseekUpstream),
    Gemini(GeminiUpstream),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicUpstream {
    #[serde(default = "default_anthropic_base_url")]
    pub base_url: String,
    pub api_key: Secret,
    #[serde(default = "default_anthropic_version")]
    pub anthropic_version: String,
    #[serde(default)]
    pub default_headers: BTreeMap<String, String>,
    #[serde(default = "default_timeout")]
    pub request_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeepseekUpstream {
    #[serde(default = "default_deepseek_base_url")]
    pub base_url: String,
    pub api_key: Secret,
    #[serde(default)]
    pub default_headers: BTreeMap<String, String>,
    #[serde(default = "default_timeout")]
    pub request_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeminiUpstream {
    #[serde(default = "default_gemini_base_url")]
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

fn default_anthropic_base_url() -> String {
    "https://api.anthropic.com".to_string()
}

fn default_anthropic_version() -> String {
    "2023-06-01".to_string()
}

fn default_deepseek_base_url() -> String {
    "https://api.deepseek.com/v1".to_string()
}

fn default_gemini_base_url() -> String {
    "https://generativelanguage.googleapis.com/v1beta".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopilotConfig {
    pub credential_path: String,
}

/// A single route mapping. Supports two shapes:
/// - **Singular** (v0.3 compat): `upstream` + `upstream_model`.
/// - **Array** (v0.4): `upstreams: [{name, model}, ...]` for fallback chains.
///
/// Both shapes are mutually exclusive on the same route; validation enforces it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteEntry {
    pub frontend: String,
    pub model: String,

    // ── Singular shape (v0.3) ──────────────────────────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_model: Option<String>,

    // ── Array shape (v0.4) ─────────────────────────────────────────────────
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<UpstreamRef>,

    /// Default reasoning effort applied when the inbound request doesn't set
    /// one. One of `minimal`, `low`, `medium`, `high`, `xhigh`. Forwarded to
    /// upstreams that understand `reasoning_effort` (Copilot/GPT-5/o-series,
    /// Anthropic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Default `anthropic-beta` header value to apply when the inbound request
    /// doesn't supply one. Common values:
    ///   `context-1m-2025-08-07` — enable Claude 1M context window
    ///   `prompt-caching-2024-07-31` — enable prompt caching
    /// Comma-separated lists are accepted (e.g. `context-1m-2025-08-07,fine-grained-tool-streaming-2025-05-14`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_beta: Option<String>,

    // ── Per-route resilience policy (v0.4) ─────────────────────────────────
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub breaker: BreakerConfig,
}

impl RouteEntry {
    /// Convenience constructor for a v0.3-shape singular-upstream route.
    /// Builds a `RouteEntry` with the singular `upstream`/`upstream_model`
    /// fields set, an empty `upstreams` array, default `retry`/`breaker`
    /// configs, and no `reasoning_effort`/`anthropic_beta`. Primarily for
    /// hand-built test fixtures; production configs are deserialized.
    pub fn singular(
        frontend: impl Into<String>,
        model: impl Into<String>,
        upstream: impl Into<String>,
        upstream_model: impl Into<String>,
    ) -> Self {
        Self {
            frontend: frontend.into(),
            model: model.into(),
            upstream: Some(upstream.into()),
            upstream_model: Some(upstream_model.into()),
            upstreams: Vec::new(),
            reasoning_effort: None,
            anthropic_beta: None,
            retry: RetryConfig::default(),
            breaker: BreakerConfig::default(),
        }
    }
}

/// Reference to one upstream in a fallback chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamRef {
    pub name: String,
    pub model: String,
}

/// Per-route retry policy. All fields have defaults from §4.5 of the
/// Phase 4 design (D12).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_initial_backoff_ms")]
    pub initial_backoff_ms: u64,
    #[serde(default = "default_multiplier")]
    pub multiplier: f64,
    #[serde(default = "default_jitter_pct")]
    pub jitter_pct: f64,
    #[serde(default = "default_total_budget_ms")]
    pub total_budget_ms: u64,
    #[serde(default = "default_retry_on")]
    pub retry_on: Vec<String>,
}

// Required so `#[serde(default)]` on the `retry: RetryConfig` field in
// `RouteEntry` works when the whole `retry:` block is omitted. The per-field
// `default = "..."` attrs only fire when a field key is missing while parsing
// THIS struct's own deserialization — they do not run when the entire struct
// is absent from the parent.
impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            initial_backoff_ms: default_initial_backoff_ms(),
            multiplier: default_multiplier(),
            jitter_pct: default_jitter_pct(),
            total_budget_ms: default_total_budget_ms(),
            retry_on: default_retry_on(),
        }
    }
}

// §4.5 D12 — Phase 4 retry/breaker defaults. Source of truth:
// docs/superpowers/specs/2026-05-08-phase-4-resiliency-design.md §4.5.
// Changing any value here is a contract change, not a tunable: it ships in
// v0.4.0 release notes and may break operators relying on the documented
// defaults. Loosen tunables via the `retry:`/`breaker:` blocks in YAML, not
// by editing these constants.
fn default_max_attempts() -> u32 {
    2
}
fn default_initial_backoff_ms() -> u64 {
    100
}
fn default_multiplier() -> f64 {
    2.0
}
fn default_jitter_pct() -> f64 {
    25.0
}
fn default_total_budget_ms() -> u64 {
    5000
}
fn default_retry_on() -> Vec<String> {
    vec![
        "network".into(),
        "upstream_5xx".into(),
        "upstream_429".into(),
    ]
}

/// Per-route circuit-breaker policy. Plan 03 wires the actual state machine;
/// Plan 01 just lands the schema fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BreakerConfig {
    #[serde(default = "default_breaker_enabled")]
    pub enabled: bool,
    #[serde(default = "default_failure_threshold_pct")]
    pub failure_threshold_pct: u32,
    #[serde(default = "default_min_requests")]
    pub min_requests: u32,
    #[serde(default = "default_window_secs")]
    pub window_secs: u64,
    #[serde(default = "default_open_cooldown_secs")]
    pub open_cooldown_secs: u64,
}

// Required so `#[serde(default)]` on the `breaker: BreakerConfig` field in
// `RouteEntry` works when the whole `breaker:` block is omitted. See the note
// above `impl Default for RetryConfig` for the full rationale.
impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            enabled: default_breaker_enabled(),
            failure_threshold_pct: default_failure_threshold_pct(),
            min_requests: default_min_requests(),
            window_secs: default_window_secs(),
            open_cooldown_secs: default_open_cooldown_secs(),
        }
    }
}

fn default_breaker_enabled() -> bool {
    true
}
fn default_failure_threshold_pct() -> u32 {
    50
}
fn default_min_requests() -> u32 {
    20
}
fn default_window_secs() -> u64 {
    60
}
fn default_open_cooldown_secs() -> u64 {
    30
}

/// Top-level auth config. Default: disabled (preserves v0.3 behavior).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    pub enabled: bool,
    pub required: bool,
    pub keys: BTreeMap<String, AuthKeyEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthKeyEntry {
    pub label: String,
}

/// Top-level rate-limit config. Default: disabled.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RateLimitConfig {
    pub enabled: bool,
    pub per_key: PerKeyConfig,
    pub per_route: BTreeMap<String, BucketConfigYaml>,
    pub per_upstream: BTreeMap<String, BucketConfigYaml>,
    pub per_ip: PerIpConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PerKeyConfig {
    pub default: Option<BucketConfigYaml>,
    pub anonymous: Option<BucketConfigYaml>,
    pub overrides: BTreeMap<String, BucketConfigYaml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BucketConfigYaml {
    pub rate_per_sec: u32,
    pub burst: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerIpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_per_ip_rate")]
    pub rate_per_sec: u32,
    #[serde(default = "default_per_ip_burst")]
    pub burst: u32,
}

impl Default for PerIpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rate_per_sec: default_per_ip_rate(),
            burst: default_per_ip_burst(),
        }
    }
}

fn default_per_ip_rate() -> u32 {
    5
}

fn default_per_ip_burst() -> u32 {
    20
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

    #[test]
    fn anthropic_upstream_yaml_round_trip_with_defaults() {
        // Minimal Anthropic upstream config; only api_key required.
        let yaml = "type: anthropic\napi_key: sk-ant-test\n";
        let cfg: UpstreamConfig = serde_yaml::from_str(yaml).unwrap();
        let UpstreamConfig::Anthropic(a) = cfg else {
            panic!("expected Anthropic variant");
        };
        assert_eq!(a.api_key.expose(), "sk-ant-test");
        assert_eq!(a.base_url, "https://api.anthropic.com");
        assert_eq!(a.anthropic_version, "2023-06-01");
        assert_eq!(a.request_timeout_secs, 30);
        assert!(a.default_headers.is_empty());
    }

    #[test]
    fn anthropic_upstream_yaml_with_overrides() {
        let yaml = "
type: anthropic
api_key: sk-ant-test
base_url: https://custom.example.com
anthropic_version: 2024-01-01
request_timeout_secs: 60
default_headers:
  x-custom: value
";
        let cfg: UpstreamConfig = serde_yaml::from_str(yaml).unwrap();
        let UpstreamConfig::Anthropic(a) = cfg else {
            panic!("expected Anthropic variant");
        };
        assert_eq!(a.base_url, "https://custom.example.com");
        assert_eq!(a.anthropic_version, "2024-01-01");
        assert_eq!(a.request_timeout_secs, 60);
        assert_eq!(
            a.default_headers.get("x-custom"),
            Some(&"value".to_string())
        );
    }

    #[test]
    fn anthropic_upstream_unknown_field_rejected() {
        let yaml = "type: anthropic\napi_key: sk-ant-test\nbogus: 1\n";
        let result: Result<UpstreamConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "deny_unknown_fields should reject 'bogus'");
    }

    #[test]
    fn deepseek_upstream_yaml_round_trip_with_defaults() {
        // Minimal DeepSeek upstream config; only api_key required.
        let yaml = "type: deepseek\napi_key: sk-deepseek-test\n";
        let cfg: UpstreamConfig = serde_yaml::from_str(yaml).unwrap();
        let UpstreamConfig::Deepseek(d) = cfg else {
            panic!("expected Deepseek variant");
        };
        assert_eq!(d.api_key.expose(), "sk-deepseek-test");
        assert_eq!(d.base_url, "https://api.deepseek.com/v1");
        assert_eq!(d.request_timeout_secs, 30);
        assert!(d.default_headers.is_empty());
    }

    #[test]
    fn deepseek_upstream_yaml_with_overrides() {
        let yaml = "
type: deepseek
api_key: sk-deepseek-test
base_url: https://custom.deepseek.example.com/v1
request_timeout_secs: 60
default_headers:
  x-custom: value
";
        let cfg: UpstreamConfig = serde_yaml::from_str(yaml).unwrap();
        let UpstreamConfig::Deepseek(d) = cfg else {
            panic!("expected Deepseek variant");
        };
        assert_eq!(d.api_key.expose(), "sk-deepseek-test");
        assert_eq!(d.base_url, "https://custom.deepseek.example.com/v1");
        assert_eq!(d.request_timeout_secs, 60);
        assert_eq!(
            d.default_headers.get("x-custom"),
            Some(&"value".to_string())
        );
    }

    #[test]
    fn deepseek_upstream_unknown_field_rejected() {
        let yaml = "type: deepseek\napi_key: sk-deepseek-test\nbogus: 1\n";
        let result: Result<UpstreamConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "deny_unknown_fields should reject 'bogus'");
    }

    #[test]
    fn gemini_upstream_yaml_round_trip_with_defaults() {
        // Minimal Gemini upstream config; only api_key required.
        let yaml = "type: gemini\napi_key: ai-studio-test\n";
        let cfg: UpstreamConfig = serde_yaml::from_str(yaml).unwrap();
        let UpstreamConfig::Gemini(g) = cfg else {
            panic!("expected Gemini variant");
        };
        assert_eq!(g.api_key.expose(), "ai-studio-test");
        assert_eq!(
            g.base_url,
            "https://generativelanguage.googleapis.com/v1beta"
        );
        assert_eq!(g.request_timeout_secs, 30);
        assert!(g.default_headers.is_empty());
    }

    #[test]
    fn gemini_upstream_yaml_with_overrides() {
        let yaml = "
type: gemini
api_key: ai-studio-test
base_url: https://custom.gemini.example.com/v1beta
request_timeout_secs: 60
default_headers:
  x-custom: value
";
        let cfg: UpstreamConfig = serde_yaml::from_str(yaml).unwrap();
        let UpstreamConfig::Gemini(g) = cfg else {
            panic!("expected Gemini variant");
        };
        assert_eq!(g.api_key.expose(), "ai-studio-test");
        assert_eq!(g.base_url, "https://custom.gemini.example.com/v1beta");
        assert_eq!(g.request_timeout_secs, 60);
        assert_eq!(
            g.default_headers.get("x-custom"),
            Some(&"value".to_string())
        );
    }

    #[test]
    fn gemini_upstream_unknown_field_rejected() {
        let yaml = "type: gemini\napi_key: ai-studio-test\nbogus: 1\n";
        let result: Result<UpstreamConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "deny_unknown_fields should reject 'bogus'");
    }

    #[test]
    fn route_entry_array_form_deserializes() {
        let yaml = r#"
            frontend: openai_chat
            model: gpt-4o
            upstreams:
              - {name: openai, model: gpt-4o-2024-11-20}
              - {name: copilot, model: gpt-4o}
            retry:
              max_attempts: 3
              total_budget_ms: 8000
        "#;
        let entry: RouteEntry = serde_yaml::from_str(yaml).expect("parses");
        assert_eq!(entry.upstreams.len(), 2);
        assert_eq!(entry.upstreams[0].name, "openai");
        assert_eq!(entry.retry.max_attempts, 3);
        assert_eq!(entry.upstream, None);
    }

    #[test]
    fn route_entry_singular_form_still_works() {
        let yaml = r#"
            frontend: openai_chat
            model: gpt-4o
            upstream: openai
            upstream_model: gpt-4o-2024-11-20
        "#;
        let entry: RouteEntry = serde_yaml::from_str(yaml).expect("parses");
        assert_eq!(entry.upstream.as_deref(), Some("openai"));
        assert_eq!(entry.upstreams.len(), 0); // empty when singular
        assert_eq!(entry.retry.max_attempts, 2); // default per D12
    }

    #[test]
    fn route_entry_singular_constructor() {
        let entry = RouteEntry::singular("openai_chat", "gpt-4o", "openai", "gpt-4o-2024-11-20");
        assert_eq!(entry.frontend, "openai_chat");
        assert_eq!(entry.model, "gpt-4o");
        assert_eq!(entry.upstream.as_deref(), Some("openai"));
        assert_eq!(entry.upstream_model.as_deref(), Some("gpt-4o-2024-11-20"));
        assert!(entry.upstreams.is_empty());
        assert_eq!(entry.retry.max_attempts, 2); // §4.5 default
    }

    #[test]
    fn v03_example_yaml_still_parses() {
        // Back-compat regression for Plan 04 P01 T1: the v0.3 example file
        // uses singular `upstream`/`upstream_model` and no retry/breaker
        // blocks. It must continue to deserialize cleanly under the new
        // schema (defaults fill in retry/breaker per §4.5).
        let yaml = include_str!("../../../config/gateway.example.yaml");
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("v0.3 example parses");
        assert!(!cfg.routes.is_empty());
        for route in &cfg.routes {
            // Singular shape preserved.
            assert!(route.upstream.is_some(), "expected singular shape");
            assert!(route.upstreams.is_empty());
            // Defaults applied.
            assert_eq!(route.retry.max_attempts, 2);
            assert!(route.breaker.enabled);
        }
    }

    #[test]
    fn v03_minimal_yaml_still_parses() {
        // Back-compat regression for the `config/gateway.minimal.yaml`
        // template — must continue to deserialize under the new schema.
        let yaml = include_str!("../../../config/gateway.minimal.yaml");
        let _cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("v0.3 minimal parses");
    }

    #[test]
    fn admin_config_block_parses() {
        let yaml = r#"
admin:
  bind: 127.0.0.1
  port: 9100
"#;
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("parses");
        let admin = cfg.admin.expect("admin block present");
        assert_eq!(admin.bind, "127.0.0.1");
        assert_eq!(admin.port, 9100);
    }

    #[test]
    fn admin_config_absent_means_disabled() {
        let yaml = "server: {bind: 127.0.0.1, port: 8787}";
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("parses");
        assert!(cfg.admin.is_none(), "admin must be None when block absent");
    }

    #[test]
    fn admin_config_empty_block_uses_defaults() {
        let yaml = r#"
admin: {}
"#;
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("parses");
        let admin = cfg.admin.expect("admin block present");
        assert_eq!(admin.bind, "127.0.0.1");
        assert_eq!(admin.port, 9100);
    }

    #[test]
    fn admin_config_partial_block_uses_field_defaults() {
        let yaml = r#"
admin:
  port: 9200
"#;
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("parses");
        let admin = cfg.admin.expect("admin block present");
        assert_eq!(
            admin.bind, "127.0.0.1",
            "bind should fall back to default when omitted"
        );
        assert_eq!(admin.port, 9200);
    }
}
