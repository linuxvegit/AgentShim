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
            if a.api_key.expose().is_empty() {
                return Err(ValidationError::InvalidUpstream(
                    name.clone(),
                    "api_key must be non-empty".to_string(),
                ));
            }
            if a.base_url.is_empty() {
                return Err(ValidationError::InvalidUpstream(
                    name.clone(),
                    "base_url must be non-empty".to_string(),
                ));
            }
            if !a.base_url.starts_with("http://") && !a.base_url.starts_with("https://") {
                return Err(ValidationError::InvalidUpstream(
                    name.clone(),
                    "base_url must start with http:// or https://".to_string(),
                ));
            }
            if a.anthropic_version.is_empty() {
                return Err(ValidationError::InvalidUpstream(
                    name.clone(),
                    "anthropic_version must be non-empty".to_string(),
                ));
            }
        }
        // TODO follow-up: api_key + base_url checks duplicate the Anthropic
        // branch above. Extract a helper once a third upstream lands (Plan
        // 03/04 cleanup ticket).
        if let UpstreamConfig::Deepseek(d) = upstream {
            if d.api_key.expose().is_empty() {
                return Err(ValidationError::InvalidUpstream(
                    name.clone(),
                    "api_key must be non-empty".to_string(),
                ));
            }
            if d.base_url.is_empty() {
                return Err(ValidationError::InvalidUpstream(
                    name.clone(),
                    "base_url must be non-empty".to_string(),
                ));
            }
            if !d.base_url.starts_with("http://") && !d.base_url.starts_with("https://") {
                return Err(ValidationError::InvalidUpstream(
                    name.clone(),
                    "base_url must start with http:// or https://".to_string(),
                ));
            }
            if d.request_timeout_secs == 0 {
                return Err(ValidationError::InvalidUpstream(
                    name.clone(),
                    "request_timeout_secs must be greater than 0".to_string(),
                ));
            }
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
                api_key: Secret::new("sk-deepseek-test"),
                base_url: "https://api.deepseek.com/v1".to_string(),
                request_timeout_secs: 30,
                default_headers: BTreeMap::new(),
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
                api_key: Secret::new(""),
                base_url: "https://api.deepseek.com/v1".to_string(),
                request_timeout_secs: 30,
                default_headers: BTreeMap::new(),
            }),
        );
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::InvalidUpstream(_, _))
        ));
    }

    #[test]
    fn deepseek_validation_rejects_bad_base_url() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "deepseek".to_string(),
            UpstreamConfig::Deepseek(DeepseekUpstream {
                api_key: Secret::new("sk-deepseek-test"),
                base_url: "ftp://api.deepseek.com/v1".to_string(),
                request_timeout_secs: 30,
                default_headers: BTreeMap::new(),
            }),
        );
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::InvalidUpstream(_, _))
        ));
    }

    #[test]
    fn deepseek_validation_rejects_zero_timeout() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "deepseek".to_string(),
            UpstreamConfig::Deepseek(DeepseekUpstream {
                api_key: Secret::new("sk-deepseek-test"),
                base_url: "https://api.deepseek.com/v1".to_string(),
                request_timeout_secs: 0,
                default_headers: BTreeMap::new(),
            }),
        );
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::InvalidUpstream(_, _))
        ));
    }
}
