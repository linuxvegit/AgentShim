use crate::schema::{BucketConfigYaml, GatewayConfig, UpstreamConfig};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("port cannot be 0")]
    ZeroPort,
    #[error("route references unknown upstream: {0}")]
    UnknownUpstream(String),
    #[error("duplicate route alias (frontend={0}, model={1})")]
    DuplicateAlias(String, String),
    #[error("unknown frontend protocol: {0} (must be 'anthropic_messages', 'openai_chat', or 'openai_responses')")]
    UnknownFrontend(String),
    #[error("upstream {0}: {1}")]
    InvalidUpstream(String, String),
    #[error("invalid route: {0}")]
    InvalidRoute(String),
}

const VALID_FRONTENDS: &[&str] = &[
    "anthropic_messages",
    "anthropic",
    "openai_chat",
    "openai",
    "openai_responses",
    "responses",
];

/// Returns true if `name` resolves to a configured upstream block.
///
/// Direct lookup wins. As a legacy back-compat case (carried from v0.3),
/// the virtual name `"copilot"` resolves whenever any
/// `UpstreamConfig::GithubCopilot` block is configured, regardless of its key
/// in `cfg.upstreams`. Operators upgrading from v0.3 may have routes that say
/// `upstream: copilot` while the actual upstream entry is keyed
/// `github_copilot:` (or any other name).
///
/// Both [`validate`] and [`validate_routes`] go through this helper so the two
/// never disagree on what counts as "configured".
fn upstream_is_configured(cfg: &GatewayConfig, name: &str) -> bool {
    if cfg.upstreams.contains_key(name) {
        return true;
    }
    if name == "copilot"
        && cfg
            .upstreams
            .values()
            .any(|u| matches!(u, UpstreamConfig::GithubCopilot))
    {
        return true;
    }
    false
}

pub fn validate(cfg: &GatewayConfig) -> Result<(), ValidationError> {
    if cfg.server.port == 0 {
        return Err(ValidationError::ZeroPort);
    }

    let mut seen = std::collections::HashSet::new();
    for route in &cfg.routes {
        if !VALID_FRONTENDS.contains(&route.frontend.as_str()) {
            return Err(ValidationError::UnknownFrontend(route.frontend.clone()));
        }
        // Collect every upstream name the route references, regardless of
        // whether it uses the singular or array shape. Validation of "exactly
        // one shape" lives in `validate_routes` below.
        let referenced: Vec<String> = if !route.upstreams.is_empty() {
            route.upstreams.iter().map(|u| u.name.clone()).collect()
        } else if let Some(name) = route.upstream.as_ref() {
            vec![name.clone()]
        } else {
            // No upstream configured at all — surfaced as a clearer error by
            // `validate_routes`. Skip the unknown-upstream check here.
            vec![]
        };
        for name in &referenced {
            if !upstream_is_configured(cfg, name) {
                return Err(ValidationError::UnknownUpstream(name.clone()));
            }
        }
        let key = (route.frontend.clone(), route.model.clone());
        if !seen.insert(key.clone()) {
            return Err(ValidationError::DuplicateAlias(key.0, key.1));
        }
    }

    for (name, upstream) in &cfg.upstreams {
        if let UpstreamConfig::Anthropic(a) = upstream {
            validate_oai_style_upstream(
                name,
                &a.base_url,
                a.api_key.expose(),
                a.request_timeout_secs,
            )?;
            if a.anthropic_version.is_empty() {
                return Err(ValidationError::InvalidUpstream(
                    name.clone(),
                    "anthropic_version must be non-empty".to_string(),
                ));
            }
        } else if let UpstreamConfig::Deepseek(d) = upstream {
            validate_oai_style_upstream(
                name,
                &d.base_url,
                d.api_key.expose(),
                d.request_timeout_secs,
            )?;
        } else if let UpstreamConfig::Gemini(g) = upstream {
            validate_oai_style_upstream(
                name,
                &g.base_url,
                g.api_key.expose(),
                g.request_timeout_secs,
            )?;
        }
    }

    // Phase 4 (Plan 04 P01) per-route validation. Wired into the canonical
    // entry point so `gateway::commands::serve`/`validate_config` (and any
    // other caller of `validate()`) get the new rules for free.
    validate_routes(cfg).map_err(ValidationError::InvalidRoute)?;

    // Phase 4 (Plan 04 P04 T3) auth + rate_limit validation. Reuses
    // `InvalidRoute` as the catch-all "config-shape error" tunnel so the
    // existing CLI surfaces these errors without churning the public enum.
    validate_rate_limit(cfg).map_err(ValidationError::InvalidRoute)?;
    validate_auth(cfg).map_err(ValidationError::InvalidRoute)?;

    Ok(())
}

