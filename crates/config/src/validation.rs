use crate::schema::{GatewayConfig, UpstreamConfig};
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
}

const VALID_FRONTENDS: &[&str] = &[
    "anthropic_messages",
    "anthropic",
    "openai_chat",
    "openai",
    "openai_responses",
    "responses",
];

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
            if !cfg.upstreams.contains_key(name) {
                // allow "copilot" as a special upstream name when copilot config present
                let is_copilot = name == "copilot"
                    && cfg
                        .upstreams
                        .values()
                        .any(|u| matches!(u, UpstreamConfig::GithubCopilot));
                if !is_copilot {
                    return Err(ValidationError::UnknownUpstream(name.clone()));
                }
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
/// 3. Every referenced upstream name resolves to a configured upstream.
/// 4. Retry policy fields are sane (`max_attempts >= 1`,
///    `total_budget_ms >= initial_backoff_ms`, `multiplier > 1.0`).
/// 5. Breaker policy fields are sane (`failure_threshold_pct` in `1..=100`,
///    `min_requests >= 1`).
///
/// Returns a human-readable `String` so config-load callers can surface the
/// message verbatim. The `validate()` entry point above returns
/// `ValidationError` instead; both can be called.
pub fn validate_routes(cfg: &GatewayConfig) -> Result<(), String> {
    for (i, route) in cfg.routes.iter().enumerate() {
        let has_singular = route.upstream.is_some() || route.upstream_model.is_some();
        let has_array = !route.upstreams.is_empty();

        // Rule 1: exactly one shape per route.
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
                if route.upstream.is_none() || route.upstream_model.is_none() {
                    return Err(format!(
                        "route[{i}] '{}/{}' singular form requires both `upstream` and `upstream_model`",
                        route.frontend, route.model
                    ));
                }
            }
            (false, true) => {} // valid array form
        }

        // Rule 2: every referenced upstream name must be configured.
        let upstream_names: Vec<&str> = if has_array {
            route.upstreams.iter().map(|u| u.name.as_str()).collect()
        } else {
            vec![route.upstream.as_deref().unwrap()]
        };
        for name in upstream_names {
            if !cfg.upstreams.contains_key(name) {
                return Err(format!(
                    "route[{i}] '{}/{}' references unknown upstream '{}'",
                    route.frontend, route.model, name
                ));
            }
        }

        // Rule 3: retry policy sanity.
        if route.retry.max_attempts < 1 {
            return Err(format!("route[{i}] retry.max_attempts must be >= 1"));
        }
        if route.retry.total_budget_ms < route.retry.initial_backoff_ms {
            return Err(format!(
                "route[{i}] retry.total_budget_ms ({}) must be >= initial_backoff_ms ({})",
                route.retry.total_budget_ms, route.retry.initial_backoff_ms
            ));
        }
        if route.retry.multiplier <= 1.0 {
            return Err(format!(
                "route[{i}] retry.multiplier ({}) must be > 1.0 for exponential backoff",
                route.retry.multiplier
            ));
        }

        // Rule 4: breaker policy sanity.
        if !(1..=100).contains(&route.breaker.failure_threshold_pct) {
            return Err(format!(
                "route[{i}] breaker.failure_threshold_pct ({}) must be in 1..=100",
                route.breaker.failure_threshold_pct
            ));
        }
        if route.breaker.min_requests < 1 {
            return Err(format!("route[{i}] breaker.min_requests must be >= 1"));
        }
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
}
