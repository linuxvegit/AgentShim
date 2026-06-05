mod admin;
mod cli;
mod commands;
mod handlers;
mod image_estimator_selector;
mod latency_probe;
mod metrics_layer;
mod pipeline;
mod reload_trigger;
mod server;
mod shutdown;
mod state;

use clap::Parser;
use cli::{Cli, Commands, CopilotCommand};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve { config } => commands::serve::run(&config).await,
        Commands::ValidateConfig { config } => commands::validate_config::run(&config),
        Commands::ShowCatalog {
            config,
            format,
            strict,
        } => commands::show_catalog::run(&config, &format, strict).await,
        Commands::Models {
            name,
            config,
            format,
        } => commands::models::run(&name, &config, &format).await,
        Commands::Copilot { sub } => match sub {
            CopilotCommand::Login { credential_path } => {
                commands::copilot_login::run(credential_path).await
            }
            CopilotCommand::Models {
                credential_path,
                format,
            } => commands::copilot_models::run(credential_path, &format).await,
        },
        #[cfg(windows)]
        Commands::Service { sub } => commands::service::run(sub).await,
    }
}