/// Shared validation for OpenAI-style upstream configs. Verifies that the
/// `api_key` and `base_url` are non-empty, the `base_url` uses an http(s)
/// scheme, and the `request_timeout_secs` is greater than zero.
///
/// Anthropic-specific checks (e.g. `anthropic_version` non-empty) are handled
/// at the call site after this helper returns.
fn validate_oai_style_upstream(
    name: &str,
    base_url: &str,
    api_key: &str,
    timeout: u64,
) -> Result<(), ValidationError> {
    if api_key.is_empty() {
        return Err(ValidationError::InvalidUpstream(
            name.to_string(),
            "api_key must be non-empty".to_string(),
        ));
    }
    if base_url.is_empty() {
        return Err(ValidationError::InvalidUpstream(
            name.to_string(),
            "base_url must be non-empty".to_string(),
        ));
    }
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Err(ValidationError::InvalidUpstream(
            name.to_string(),
            "base_url must start with http:// or https://".to_string(),
        ));
    }
    if timeout == 0 {
        return Err(ValidationError::InvalidUpstream(
            name.to_string(),
            "request_timeout_secs must be greater than 0".to_string(),
        ));
    }
    Ok(())
}

/// Phase 4 (Plan 04 P01) per-route validation. Enforces:
/// 1. Each route uses **exactly one** upstream shape (singular OR array).
/// 2. Singular form requires both `upstream` and `upstream_model`.
/// 3. Every referenced upstream name resolves to a configured upstream
///    (via [`upstream_is_configured`], which honors the legacy `copilot`
///    virtual name).
/// 4. Retry policy fields are sane (`max_attempts >= 1`,
///    `total_budget_ms >= initial_backoff_ms`, `multiplier > 1.0` and finite).
/// 5. `retry.jitter_pct` is finite and within `0.0..=100.0` (negative or
///    non-finite values would crash `rand::gen_range` on every retry).
/// 6. Breaker policy fields are sane (`failure_threshold_pct` in `1..=100`,
///    `min_requests >= 1`).
///
/// Returns on first failure; rerun after each fix to surface subsequent issues.
///
/// Returns a human-readable `String` so config-load callers can surface the
/// message verbatim. The `validate()` entry point wraps these into
/// `ValidationError::InvalidRoute` and is the canonical config-load call site;
/// callers that already hold a `&GatewayConfig` may invoke `validate_routes`
/// directly.
pub fn validate_routes(cfg: &GatewayConfig) -> Result<(), String> {
    for (i, route) in cfg.routes.iter().enumerate() {
        let has_singular = route.upstream.is_some() || route.upstream_model.is_some();
        let has_array = !route.upstreams.is_empty();

        // Rule 1: exactly one shape per route (singular OR array, never both, never neither).
        match (has_singular, has_array) {
            (true, true) => {
                return Err(format!(
                    "route[{i}] '{}/{}' specifies both `upstream`/`upstream_model` and `upstreams`; pick one form",
                    route.frontend, route.model
                ));
            }
            (false, false) => {
                return Err(format!(
                    "route[{i}] '{}/{}' has no upstream configured; provide either `upstream`+`upstream_model` or `upstreams`",
                    route.frontend, route.model
                ));
            }
            (true, false) => {
                // Rule 2: singular form requires BOTH `upstream` and `upstream_model`.
                if route.upstream.is_none() || route.upstream_model.is_none() {
                    return Err(format!(
                        "route[{i}] '{}/{}' singular form requires both `upstream` and `upstream_model`",
                        route.frontend, route.model
                    ));
                }
            }
            (false, true) => {} // valid array form
        }

        // Rule 3: every referenced upstream name must be configured. Honors the
        // legacy `copilot` virtual name so v0.3 configs keep working — see
        // `upstream_is_configured` for the full back-compat rule.
        let upstream_names: Vec<&str> = if has_array {
            route.upstreams.iter().map(|u| u.name.as_str()).collect()
        } else {
            vec![route.upstream.as_deref().unwrap()]
        };
        for name in upstream_names {
            if !upstream_is_configured(cfg, name) {
                return Err(format!(
                    "route[{i}] '{}/{}' references unknown upstream '{}'",
                    route.frontend, route.model, name
                ));
            }
        }

        // Rule 4: retry policy sanity.
        if route.retry.max_attempts == 0 {
            return Err(format!("route[{i}] retry.max_attempts must be >= 1"));
        }
        if route.retry.total_budget_ms < route.retry.initial_backoff_ms {
            return Err(format!(
                "route[{i}] retry.total_budget_ms ({}) must be >= initial_backoff_ms ({})",
                route.retry.total_budget_ms, route.retry.initial_backoff_ms
            ));
        }
        if !route.retry.multiplier.is_finite() || route.retry.multiplier <= 1.0 {
            return Err(format!(
                "route[{i}] retry.multiplier ({}) must be > 1.0 for exponential backoff",
                route.retry.multiplier
            ));
        }

        // Rule 5: jitter_pct must be a finite, non-negative number <= 100.
        // (rand::gen_range panics on inverted/empty ranges, so a negative or
        // NaN jitter would crash the gateway on every retry attempt.)
        if !route.retry.jitter_pct.is_finite()
            || route.retry.jitter_pct < 0.0
            || route.retry.jitter_pct > 100.0
        {
            return Err(format!(
                "route[{i}] retry.jitter_pct ({}) must be in 0.0..=100.0 (finite, non-negative)",
                route.retry.jitter_pct
            ));
        }

        // Rule 6: breaker policy sanity.
        if !(1..=100).contains(&route.breaker.failure_threshold_pct) {
            return Err(format!(
                "route[{i}] breaker.failure_threshold_pct ({}) must be in 1..=100",
                route.breaker.failure_threshold_pct
            ));
        }
        if route.breaker.min_requests == 0 {
            return Err(format!("route[{i}] breaker.min_requests must be >= 1"));
        }
    }
    Ok(())
}

