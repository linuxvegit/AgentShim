use anyhow::Result;
use std::path::Path;

pub async fn run(config_path: &Path) -> Result<()> {
    let cfg = agent_shim_config::load_from_path(config_path)
        .map_err(|e| anyhow::anyhow!("Failed to load config: {}", e))?;
    agent_shim_config::validate(&cfg)
        .map_err(|e| anyhow::anyhow!("Config validation failed: {}", e))?;
    agent_shim_observability::init(&cfg.logging);
    let state = crate::state::AppState::new(cfg).await;
    crate::server::run(state).await
}
