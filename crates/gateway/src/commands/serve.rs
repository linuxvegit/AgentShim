use anyhow::Result;
use std::path::Path;

pub async fn run(config_path: &Path) -> Result<()> {
    let cfg = agent_shim_config::load_from_path(config_path)
        .map_err(|e| anyhow::anyhow!("Failed to load config: {}", e))?;
    agent_shim_config::validate(&cfg)
        .map_err(|e| anyhow::anyhow!("Config validation failed: {}", e))?;
    let tracing_handles = agent_shim_observability::init(&cfg.logging, cfg.otel.as_ref());
    let (state, reload_rx) = crate::state::AppState::new(cfg).await;
    // TODO(P04 T4): spawn reload-applying task that drains `reload_rx`
    // and performs `state.snapshot.store(Arc::new(new_snap))` for each
    // request. For now the channel exists so handlers compile against
    // `core.reload_tx`, but no consumer is wired yet.
    let _ = reload_rx;
    let result = if state.core.admin_config.is_some() {
        crate::server::run_with_admin(state).await
    } else {
        crate::server::run(state).await
    };
    if let Some(otel) = tracing_handles.otel {
        otel.shutdown();
    }
    result
}