/// Phase 4 (Plan 04 P04 T3) `rate_limit` block validation. Implements
/// rules 7, 8, 9, and 10 from the §4.4 design:
///   7. `per_route` keys reference existing routes (`<frontend>/<model>`).
///   8. `per_upstream` keys reference configured upstreams.
///   9. `per_key.overrides` keys must start with `sha256:` (no plaintext keys).
///  10. Every bucket has `rate_per_sec > 0` AND `burst >= 1`.
///
/// Returns a human-readable `String` so the canonical `validate()` entry point
/// can surface the message verbatim through `ValidationError::InvalidRoute`.
///
/// Short-circuits on `rate_limit.enabled = false`: if rate limiting is off
/// at the master switch, stale or partially-edited bucket entries should
/// not surface as hard config errors at startup. The fields are still
/// parsed and stored — they're just not validated against the route table
/// or bucket bounds. Operators who want stricter pre-validation should
/// flip `enabled: true`.
pub fn validate_rate_limit(cfg: &GatewayConfig) -> Result<(), String> {
    if !cfg.rate_limit.enabled {
        return Ok(());
    }

    // Rule 7: per_route keys reference existing routes.
    for key in cfg.rate_limit.per_route.keys() {
        let parts: Vec<&str> = key.splitn(2, '/').collect();
        if parts.len() != 2 {
            return Err(format!(
                "rate_limit.per_route key '{key}' must be '<frontend>/<model>'"
            ));
        }
        let exists = cfg
            .routes
            .iter()
            .any(|r| r.frontend == parts[0] && r.model == parts[1]);
        if !exists {
            return Err(format!(
                "rate_limit.per_route key '{key}' references non-existent route"
            ));
        }
    }

    // Rule 8: per_upstream keys reference configured upstreams.
    for key in cfg.rate_limit.per_upstream.keys() {
        if !cfg.upstreams.contains_key(key) {
            return Err(format!(
                "rate_limit.per_upstream key '{key}' references non-existent upstream"
            ));
        }
    }

    // Rule 9: per_key overrides are SHA-256 hashes (sha256:<64 lowercase hex>).
    for key in cfg.rate_limit.per_key.overrides.keys() {
        check_sha256_key(key, "rate_limit.per_key.overrides")?;
    }

    // Rule 10: every bucket has rate_per_sec > 0 AND burst >= 1.
    let validate_bucket = |b: &BucketConfigYaml, ctx: &str| -> Result<(), String> {
        if b.rate_per_sec == 0 {
            return Err(format!("{ctx} rate_per_sec must be > 0"));
        }
        if b.burst == 0 {
            return Err(format!("{ctx} burst must be >= 1"));
        }
        Ok(())
    };
    if let Some(b) = &cfg.rate_limit.per_key.default {
        validate_bucket(b, "rate_limit.per_key.default")?;
    }
    if let Some(b) = &cfg.rate_limit.per_key.anonymous {
        validate_bucket(b, "rate_limit.per_key.anonymous")?;
    }
    for (k, v) in &cfg.rate_limit.per_key.overrides {
        validate_bucket(v, &format!("rate_limit.per_key.overrides[{k}]"))?;
    }
    for (k, v) in &cfg.rate_limit.per_route {
        validate_bucket(v, &format!("rate_limit.per_route[{k}]"))?;
    }
    for (k, v) in &cfg.rate_limit.per_upstream {
        validate_bucket(v, &format!("rate_limit.per_upstream[{k}]"))?;
    }
    if cfg.rate_limit.per_ip.enabled {
        if cfg.rate_limit.per_ip.rate_per_sec == 0 {
            return Err("rate_limit.per_ip.rate_per_sec must be > 0".into());
        }
        if cfg.rate_limit.per_ip.burst == 0 {
            return Err("rate_limit.per_ip.burst must be >= 1".into());
        }
    }

    Ok(())
}

