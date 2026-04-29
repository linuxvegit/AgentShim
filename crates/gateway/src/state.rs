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
use agent_shim_router::model_index::ModelIndex;
use agent_shim_router::StaticRouter;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<GatewayConfig>,
    pub anthropic: Arc<AnthropicMessages>,
    pub openai: Arc<OpenAiChat>,
    pub providers: Arc<ProviderRegistry>,
    pub router: Arc<StaticRouter>,
    pub model_index: Arc<ModelIndex>,
}

impl AppState {
    pub async fn new(config: GatewayConfig) -> Self {
        let keepalive = Duration::from_secs(config.server.keepalive_secs);
        let anthropic = Arc::new(AnthropicMessages {
            keepalive: Some(keepalive),
        });
        let openai = Arc::new(OpenAiChat {
            keepalive: Some(keepalive),
            clock_override: None,
        });

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
                        .map(|c| expand_tilde(&c.credential_path))
                        .unwrap_or_else(|| {
                            credential_store::default_path()
                                .unwrap_or_else(|_| PathBuf::from("./copilot.json"))
                        });
                    tracing::info!(path = %credential_path.display(), "copilot credential path");
                    match github_copilot::CopilotProvider::spawn(credential_path) {
                        Ok(p) => registry.register(name.clone(), Arc::new(p)),
                        Err(e) => tracing::error!("failed to build Copilot provider {name}: {e}"),
                    }
                }
            }
        }

        let router = Arc::new(StaticRouter::from_config(&config));

        let mut discovered = std::collections::HashMap::new();
        for (name, provider) in registry.iter() {
            match provider.list_models().await {
                Ok(Some(models)) => {
                    tracing::info!(provider = %name, count = models.len(), "discovered models");
                    discovered.insert(name.clone(), models);
                }
                Ok(None) => {
                    tracing::debug!(provider = %name, "provider does not support model discovery");
                }
                Err(e) => {
                    tracing::warn!(provider = %name, error = %e, "model discovery failed, skipping");
                }
            }
        }
        let model_index = Arc::new(ModelIndex::new(discovered));

        Self {
            config: Arc::new(config),
            anthropic,
            openai,
            providers: Arc::new(registry),
            router,
            model_index,
        }
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}
