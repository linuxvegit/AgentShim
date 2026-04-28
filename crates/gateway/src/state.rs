use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_shim_config::{GatewayConfig, UpstreamConfig};
use agent_shim_frontends::{anthropic_messages::AnthropicMessages, openai_chat::OpenAiChat};
use agent_shim_providers::{
    github_copilot::{self, credential_store},
    openai_compatible::{self},
    ProviderRegistry,
};
use agent_shim_router::StaticRouter;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<GatewayConfig>,
    pub anthropic: Arc<AnthropicMessages>,
    pub openai: Arc<OpenAiChat>,
    pub providers: Arc<ProviderRegistry>,
    pub router: Arc<StaticRouter>,
}

impl AppState {
    pub fn new(config: GatewayConfig) -> Self {
        let keepalive = Duration::from_secs(config.server.keepalive_secs);
        let anthropic = Arc::new(AnthropicMessages { keepalive: Some(keepalive) });
        let openai = Arc::new(OpenAiChat { keepalive: Some(keepalive), clock_override: None });

        let mut registry = ProviderRegistry::new();
        for (name, upstream) in &config.upstreams {
            match upstream {
                UpstreamConfig::OpenAiCompatible(cfg) => {
                    match openai_compatible::from_config(name, cfg) {
                        Ok(p) => registry.register(name.clone(), Arc::new(p)),
                        Err(e) => tracing::error!("failed to build provider {name}: {e}"),
                    }
                }
                UpstreamConfig::GithubCopilot => {
                    let credential_path = config
                        .copilot
                        .as_ref()
                        .map(|c| PathBuf::from(&c.credential_path))
                        .unwrap_or_else(|| {
                            credential_store::default_path()
                                .unwrap_or_else(|_| PathBuf::from("./copilot.json"))
                        });
                    match github_copilot::CopilotProvider::spawn(credential_path) {
                        Ok(p) => registry.register(name.clone(), Arc::new(p)),
                        Err(e) => tracing::error!("failed to build Copilot provider {name}: {e}"),
                    }
                }
            }
        }

        let router = Arc::new(StaticRouter::from_config(&config));

        Self {
            config: Arc::new(config),
            anthropic,
            openai,
            providers: Arc::new(registry),
            router,
        }
    }
}
