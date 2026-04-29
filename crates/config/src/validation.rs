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
    #[error("unknown frontend protocol: {0} (must be 'anthropic_messages' or 'openai_chat')")]
    UnknownFrontend(String),
}

const VALID_FRONTENDS: &[&str] = &["anthropic_messages", "openai_chat"];

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
        });
        cfg.routes.push(RouteEntry {
            frontend: "openai_chat".to_string(),
            model: "gpt-4".to_string(),
            upstream: "upstream1".to_string(),
            upstream_model: "gpt-4".to_string(),
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
        });
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::UnknownFrontend(_))
        ));
    }
}
