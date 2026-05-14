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
pub mod log_fallback;
pub mod names;
pub mod status;

// run/start/stop/restart land in Phase 4. Stub the module so the dispatch
// table compiles today.
pub mod run_stub;

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
        ServiceCommand::Run { config } => run_stub::run(&config),
        ServiceCommand::Start { name } => {
            let _ = name;
            anyhow::bail!("`agent-shim service start` lands in Phase 4")
        }
        ServiceCommand::Stop { name } => {
            let _ = name;
            anyhow::bail!("`agent-shim service stop` lands in Phase 4")
        }
        ServiceCommand::Restart { name } => {
            let _ = name;
            anyhow::bail!("`agent-shim service restart` lands in Phase 4")
        }
    }
}
