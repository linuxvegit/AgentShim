mod cli;
mod commands;
mod handlers;
mod pipeline;
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
        Commands::Copilot { sub } => match sub {
            CopilotCommand::Login { credential_path } => {
                commands::copilot_login::run(credential_path).await
            }
            CopilotCommand::Models { credential_path } => {
                commands::copilot_models::run(credential_path).await
            }
        },
    }
}
