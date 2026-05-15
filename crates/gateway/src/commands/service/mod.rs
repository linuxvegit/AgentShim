//! Windows Service subcommand implementations.
//!
//! Gated to Windows targets via `cfg(windows)` at the parent module. The
//! `windows-service` and `windows` crates are pulled in only by the
//! `[target.'cfg(windows)'.dependencies]` block of `crates/gateway/Cargo.toml`,
//! so non-Windows builds never see them.
//!
//! Spec: `docs/superpowers/specs/2026-05-14-windows-service-and-file-logging-design.md`

pub mod elevation;
pub mod install;
pub mod lifecycle;
pub mod log_fallback;
pub mod names;
pub mod run;
pub mod startup_error;
pub mod status;

use crate::cli::ServiceCommand;

/// Top-level dispatch for `agent-shim service <sub>`. Called from `main.rs`.
pub async fn run(sub: ServiceCommand) -> anyhow::Result<()> {
    match sub {
        ServiceCommand::Install {
            config,
            name,
            display_name,
            account,
            password,
            start_type,
        } => install::install(install::InstallArgs {
            config,
            name,
            display_name,
            account,
            password,
            start_type,
        }),
        ServiceCommand::Uninstall { name } => install::uninstall(&name),
        ServiceCommand::Status { name } => status::status(&name),
        ServiceCommand::Run { config } => {
            // service_dispatcher::start blocks the calling thread for the
            // full lifetime of the service (it doesn't return until SCM
            // reports the service stopped). Isolate it on tokio's blocking
            // pool so the runtime stays responsive for the SCM stop signal
            // path (which itself dispatches into a tokio-managed task).
            tokio::task::spawn_blocking(move || run::run(&config))
                .await
                .map_err(|e| anyhow::anyhow!("service run task panicked: {e}"))?
        }
        ServiceCommand::Start { name } => lifecycle::start(&name),
        ServiceCommand::Stop { name } => lifecycle::stop(&name),
        ServiceCommand::Restart { name } => lifecycle::restart(&name),
    }
}