/// Phase 4 (Plan 04 P04 T3) `auth` block validation. Enforces:
///   - `auth.required=true` requires `auth.enabled=true`.
///   - Every `auth.keys` key must be a `sha256:<64 lowercase hex>` value
///     (catches plaintext-key paste mistakes AND typo-shaped hashes).
pub fn validate_auth(cfg: &GatewayConfig) -> Result<(), String> {
    if cfg.auth.required && !cfg.auth.enabled {
        return Err("auth.required=true requires auth.enabled=true".into());
    }
    for key in cfg.auth.keys.keys() {
        check_sha256_key(key, "auth.keys")?;
    }
    Ok(())
}

/// Validate that `key` is shaped `sha256:<64 lowercase hex>`.
///
/// Operators paste these from the recipe in `docs/resilience.md`
/// (`echo -n key | sha256sum | awk '{print "sha256:" $1}'`). We catch
/// three classes of paste mistake here:
///   - Missing `sha256:` prefix (looks like a plaintext API key).
///   - Wrong hex length (truncated or extra chars).
///   - Non-hex characters in the hash (e.g. `sha256:nothex...`).
fn check_sha256_key(key: &str, ctx: &str) -> Result<(), String> {
    let hex = match key.strip_prefix("sha256:") {
        Some(h) => h,
        None => {
            return Err(format!(
                "{ctx} key '{key}' must start with 'sha256:'; \
                 do not paste plaintext API keys here"
            ));
        }
    };
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "{ctx} key '{key}' has malformed hash; expected 'sha256:' \
             followed by 64 lowercase hex characters"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::*;
    use crate::secrets::Secret;
    use std::collections::BTreeMap;

    fn minimal_config() -> GatewayConfig {
        GatewayConfig {
            server: ServerConfig::default(),
            logging: LoggingConfig::default(),
            upstreams: BTreeMap::new(),
            routes: vec![],
            auth: AuthConfig::default(),
            rate_limit: RateLimitConfig::default(),
            copilot: None,
        }
    }

    #[test]
    fn valid_config_passes() {
        let cfg = minimal_config();
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn zero_port_fails() {
        let mut cfg = minimal_config();
        cfg.server.port = 0;
        assert!(matches!(validate(&cfg), Err(ValidationError::ZeroPort)));
    }

    #[test]
    fn unknown_upstream_fails() {
        let mut cfg = minimal_config();
        cfg.routes.push(RouteEntry {
            frontend: "openai_chat".to_string(),
            model: "gpt-4".to_string(),
            upstream: Some("nonexistent".to_string()),
            upstream_model: Some("gpt-4".to_string()),
            upstreams: vec![],
            reasoning_effort: None,
            anthropic_beta: None,
            retry: RetryConfig::default(),
            breaker: BreakerConfig::default(),
        });
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::UnknownUpstream(_))
        ));
    }

    #[test]
    fn duplicate_alias_fails() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "upstream1".to_string(),
            UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
                base_url: "https://api.openai.com".to_string(),
                api_key: Secret::new("key"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
            }),
        );
        cfg.routes.push(RouteEntry {
            frontend: "openai_chat".to_string(),
            model: "gpt-4".to_string(),
            upstream: Some("upstream1".to_string()),
            upstream_model: Some("gpt-4".to_string()),
            upstreams: vec![],
            reasoning_effort: None,
            anthropic_beta: None,
            retry: RetryConfig::default(),
            breaker: BreakerConfig::default(),
        });
        cfg.routes.push(RouteEntry {
            frontend: "openai_chat".to_string(),
            model: "gpt-4".to_string(),
            upstream: Some("upstream1".to_string()),
            upstream_model: Some("gpt-4".to_string()),
            upstreams: vec![],
            reasoning_effort: None,
            anthropic_beta: None,
            retry: RetryConfig::default(),
            breaker: BreakerConfig::default(),
        });
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::DuplicateAlias(_, _))
        ));
    }

    #[test]
    fn unknown_frontend_fails() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "upstream1".to_string(),
            UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
                base_url: "https://api.openai.com".to_string(),
                api_key: Secret::new("key"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
            }),
        );
        cfg.routes.push(RouteEntry {
            frontend: "unknown_protocol".to_string(),
            model: "gpt-4".to_string(),
            upstream: Some("upstream1".to_string()),
            upstream_model: Some("gpt-4".to_string()),
            upstreams: vec![],
            reasoning_effort: None,
            anthropic_beta: None,
            retry: RetryConfig::default(),
            breaker: BreakerConfig::default(),
        });
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::UnknownFrontend(_))
        ));
    }

    #[test]
    fn anthropic_upstream_validation_passes() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "anthropic".to_string(),
            UpstreamConfig::Anthropic(AnthropicUpstream {
                base_url: "https://api.anthropic.com".to_string(),
                api_key: Secret::new("sk-ant-test"),
                anthropic_version: "2023-06-01".to_string(),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
            }),
        );
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn anthropic_upstream_empty_api_key_fails() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "anthropic".to_string(),
            UpstreamConfig::Anthropic(AnthropicUpstream {
                base_url: "https://api.anthropic.com".to_string(),
                api_key: Secret::new(""),
                anthropic_version: "2023-06-01".to_string(),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
            }),
        );
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::InvalidUpstream(_, _))
        ));
    }

    #[test]
    fn anthropic_upstream_bad_base_url_fails() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "anthropic".to_string(),
            UpstreamConfig::Anthropic(AnthropicUpstream {
                base_url: "ftp://api.anthropic.com".to_string(),
                api_key: Secret::new("sk-ant-test"),
                anthropic_version: "2023-06-01".to_string(),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
            }),
        );
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::InvalidUpstream(_, _))
        ));
    }

    #[test]
    fn deepseek_upstream_validation_passes() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "deepseek".to_string(),
            UpstreamConfig::Deepseek(DeepseekUpstream {
                base_url: "https://api.deepseek.com/v1".to_string(),
                api_key: Secret::new("sk-deepseek-test"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
            }),
        );
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn deepseek_validation_rejects_empty_api_key() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "deepseek".to_string(),
            UpstreamConfig::Deepseek(DeepseekUpstream {
                base_url: "https://api.deepseek.com/v1".to_string(),
                api_key: Secret::new(""),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
            }),
        );
        match validate(&cfg) {
            Err(ValidationError::InvalidUpstream(name, msg)) => {
                assert_eq!(name, "deepseek");
                assert!(
                    msg.contains("api_key"),
                    "expected api_key error, got: {msg}"
                );
            }
            other => panic!("expected InvalidUpstream, got {other:?}"),
        }
    }

    #[test]
    fn deepseek_validation_rejects_bad_base_url() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "deepseek".to_string(),
            UpstreamConfig::Deepseek(DeepseekUpstream {
                base_url: "ftp://api.deepseek.com/v1".to_string(),
                api_key: Secret::new("sk-deepseek-test"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
            }),
        );
        match validate(&cfg) {
            Err(ValidationError::InvalidUpstream(name, msg)) => {
                assert_eq!(name, "deepseek");
                assert!(
                    msg.contains("base_url"),
                    "expected base_url error, got: {msg}"
                );
            }
            other => panic!("expected InvalidUpstream, got {other:?}"),
        }
    }

    #[test]
    fn deepseek_validation_rejects_zero_timeout() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "deepseek".to_string(),
            UpstreamConfig::Deepseek(DeepseekUpstream {
                base_url: "https://api.deepseek.com/v1".to_string(),
                api_key: Secret::new("sk-deepseek-test"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 0,
            }),
        );
        match validate(&cfg) {
            Err(ValidationError::InvalidUpstream(name, msg)) => {
                assert_eq!(name, "deepseek");
                assert!(
                    msg.contains("request_timeout_secs"),
                    "expected request_timeout_secs error, got: {msg}"
                );
            }
            other => panic!("expected InvalidUpstream, got {other:?}"),
        }
    }

    #[test]
    fn gemini_upstream_validation_passes() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "gemini".to_string(),
            UpstreamConfig::Gemini(GeminiUpstream {
                base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
                api_key: Secret::new("ai-studio-test"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
            }),
        );
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn gemini_validation_rejects_empty_api_key() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "gemini".to_string(),
            UpstreamConfig::Gemini(GeminiUpstream {
                base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
                api_key: Secret::new(""),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
            }),
        );
        match validate(&cfg) {
            Err(ValidationError::InvalidUpstream(name, msg)) => {
                assert_eq!(name, "gemini");
                assert!(
                    msg.contains("api_key"),
                    "expected api_key error, got: {msg}"
                );
            }
            other => panic!("expected InvalidUpstream, got {other:?}"),
        }
    }

    #[test]
    fn gemini_validation_rejects_bad_base_url() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "gemini".to_string(),
            UpstreamConfig::Gemini(GeminiUpstream {
                base_url: "ftp://generativelanguage.googleapis.com/v1beta".to_string(),
                api_key: Secret::new("ai-studio-test"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
            }),
        );
        match validate(&cfg) {
            Err(ValidationError::InvalidUpstream(name, msg)) => {
                assert_eq!(name, "gemini");
                assert!(
                    msg.contains("base_url"),
                    "expected base_url error, got: {msg}"
                );
            }
            other => panic!("expected InvalidUpstream, got {other:?}"),
        }
    }

    #[test]
    fn gemini_validation_rejects_zero_timeout() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "gemini".to_string(),
            UpstreamConfig::Gemini(GeminiUpstream {
                base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
                api_key: Secret::new("ai-studio-test"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 0,
            }),
        );
        match validate(&cfg) {
            Err(ValidationError::InvalidUpstream(name, msg)) => {
                assert_eq!(name, "gemini");
                assert!(
                    msg.contains("request_timeout_secs"),
                    "expected request_timeout_secs error, got: {msg}"
                );
            }
            other => panic!("expected InvalidUpstream, got {other:?}"),
        }
    }

    // ── Plan 04 P01 T1: validate_routes coverage ────────────────────────────

    /// Build a `GatewayConfig` with a single configured upstream `openai` and
    /// one route parsed from the supplied YAML snippet. The helper backs the
    /// 5 validate_routes test cases below.
    fn make_cfg_with_route_yaml(route_yaml: &str) -> GatewayConfig {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "openai".to_string(),
            UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
                base_url: "https://api.openai.com".to_string(),
                api_key: Secret::new("sk-test"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
            }),
        );
        let route: RouteEntry = serde_yaml::from_str(route_yaml).expect("route YAML parses");
        cfg.routes.push(route);
        cfg
    }

    #[test]
    fn rejects_route_with_both_shapes() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstream: openai
            upstream_model: gpt-4o
            upstreams:
              - {name: copilot, model: gpt-4o}
        "#,
        );
        let err = validate_routes(&cfg).unwrap_err();
        assert!(err.contains("specifies both"));
    }

    #[test]
    fn rejects_route_with_no_upstream() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
        "#,
        );
        let err = validate_routes(&cfg).unwrap_err();
        assert!(err.contains("no upstream configured"));
    }

    #[test]
    fn rejects_unknown_upstream_reference() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstreams:
              - {name: nonexistent, model: gpt-4o}
        "#,
        );
        // make_cfg_with_route_yaml configures only "openai" in cfg.upstreams.
        let err = validate_routes(&cfg).unwrap_err();
        assert!(err.contains("nonexistent"));
    }

    #[test]
    fn accepts_well_formed_array_route() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstreams:
              - {name: openai, model: gpt-4o-2024-11-20}
        "#,
        );
        validate_routes(&cfg).expect("valid config");
    }

    #[test]
    fn rejects_multiplier_le_one() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstream: openai
            upstream_model: gpt-4o
            retry:
              multiplier: 1.0
        "#,
        );
        let err = validate_routes(&cfg).unwrap_err();
        assert!(err.contains("multiplier"));
    }

    /// Wiring regression for review-issue #1: an invalid YAML must be rejected
    /// by the canonical `validate()` entry point — not just by direct
    /// `validate_routes` calls. Without this, `agent-shim serve --config` would
    /// happily accept multiplier=1.0 in production.
    #[test]
    fn validate_rejects_invalid_route_via_canonical_entry() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstream: openai
            upstream_model: gpt-4o
            retry:
              multiplier: 1.0
        "#,
        );
        match validate(&cfg) {
            Err(ValidationError::InvalidRoute(msg)) => {
                assert!(msg.contains("multiplier"), "got: {msg}");
            }
            other => panic!("expected InvalidRoute, got {other:?}"),
        }
    }

    /// Regression for review-issue #3 (copilot virtual-name consistency):
    /// `validate_routes` must accept `upstream: copilot` whenever any
    /// `github_copilot` upstream block is configured, matching the legacy
    /// behavior of `validate()`. Both validators share `upstream_is_configured`.
    #[test]
    fn validate_routes_accepts_copilot_virtual_name() {
        let mut cfg = minimal_config();
        // Upstream block is keyed `github_copilot` (not `copilot`).
        cfg.upstreams
            .insert("github_copilot".to_string(), UpstreamConfig::GithubCopilot);
        let route: RouteEntry = serde_yaml::from_str(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstream: copilot
            upstream_model: gpt-4o
        "#,
        )
        .expect("route YAML parses");
        cfg.routes.push(route);
        validate_routes(&cfg).expect("copilot virtual name should resolve");
        // And the canonical entry point agrees.
        validate(&cfg).expect("validate() should also pass");
    }

    // ── Plan 04 P01 T2 followup: jitter_pct + NaN multiplier rules ──────────

    #[test]
    fn rejects_negative_jitter_pct() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstream: openai
            upstream_model: gpt-4o
            retry:
              jitter_pct: -25.0
        "#,
        );
        let err = validate_routes(&cfg).unwrap_err();
        assert!(err.contains("jitter_pct"));
    }

    #[test]
    fn rejects_jitter_pct_above_100() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstream: openai
            upstream_model: gpt-4o
            retry:
              jitter_pct: 150.0
        "#,
        );
        let err = validate_routes(&cfg).unwrap_err();
        assert!(err.contains("jitter_pct"));
    }

    #[test]
    fn rejects_nan_multiplier() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstream: openai
            upstream_model: gpt-4o
            retry:
              multiplier: .nan
        "#,
        );
        let err = validate_routes(&cfg).unwrap_err();
        assert!(err.contains("multiplier"));
    }

    // ── Plan 04 P04 T3: rate_limit + auth validation ────────────────────────

    #[test]
    fn rejects_per_route_unknown_route() {
        // NOTE: schema rename quirk — the OpenAiCompatible variant deserializes
        // as `open_ai_compatible` (snake_case'd from `OpenAiCompatible`), not
        // `openai_compatible`. The plan's YAML had the wrong tag; corrected
        // here so the YAML parses far enough to exercise the rate_limit rule.
        let cfg_yaml = r#"
upstreams:
  oai: {type: open_ai_compatible, base_url: "https://x", api_key: "x"}
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstream: oai
    upstream_model: gpt-4o
rate_limit:
  enabled: true
  per_route:
    "anthropic_messages/claude-opus-4-7": {rate_per_sec: 10, burst: 30}
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        assert!(validate_rate_limit(&cfg).is_err());
    }

    #[test]
    fn rejects_per_key_override_without_sha256_prefix() {
        let cfg_yaml = r#"
rate_limit:
  enabled: true
  per_key:
    overrides:
      "plaintext-key": {rate_per_sec: 100, burst: 300}
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        let err = validate_rate_limit(&cfg).unwrap_err();
        assert!(err.contains("sha256:"));
    }

    #[test]
    fn rejects_zero_burst() {
        let cfg_yaml = r#"
rate_limit:
  enabled: true
  per_key:
    default: {rate_per_sec: 10, burst: 0}
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        assert!(validate_rate_limit(&cfg).is_err());
    }

    #[test]
    fn rejects_required_without_enabled() {
        let cfg_yaml = r#"
auth:
  enabled: false
  required: true
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        assert!(validate_auth(&cfg).is_err());
    }

    // ── Followup tests (rule-8 coverage + sha256 format + disabled bypass +
    //     v0.3 back-compat) — added in the P04 T3 followup ──

    #[test]
    fn rejects_per_upstream_unknown_upstream() {
        // Rule 8: per_upstream key must reference an upstream that exists.
        // The plan has rule-7 coverage (per_route → unknown route) but no
        // companion test for per_upstream, leaving the rule provable only
        // by inspection.
        let cfg_yaml = r#"
upstreams:
  oai: {type: open_ai_compatible, base_url: "http://x", api_key: "x"}
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstream: oai
    upstream_model: gpt-4o
rate_limit:
  enabled: true
  per_upstream:
    "ghost-upstream": {rate_per_sec: 10, burst: 30}
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        let err = validate_rate_limit(&cfg).unwrap_err();
        assert!(err.contains("ghost-upstream"));
        assert!(err.contains("non-existent upstream"));
    }

    #[test]
    fn rejects_auth_key_without_sha256_prefix() {
        // Auth.keys parallel to rule 9: a plaintext API key pasted into
        // auth.keys (instead of its hash) is the easy paste mistake to
        // catch. The plan tested validate_auth's required→enabled rule
        // but not the sha256: prefix path.
        let cfg_yaml = r#"
auth:
  enabled: true
  keys:
    "plaintext-api-key":
      label: "alice"
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        let err = validate_auth(&cfg).unwrap_err();
        assert!(err.contains("sha256:"));
    }

    #[test]
    fn rejects_sha256_with_wrong_hex_length() {
        // The original `starts_with("sha256:")` check accepted any prefix.
        // This pins the new format check: 64 lowercase hex chars after
        // the prefix.
        let cfg_yaml = r#"
auth:
  enabled: true
  keys:
    "sha256:abc":
      label: "truncated"
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        let err = validate_auth(&cfg).unwrap_err();
        assert!(err.contains("malformed hash"));
    }

    #[test]
    fn rejects_sha256_with_non_hex_chars() {
        // `sha256:nothex...` was previously accepted by the prefix-only
        // check. Now rejected.
        let cfg_yaml = r#"
auth:
  enabled: true
  keys:
    "sha256:nothex01234567890123456789012345678901234567890123456789012345":
      label: "non-hex"
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        let err = validate_auth(&cfg).unwrap_err();
        assert!(err.contains("malformed hash"));
    }

    #[test]
    fn well_formed_sha256_hash_passes() {
        // Positive case: a valid sha256:<64 lowercase hex> validates.
        // Without this, a regression that broke the happy path would
        // only be caught by the more end-to-end smoke tests.
        let cfg_yaml = r#"
auth:
  enabled: true
  keys:
    "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824":
      label: "alice"
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        validate_auth(&cfg).expect("well-formed sha256 hash must validate");
    }

    #[test]
    fn rate_limit_disabled_skips_all_validation() {
        // Master switch off → stale or partially-edited rate-limit fields
        // should NOT surface as hard config errors. Operators flipping
        // `enabled: false` to silence rate limiting in an emergency
        // shouldn't be forced to clean up the rest of the block at the
        // same time.
        let cfg_yaml = r#"
upstreams:
  oai: {type: open_ai_compatible, base_url: "http://x", api_key: "x"}
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstream: oai
    upstream_model: gpt-4o
rate_limit:
  enabled: false
  per_route:
    "anthropic_messages/claude-opus-4-7": {rate_per_sec: 0, burst: 0}
  per_upstream:
    "ghost": {rate_per_sec: 10, burst: 30}
  per_key:
    overrides:
      "plaintext-key": {rate_per_sec: 100, burst: 300}
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        validate_rate_limit(&cfg).expect("disabled rate_limit must skip validation entirely");
    }

    #[test]
    fn v03_config_without_auth_or_rate_limit_blocks_parses() {
        // Direct v0.3 back-compat assertion: a config with neither block
        // must parse cleanly via #[serde(default)] AND pass validation.
        // Existing v0.3 fixtures cover this transitively, but the
        // assertion is fragile under future schema changes.
        let cfg_yaml = r#"
upstreams:
  oai: {type: open_ai_compatible, base_url: "http://x", api_key: "x"}
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstream: oai
    upstream_model: gpt-4o
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        validate_rate_limit(&cfg).expect("default rate_limit must validate");
        validate_auth(&cfg).expect("default auth must validate");
        // Both Default impls produce the disabled state.
        assert!(!cfg.auth.enabled);
        assert!(!cfg.auth.required);
        assert!(!cfg.rate_limit.enabled);
    }
}
