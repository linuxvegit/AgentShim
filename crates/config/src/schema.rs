use crate::secrets::Secret;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

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
    /// Top-level plugin declarations. Each entry is a named plugin
    /// instance keyed by `<plugin_name>`. Routes reference these by
    /// name. Plan 07 P03.
    #[serde(default)]
    pub plugins: BTreeMap<String, crate::plugins::PluginEntry>,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    pub copilot: Option<CopilotConfig>,
    /// Optional admin listener. When `None`, no admin endpoints are
    /// exposed and no second listener is bound. Plan 01 P01.
    #[serde(default)]
    pub admin: Option<AdminConfig>,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub otel: Option<OtelConfig>,
    /// Shutdown lifecycle config. Phase 7 P05 §8.
    #[serde(default)]
    pub shutdown: ShutdownConfig,
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

/// Prometheus metrics configuration. Plan 02 P02.
///
/// Currently only exposes per-metric histogram bucket overrides. The
/// metrics surface itself is wired in T2 (recorder install + /metrics
/// handler); this struct just lands the schema field so callsites can
/// reference `config.metrics.histogram_buckets` from T2 onwards.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    /// Per-metric-name histogram bucket overrides. Keys are metric names
    /// (e.g. `"agent_shim_request_duration_seconds"` or the bare suffix
    /// `"request_duration_seconds"` — both forms accepted; the bare form
    /// gets the `agent_shim_` prefix prepended at install time).
    /// Absent → exporter defaults (Prometheus's standard duration buckets).
    #[serde(default)]
    pub histogram_buckets: BTreeMap<String, Vec<f64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OtelConfig {
    /// OTLP/gRPC collector endpoint. `None` → spans created locally,
    /// not exported. Spec D4.
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default = "default_service_name")]
    pub service_name: String,
    #[serde(default)]
    pub service_version: Option<String>,
    #[serde(default = "default_sample_ratio")]
    pub sample_ratio: f64,
    /// Operator-supplied resource attributes (e.g. deployment.environment,
    /// cloud.region). Merged into the OTel `Resource` on init.
    #[serde(default)]
    pub resource_attrs: BTreeMap<String, String>,
}

fn default_service_name() -> String {
    "agent-shim".to_string()
}

fn default_sample_ratio() -> f64 {
    1.0
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            service_name: default_service_name(),
            service_version: None,
            sample_ratio: default_sample_ratio(),
            resource_attrs: BTreeMap::new(),
        }
    }
}

/// Shutdown lifecycle timing knobs. Phase 7 P05 §8 / Q8.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ShutdownConfig {
    /// Seconds to wait for H7 (`on_response_complete`) plugin tasks to
    /// complete during gateway shutdown before dropping them. Default 5.
    /// Layer A validation rejects values > 300.
    #[serde(default = "default_plugin_flush_secs")]
    pub plugin_flush_secs: u64,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            plugin_flush_secs: default_plugin_flush_secs(),
        }
    }
}

fn default_plugin_flush_secs() -> u64 {
    5
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
    /// Optional file logging configuration. When set, log events are
    /// written to a rolling file in addition to stdout. See
    /// `FileLoggingConfig` for fields. Plan windows-service P02 T3.
    #[serde(default)]
    pub file: Option<FileLoggingConfig>,
}

fn default_filter() -> String {
    "info".to_string()
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            format: LogFormat::default(),
            filter: default_filter(),
            file: None,
        }
    }
}

/// File-logging configuration. When present on `LoggingConfig::file`,
/// a `tracing-appender` rolling file writer is installed in addition
/// to the stdout layer. Async writes via `tracing_appender::non_blocking`;
/// the `WorkerGuard` lives on `TracingHandles` and is dropped at process
/// exit to flush. SIGKILL bypasses the flush — documented in
/// `docs/observability.md`. Plan windows-service P02 T3.
///
/// All fields except `path` have defaults; `path` is required.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileLoggingConfig {
    /// Absolute log file path. The parent directory is created at
    /// startup if it does not exist; permission failure is fatal.
    pub path: PathBuf,
    /// File log format. Independent of `LoggingConfig::format`
    /// (which controls stdout). Defaults to `json` because files
    /// are usually consumed by tooling, not humans.
    #[serde(default = "default_file_format")]
    pub format: LogFormat,
    /// Rotation cadence: `daily`, `hourly`, or `never`.
    #[serde(default)]
    pub rotation: RotationPolicy,
    /// Maximum number of log files retained (current file + history).
    /// `0` means unlimited; otherwise the oldest rolled file is deleted
    /// after rotation. Default: 7 — one week of daily logs.
    #[serde(default = "default_max_files")]
    pub max_files: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RotationPolicy {
    #[default]
    Daily,
    Hourly,
    Never,
}

