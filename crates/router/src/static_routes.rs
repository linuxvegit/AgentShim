use std::collections::HashMap;

use agent_shim_config::GatewayConfig;
use agent_shim_core::{request::ReasoningEffort, BackendTarget, FrontendKind, RoutePolicy};

use crate::{RouteError, Router};

/// Key for the route table.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RouteKey {
    frontend: FrontendKind,
    model: String,
}

/// A wildcard route entry: `model: "*"` matches any model not handled by a
/// specific route. The `upstream_model` field controls what's sent upstream:
/// - `"*"` means pass the original model name through
/// - anything else is used as a literal override
struct WildcardTarget {
    provider: String,
    upstream_model: String,
    policy: RoutePolicy,
}

/// A static router built from `GatewayConfig.routes`.
pub struct StaticRouter {
    routes: HashMap<RouteKey, BackendTarget>,
    wildcards: HashMap<FrontendKind, WildcardTarget>,
}

impl StaticRouter {
    pub fn from_config(cfg: &GatewayConfig) -> Self {
        let mut routes = HashMap::new();
        let mut wildcards = HashMap::new();
        for entry in &cfg.routes {
            let frontend = match entry.frontend.as_str() {
                "anthropic_messages" | "anthropic" => FrontendKind::AnthropicMessages,
                "openai_chat" | "openai" => FrontendKind::OpenAiChat,
                "openai_responses" | "responses" => FrontendKind::OpenAiResponses,
                other => {
                    tracing::warn!("unknown frontend kind in route config: {other}");
                    continue;
                }
            };
            let default_reasoning_effort = entry
                .reasoning_effort
                .as_deref()
                .and_then(ReasoningEffort::parse);
            if entry.reasoning_effort.is_some() && default_reasoning_effort.is_none() {
                tracing::warn!(
                    value = ?entry.reasoning_effort,
                    "ignoring unknown reasoning_effort in route config (expected minimal/low/medium/high/xhigh)"
                );
            }
            let policy = RoutePolicy {
                default_reasoning_effort,
                default_anthropic_beta: entry.anthropic_beta.clone(),
            };
            if entry.model == "*" {
                wildcards.insert(
                    frontend,
                    WildcardTarget {
                        provider: entry.upstream.clone(),
                        upstream_model: entry.upstream_model.clone(),
                        policy,
                    },
                );
                continue;
            }
            let key = RouteKey {
                frontend,
                model: entry.model.clone(),
            };
            let target = BackendTarget {
                provider: entry.upstream.clone(),
                model: entry.upstream_model.clone(),
                policy,
            };
            routes.insert(key, target);
        }
        Self { routes, wildcards }
    }
}

impl Router for StaticRouter {
    fn resolve(&self, frontend: FrontendKind, model: &str) -> Result<BackendTarget, RouteError> {
        let key = RouteKey {
            frontend,
            model: model.to_string(),
        };
        if let Some(target) = self.routes.get(&key) {
            return Ok(target.clone());
        }
        if let Some(wc) = self.wildcards.get(&frontend) {
            let upstream_model = if wc.upstream_model == "*" {
                model.to_string()
            } else {
                wc.upstream_model.clone()
            };
            return Ok(BackendTarget {
                provider: wc.provider.clone(),
                model: upstream_model,
                policy: wc.policy.clone(),
            });
        }
        Err(RouteError::NoRoute {
            frontend,
            model: model.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_config::{GatewayConfig, RouteEntry};

    fn cfg_with_route(
        frontend: &str,
        model: &str,
        upstream: &str,
        upstream_model: &str,
    ) -> GatewayConfig {
        GatewayConfig {
            server: Default::default(),
            logging: Default::default(),
            upstreams: Default::default(),
            routes: vec![RouteEntry {
                frontend: frontend.to_string(),
                model: model.to_string(),
                upstream: upstream.to_string(),
                upstream_model: upstream_model.to_string(),
                reasoning_effort: None,
                anthropic_beta: None,
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
        let err = router
            .resolve(FrontendKind::OpenAiChat, "unknown-model")
            .unwrap_err();
        assert!(matches!(err, RouteError::NoRoute { .. }));
    }

    #[test]
    fn resolves_anthropic_route() {
        let cfg = cfg_with_route(
            "anthropic_messages",
            "claude-3-5-sonnet",
            "upstream-a",
            "claude-3-5-sonnet-20241022",
        );
        let router = StaticRouter::from_config(&cfg);
        let target = router
            .resolve(FrontendKind::AnthropicMessages, "claude-3-5-sonnet")
            .unwrap();
        assert_eq!(target.provider, "upstream-a");
    }

    #[test]
    fn wildcard_route_passes_model_through() {
        let mut cfg = cfg_with_route("anthropic_messages", "*", "copilot", "*");
        cfg.routes.push(RouteEntry {
            frontend: "anthropic_messages".to_string(),
            model: "override".to_string(),
            upstream: "other".to_string(),
            upstream_model: "other-model".to_string(),
            reasoning_effort: None,
            anthropic_beta: None,
        });
        let router = StaticRouter::from_config(&cfg);
        // Specific route wins
        let t = router
            .resolve(FrontendKind::AnthropicMessages, "override")
            .unwrap();
        assert_eq!(t.provider, "other");
        // Wildcard catches anything else, passes model name through
        let t = router
            .resolve(FrontendKind::AnthropicMessages, "claude-opus-4-7")
            .unwrap();
        assert_eq!(t.provider, "copilot");
        assert_eq!(t.model, "claude-opus-4-7");
    }
}
