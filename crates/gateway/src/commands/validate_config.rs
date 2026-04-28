use std::path::Path;
use anyhow::Result;

pub fn run(config_path: &Path) -> Result<()> {
    let cfg = agent_shim_config::load_from_path(config_path)
        .map_err(|e| anyhow::anyhow!("Failed to load config: {}", e))?;
    agent_shim_config::validate(&cfg)
        .map_err(|e| anyhow::anyhow!("Config validation failed: {}", e))?;
    println!(
        "OK: {} routes, {} upstreams",
        cfg.routes.len(),
        cfg.upstreams.len()
    );
    Ok(())
}
