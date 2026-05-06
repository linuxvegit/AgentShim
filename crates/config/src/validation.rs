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
        if !cfg.upstreams.contains_key(&route.upstream) {
            // allow "copilot" as a special upstream name when copilot config present
            let is_copilot = route.upstream == "copilot"
                && cfg
                    .upstreams
                    .values()
                    .any(|u| matches!(u, UpstreamConfig::GithubCopilot));
            if !is_copilot {
                return Err(ValidationError::UnknownUpstream(route.upstream.clone()));
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
            upstream: "nonexistent".to_string(),
            upstream_model: "gpt-4".to_string(),
            reasoning_effort: None,
            anthropic_beta: None,
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
            upstream: "upstream1".to_string(),
            upstream_model: "gpt-4".to_string(),
            reasoning_effort: None,
            anthropic_beta: None,
        });
        cfg.routes.push(RouteEntry {
            frontend: "openai_chat".to_string(),
            model: "gpt-4".to_string(),
            upstream: "upstream1".to_string(),
            upstream_model: "gpt-4".to_string(),
            reasoning_effort: None,
            anthropic_beta: None,
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
            upstream: "upstream1".to_string(),
            upstream_model: "gpt-4".to_string(),
            reasoning_effort: None,
            anthropic_beta: None,
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
}