fn default_file_format() -> LogFormat {
    LogFormat::Json
}

fn default_max_files() -> usize {
    7
}

/// Service tier label on an upstream. Used by Phase 6 cost-aware
/// routing: routes can specify `min_tier`, and upstreams below that
/// tier are filtered out of the chain before the resilience layer runs.
///
/// Derived ordering is the operator-intuitive one:
///   economy < standard < premium
///
/// Plan 06 P03 T1.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Tier {
    Economy,
    Standard,
    Premium,
}

/// Per-upstream USD cost per million tokens. Used by Phase 6
/// cost-aware routing to estimate request cost and reject upstreams
/// whose estimate exceeds a route's `max_cost_usd`.
///
/// Both fields must be present and non-negative; partial declarations
/// fail validation (rule 15). Plan 06 P03 T1.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UpstreamCost {
    pub input_per_million_usd: f64,
    pub output_per_million_usd: f64,
}

/// Tagged upstream enum.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum UpstreamConfig {
    OpenAiCompatible(OpenAiCompatibleUpstream),
    GithubCopilot(GithubCopilotUpstream),
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
    /// Tier label. Plan 06 P03 — required.
    pub tier: Tier,
    /// Per-token cost in USD. Plan 06 P03 — optional.
    /// If present, used by cost-aware routing to estimate per-request
    /// cost and apply `route.max_cost_usd` caps.
    #[serde(default)]
    pub cost: Option<UpstreamCost>,
    /// p95 latency budget in milliseconds. Plan 06 P03 — optional.
    /// If present, the cost filter compares against the recent p95
    /// from the metrics histogram. Upstreams over budget are skipped.
    #[serde(default)]
    pub p95_latency_budget_ms: Option<u64>,
}

/// GitHub Copilot upstream. Subscription product — no per-token cost,
/// so `cost` is typically `None`. `tier` is still required for schema
/// uniformity (Plan 06 P03).
///
/// Note: there is intentionally no `Default` impl. The `tier` field is
/// required per spec §5.4; defaulting it would silently violate the
/// breaking-change contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubCopilotUpstream {
    /// Tier label. Plan 06 P03 — required.
    pub tier: Tier,
    /// Per-token cost in USD. Plan 06 P03 — optional.
    /// If present, used by cost-aware routing to estimate per-request
    /// cost and apply `route.max_cost_usd` caps.
    #[serde(default)]
    pub cost: Option<UpstreamCost>,
    /// p95 latency budget in milliseconds. Plan 06 P03 — optional.
    /// If present, the cost filter compares against the recent p95
    /// from the metrics histogram. Upstreams over budget are skipped.
    #[serde(default)]
    pub p95_latency_budget_ms: Option<u64>,
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
    /// Tier label. Plan 06 P03 — required.
    pub tier: Tier,
    /// Per-token cost in USD. Plan 06 P03 — optional.
    /// If present, used by cost-aware routing to estimate per-request
    /// cost and apply `route.max_cost_usd` caps.
    #[serde(default)]
    pub cost: Option<UpstreamCost>,
    /// p95 latency budget in milliseconds. Plan 06 P03 — optional.
    /// If present, the cost filter compares against the recent p95
    /// from the metrics histogram. Upstreams over budget are skipped.
    #[serde(default)]
    pub p95_latency_budget_ms: Option<u64>,
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
    /// Tier label. Plan 06 P03 — required.
    pub tier: Tier,
    /// Per-token cost in USD. Plan 06 P03 — optional.
    /// If present, used by cost-aware routing to estimate per-request
    /// cost and apply `route.max_cost_usd` caps.
    #[serde(default)]
    pub cost: Option<UpstreamCost>,
    /// p95 latency budget in milliseconds. Plan 06 P03 — optional.
    /// If present, the cost filter compares against the recent p95
    /// from the metrics histogram. Upstreams over budget are skipped.
    #[serde(default)]
    pub p95_latency_budget_ms: Option<u64>,
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
    /// Tier label. Plan 06 P03 — required.
    pub tier: Tier,
    /// Per-token cost in USD. Plan 06 P03 — optional.
    /// If present, used by cost-aware routing to estimate per-request
    /// cost and apply `route.max_cost_usd` caps.
    #[serde(default)]
    pub cost: Option<UpstreamCost>,
    /// p95 latency budget in milliseconds. Plan 06 P03 — optional.
    /// If present, the cost filter compares against the recent p95
    /// from the metrics histogram. Upstreams over budget are skipped.
    #[serde(default)]
    pub p95_latency_budget_ms: Option<u64>,
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

    // ── Per-route cost-aware routing knobs (Phase 6 P03) ───────────────────
    /// Minimum tier required for an upstream in this route's chain to
    /// be considered. Upstreams with `tier < min_tier` are filtered
    /// out before the resilience layer runs. Plan 06 P03 — optional.
    /// When None, no tier filtering applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_tier: Option<Tier>,
    /// Per-request cost cap in USD. The cost filter estimates the cost
    /// (using `tiktoken-rs` for input tokens + `request.max_tokens` for
    /// output) and rejects upstreams whose estimate exceeds this cap.
    /// Plan 06 P03 — optional. When None, no cap applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,

    // ── Per-route plugin attachments (Phase 7 P03) ─────────────────────────
    /// Per-hook ordered lists of plugin references. `None` (or all-empty
    /// lists) means no plugins run on this route — the registry's fast
    /// path is enabled. Plan 07 P03.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<crate::plugins::RoutePluginsBlock>,

    // ── Per-route canonical effort rewrite table (2026-05-28) ──────────────
    /// Optional ordered list of `{ match, set }` rules over canonical
    /// reasoning effort. First matching `match` fires its `set`; unmatched
    /// passes through. Both fields are strings drawn from the canonical
    /// effort vocabulary `{minimal, low, medium, high, xhigh, max}`.
    /// Validation rejects unknown values at config-load time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_mapping: Vec<MappingRuleConfig>,
}

