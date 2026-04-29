use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "agent-shim", about = "AgentShim API gateway")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start the gateway server
    Serve {
        /// Path to the config file
        #[arg(
            short,
            long,
            env = "AGENT_SHIM_CONFIG",
            default_value = "config/gateway.yaml"
        )]
        config: PathBuf,
    },
    /// Validate a config file and exit
    ValidateConfig {
        /// Path to the config file
        #[arg(short, long, env = "AGENT_SHIM_CONFIG")]
        config: PathBuf,
    },
    /// GitHub Copilot authentication commands
    Copilot {
        #[command(subcommand)]
        sub: CopilotCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum CopilotCommand {
    /// Authenticate with GitHub Copilot via device flow
    Login {
        /// Path to store credentials (default: platform config dir)
        #[arg(long)]
        credential_path: Option<PathBuf>,
    },
    /// List available models from Copilot
    Models {
        /// Path to credentials file
        #[arg(long)]
        credential_path: Option<PathBuf>,
    },
}
