use std::path::PathBuf;

use agent_shim_providers::github_copilot::{auth, credential_store};

pub async fn run(credential_path: Option<PathBuf>) -> anyhow::Result<()> {
    let path = match credential_path {
        Some(p) => p,
        None => credential_store::default_path()
            .map_err(|e| anyhow::anyhow!("could not determine credential path: {e}"))?,
    };

    println!("Logging in to GitHub Copilot…");

    match auth::login_device_flow(path.clone()).await? {
        auth::DeviceFlowOutcome::Success { path } => {
            println!("Successfully authenticated. Credentials saved to: {}", path.display());
        }
        auth::DeviceFlowOutcome::Declined => {
            println!("Authorization was declined.");
        }
    }

    Ok(())
}
