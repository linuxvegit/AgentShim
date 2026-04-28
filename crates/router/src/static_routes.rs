use std::collections::HashMap;

use agent_shim_config::GatewayConfig;
use agent_shim_core::{BackendTarget, FrontendKind};

use crate::{RouteError, Router};

/// Key for the route table.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RouteKey {
    frontend: FrontendKind,
    model: String,
}

/// A static router built from `GatewayConfig.routes`.
pub struct StaticRouter {
    routes: HashMap<RouteKey, BackendTarget>,
}

impl StaticRouter {
    pub fn from_config(cfg: &GatewayConfig) -> Self {
        let mut routes = HashMap::new();
        for entry in &cfg.routes {
            let frontend = match entry.frontend.as_str() {
                "anthropic_messages" | "anthropic" => FrontendKind::AnthropicMessages,
                "openai_chat" | "openai" => FrontendKind::OpenAiChat,
                other => {
                    tracing::warn!("unknown frontend kind in route config: {other}");
                    continue;
                }
            };
            let key = RouteKey {
                frontend,
                model: entry.model.clone(),
            };
            let target = BackendTarget {
                provider: entry.upstream.clone(),
                model: entry.upstream_model.clone(),
            };
            routes.insert(key, target);
        }
        Self { routes }
    }
}

impl Router for StaticRouter {
    fn resolve(&self, frontend: FrontendKind, model: &str) -> Result<BackendTarget, RouteError> {
        let key = RouteKey {
            frontend,
            model: model.to_string(),
        };
        self.routes.get(&key).cloned().ok_or_else(|| RouteError::NoRoute {
            frontend,
            model: model.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_config::{GatewayConfig, RouteEntry};

    fn cfg_with_route(frontend: &str, model: &str, upstream: &str, upstream_model: &str) -> GatewayConfig {
        GatewayConfig {
            server: Default::default(),
            logging: Default::default(),
            upstreams: Default::default(),
            routes: vec![RouteEntry {
                frontend: frontend.to_string(),
                model: model.to_string(),
                upstream: upstream.to_string(),
                upstream_model: upstream_model.to_string(),
            }],
            copilot: None,
        }
    }

    #[test]
    fn resolves_known_route() {
        let cfg = cfg_with_route("openai_chat", "gpt-4o", "my-upstream", "gpt-4o-2024-11-20");
        let router = StaticRouter::from_config(&cfg);
        let target = router.resolve(FrontendKind::OpenAiChat, "gpt-4o").unwrap();
        assert_eq!(target.provider, "my-upstream");
        assert_eq!(target.model, "gpt-4o-2024-11-20");
    }

    #[test]
    fn unknown_model_returns_no_route() {
        let cfg = cfg_with_route("openai_chat", "gpt-4o", "my-upstream", "gpt-4o-2024-11-20");
        let router = StaticRouter::from_config(&cfg);
        let err = router.resolve(FrontendKind::OpenAiChat, "unknown-model").unwrap_err();
        assert!(matches!(err, RouteError::NoRoute { .. }));
    }

    #[test]
    fn resolves_anthropic_route() {
        let cfg = cfg_with_route("anthropic_messages", "claude-3-5-sonnet", "upstream-a", "claude-3-5-sonnet-20241022");
        let router = StaticRouter::from_config(&cfg);
        let target = router.resolve(FrontendKind::AnthropicMessages, "claude-3-5-sonnet").unwrap();
        assert_eq!(target.provider, "upstream-a");
    }
}
