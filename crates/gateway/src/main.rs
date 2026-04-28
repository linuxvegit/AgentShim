mod cli;
mod commands;
mod handlers;
mod server;
mod shutdown;
mod state;

use clap::Parser;
use cli::{Cli, Commands};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve { config } => commands::serve::run(&config).await,
        Commands::ValidateConfig { config } => commands::validate_config::run(&config),
    }
}
