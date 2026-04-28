use std::path::PathBuf;

use agent_shim_providers::github_copilot::{credential_store, models, token_manager};

pub async fn run(credential_path: Option<PathBuf>) -> anyhow::Result<()> {
    let path = match credential_path {
        Some(p) => p,
        None => credential_store::default_path()
            .map_err(|e| anyhow::anyhow!("could not determine credential path: {e}"))?,
    };

    let creds = credential_store::load(&path)
        .map_err(|e| anyhow::anyhow!("failed to load credentials from {}: {e} — run `agent-shim copilot login` first", path.display()))?;

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let token = token_manager::exchange_with_base(&http, &creds, "https://api.github.com").await
        .map_err(|e| anyhow::anyhow!("token exchange failed: {e}"))?;

    println!("Copilot API base: {}", token.api_base);
    println!("Available models:");

    let model_ids = models::list_models(&http, &token).await
        .map_err(|e| anyhow::anyhow!("failed to list models: {e}"))?;

    for id in &model_ids {
        println!("  {}", id);
    }
    println!("\n{} models available", model_ids.len());

    Ok(())
}
