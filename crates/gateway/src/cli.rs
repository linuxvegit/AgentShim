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
    /// Windows Service management (install/uninstall/start/stop/status)
    #[cfg(windows)]
    Service {
        #[command(subcommand)]
        sub: ServiceCommand,
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

#[cfg(windows)]
#[derive(Subcommand, Debug)]
pub enum ServiceCommand {
    /// Register agent-shim as a Windows Service.
    Install {
        /// Absolute path to the config file. Validated before SCM registration.
        #[arg(short, long)]
        config: PathBuf,
        /// Service name as seen by `sc.exe`. Default: `agent-shim`.
        #[arg(long, default_value = "agent-shim")]
        name: String,
        /// Display name shown in the Services MMC console.
        #[arg(long, default_value = "AgentShim Gateway")]
        display_name: String,
        /// Service account. One of: LocalSystem, NetworkService, LocalService,
        /// or DOMAIN\\user. Default: LocalSystem.
        #[arg(long, default_value = "LocalSystem")]
        account: String,
        /// Password for the account when `--account` is a domain user.
        /// Ignored for built-in accounts.
        #[arg(long)]
        password: Option<String>,
        /// Start type: `auto`, `manual`, or `disabled`. Default: `auto`.
        #[arg(long, default_value = "auto")]
        start_type: String,
    },
    /// Remove an agent-shim service registration.
    Uninstall {
        #[arg(long, default_value = "agent-shim")]
        name: String,
    },
    /// Query SCM status, PID, and configured paths.
    Status {
        #[arg(long, default_value = "agent-shim")]
        name: String,
    },
    /// SCM entry point. Hidden — never run manually. Phase 4.
    #[command(hide = true)]
    Run {
        #[arg(short, long)]
        config: PathBuf,
    },
    /// Start the registered service. Phase 4.
    Start {
        #[arg(long, default_value = "agent-shim")]
        name: String,
    },
    /// Stop the registered service. Phase 4.
    Stop {
        #[arg(long, default_value = "agent-shim")]
        name: String,
    },
    /// Restart (stop + start) the registered service. Phase 4.
    Restart {
        #[arg(long, default_value = "agent-shim")]
        name: String,
    },
}