/// One row in a route's `reasoning_mapping` table. The strings are validated
/// against the canonical effort vocabulary by `validation::validate_routes`
/// before the router translates them into `agent_shim_core::policy::MappingRule`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingRuleConfig {
    pub r#match: String,
    pub set: String,
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
            min_tier: None,
            max_cost_usd: None,
            plugins: None,
            reasoning_mapping: Vec::new(),
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
        let yaml = "type: anthropic\napi_key: sk-ant-test\ntier: standard\n";
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
tier: standard
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
        let yaml = "type: anthropic\napi_key: sk-ant-test\ntier: standard\nbogus: 1\n";
        let result: Result<UpstreamConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "deny_unknown_fields should reject 'bogus'");
    }

    #[test]
    fn deepseek_upstream_yaml_round_trip_with_defaults() {
        // Minimal DeepSeek upstream config; only api_key required.
        let yaml = "type: deepseek\napi_key: sk-deepseek-test\ntier: standard\n";
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
tier: standard
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
        let yaml = "type: deepseek\napi_key: sk-deepseek-test\ntier: standard\nbogus: 1\n";
        let result: Result<UpstreamConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "deny_unknown_fields should reject 'bogus'");
    }

    #[test]
    fn gemini_upstream_yaml_round_trip_with_defaults() {
        // Minimal Gemini upstream config; only api_key required.
        let yaml = "type: gemini\napi_key: ai-studio-test\ntier: standard\n";
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
tier: standard
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
        let yaml = "type: gemini\napi_key: ai-studio-test\ntier: standard\nbogus: 1\n";
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
        // Mapping table defaults to empty so existing routes are unaffected.
        assert!(entry.reasoning_mapping.is_empty());
    }

    #[test]
    fn route_entry_parses_reasoning_mapping() {
        let yaml = r#"
            frontend: anthropic_messages
            model: claude-opus-4-7
            upstream: copilot
            upstream_model: claude-opus-4-7
            reasoning_mapping:
              - match: max
                set: xhigh
              - match: high
                set: xhigh
        "#;
        let entry: RouteEntry = serde_yaml::from_str(yaml).expect("parses");
        assert_eq!(entry.reasoning_mapping.len(), 2);
        assert_eq!(entry.reasoning_mapping[0].r#match, "max");
        assert_eq!(entry.reasoning_mapping[0].set, "xhigh");
        assert_eq!(entry.reasoning_mapping[1].r#match, "high");
        assert_eq!(entry.reasoning_mapping[1].set, "xhigh");
    }

    #[test]
    fn reasoning_mapping_rejects_unknown_set_field() {
        let yaml = r#"
            frontend: anthropic_messages
            model: x
            upstream: u
            upstream_model: um
            reasoning_mapping:
              - match: high
                set: high
                unknown: oops
        "#;
        let result: Result<RouteEntry, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "deny_unknown_fields must reject");
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

    #[test]
    fn metrics_config_defaults() {
        let yaml = "server: {bind: 127.0.0.1, port: 8787}";
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("parses");
        assert!(cfg.metrics.histogram_buckets.is_empty());
    }

    #[test]
    fn metrics_config_custom_buckets() {
        let yaml = r#"
metrics:
  histogram_buckets:
    request_duration_seconds: [0.1, 0.5, 1.0, 5.0]
"#;
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("parses");
        let buckets = cfg
            .metrics
            .histogram_buckets
            .get("request_duration_seconds")
            .unwrap();
        assert_eq!(buckets, &vec![0.1, 0.5, 1.0, 5.0]);
    }

    #[test]
    fn otel_config_absent_means_disabled() {
        let yaml = "server: {bind: 127.0.0.1, port: 8787}";
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("parses");
        assert!(cfg.otel.is_none());
    }

    #[test]
    fn otel_config_with_endpoint() {
        let yaml = r#"
otel:
  endpoint: http://otel-collector:4317
  service_name: gw
  sample_ratio: 0.5
"#;
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("parses");
        let otel = cfg.otel.expect("present");
        assert_eq!(otel.endpoint.as_deref(), Some("http://otel-collector:4317"));
        assert_eq!(otel.service_name, "gw");
        assert_eq!(otel.sample_ratio, 0.5);
        assert!(otel.resource_attrs.is_empty());
    }

    #[test]
    fn otel_config_defaults() {
        let yaml = r#"
otel: {}
"#;
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("parses");
        let otel = cfg.otel.expect("present");
        assert!(otel.endpoint.is_none(), "default endpoint should be None");
        assert_eq!(otel.service_name, "agent-shim");
        assert_eq!(otel.sample_ratio, 1.0);
    }

    // Plan windows-service P02 T2: failing TDD-red tests for
    // FileLoggingConfig and RotationPolicy.
    #[test]
    fn file_logging_config_roundtrips_defaults() {
        // Default for FileLoggingConfig is "no file logging" — the parent
        // LoggingConfig.file field is Option<FileLoggingConfig>, default None.
        let yaml = r#"
format: json
filter: info
file:
  path: /var/log/agent-shim/agent-shim.log
"#;
        let cfg: LoggingConfig = serde_yaml::from_str(yaml).unwrap();
        let file = cfg.file.expect("file: section should parse");
        assert_eq!(
            file.path,
            std::path::PathBuf::from("/var/log/agent-shim/agent-shim.log")
        );
        assert_eq!(file.format, LogFormat::Json);
        assert_eq!(file.rotation, RotationPolicy::Daily);
        assert_eq!(file.max_files, 7);
    }

    #[test]
    fn file_logging_config_explicit_all_fields() {
        let yaml = r#"
format: pretty
filter: debug
file:
  path: /tmp/agent.log
  format: pretty
  rotation: hourly
  max_files: 24
"#;
        let cfg: LoggingConfig = serde_yaml::from_str(yaml).unwrap();
        let file = cfg.file.unwrap();
        assert_eq!(file.format, LogFormat::Pretty);
        assert_eq!(file.rotation, RotationPolicy::Hourly);
        assert_eq!(file.max_files, 24);
    }

    #[test]
    fn rotation_policy_serde_snake_case() {
        assert_eq!(
            serde_yaml::from_str::<RotationPolicy>("daily").unwrap(),
            RotationPolicy::Daily,
        );
        assert_eq!(
            serde_yaml::from_str::<RotationPolicy>("hourly").unwrap(),
            RotationPolicy::Hourly,
        );
        assert_eq!(
            serde_yaml::from_str::<RotationPolicy>("never").unwrap(),
            RotationPolicy::Never,
        );
    }

    #[test]
    fn file_logging_unknown_field_rejected() {
        // deny_unknown_fields contract: typos must fail at startup.
        let yaml = r#"
format: json
filter: info
file:
  path: /tmp/x.log
  weekly: true
"#;
        let result: Result<LoggingConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "unknown field should be rejected");
    }

    // ── Phase 7 P05 T10: ShutdownConfig ─────────────────────────────────────

    #[test]
    fn shutdown_default_when_absent() {
        let yaml = "upstreams: {}\nroutes: []";
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.shutdown.plugin_flush_secs, 5);
    }

    #[test]
    fn shutdown_explicit_value_parsed() {
        let yaml = r#"
upstreams: {}
routes: []
shutdown:
  plugin_flush_secs: 30
"#;
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.shutdown.plugin_flush_secs, 30);
    }

    #[test]
    fn shutdown_rejects_unknown_field() {
        let yaml = r#"
upstreams: {}
routes: []
shutdown:
  plugin_flush_secs: 5
  bogus_field: 1
"#;
        let r: Result<GatewayConfig, _> = serde_yaml::from_str(yaml);
        assert!(r.is_err(), "deny_unknown_fields must reject `bogus_field`");
    }
}
