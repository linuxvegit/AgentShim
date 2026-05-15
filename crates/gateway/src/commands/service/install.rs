//! `agent-shim service install` and `agent-shim service uninstall`.
//!
//! Install workflow:
//!   1. require_admin()
//!   2. Validate the config path is absolute.
//!   3. Load and validate the config (reuses `agent_shim_config::validate`).
//!   4. Resolve the current exe via `std::env::current_exe()`.
//!   5. Construct ServiceInfo and call ServiceManager::create_service.
//!   6. Set the service description.
//!
//! Uninstall workflow:
//!   1. require_admin()
//!   2. Open the service, stop it if running, then call delete.

use crate::commands::service::elevation::require_admin;
use crate::commands::service::names::launch_arguments;
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::PathBuf;
use windows_service::service::{
    ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

/// Arguments forwarded from the CLI to the install command. Keeping a
/// struct (rather than a long positional argument list) keeps the
/// dispatch in `commands/service/mod.rs` readable.
pub struct InstallArgs {
    pub config: PathBuf,
    pub name: String,
    pub display_name: String,
    pub account: String,
    pub password: Option<String>,
    pub start_type: String,
}

pub fn install(args: InstallArgs) -> Result<()> {
    require_admin()?;

    // 1. Reject relative paths up-front. SCM-launched processes have cwd
    //    = C:\\Windows\\System32, so a relative path becomes a different
    //    file at service runtime than at install time.
    if args.config.is_relative() {
        anyhow::bail!(
            "--config must be an absolute path; got {:?}\n\
             Hint: the Windows Service Control Manager runs the service \
             with cwd = C:\\Windows\\System32.",
            args.config
        );
    }

    // 2. Pre-validate the config file. Same checks as `agent-shim validate-config`.
    let cfg = agent_shim_config::load_from_path(&args.config)
        .with_context(|| format!("loading config at {:?}", args.config))?;
    agent_shim_config::validate(&cfg).context("validating config")?;

    // 3. Find the current exe.
    let exe = std::env::current_exe().context("resolving current exe path")?;
    if let Some(dir) = exe.parent() {
        if dir.ends_with("debug")
            || dir.ends_with(std::env::temp_dir().file_name().unwrap_or_default())
        {
            eprintln!(
                "WARNING: agent-shim binary appears to be under a debug or temp directory ({:?}). \
                 For a production service, install the release binary to a stable location.",
                dir
            );
        }
    }

    // 4. Translate CLI start-type into the windows-service enum.
    let start_type = match args.start_type.to_lowercase().as_str() {
        "auto" | "automatic" => ServiceStartType::AutoStart,
        "manual" | "demand" | "ondemand" => ServiceStartType::OnDemand,
        "disabled" => ServiceStartType::Disabled,
        other => {
            anyhow::bail!("invalid --start-type {other:?}; expected auto, manual, or disabled")
        }
    };

    // 5. Translate account into (account_name, account_password).
    //    LocalSystem / LocalService / NetworkService use the standard
    //    SCM "NT AUTHORITY\\<name>" form (or None for LocalSystem).
    let (account_name, account_password): (Option<OsString>, Option<OsString>) =
        match args.account.as_str() {
            "LocalSystem" => (None, None),
            "NetworkService" => (Some(OsString::from(r"NT AUTHORITY\NetworkService")), None),
            "LocalService" => (Some(OsString::from(r"NT AUTHORITY\LocalService")), None),
            other => {
                let pwd = args.password.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "--password is required when --account is a domain user (got {other:?})"
                    )
                })?;
                (Some(OsString::from(other)), Some(OsString::from(pwd)))
            }
        };

    // 6. Talk to SCM.
    let manager_access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    let manager =
        ServiceManager::local_computer(None::<&str>, manager_access).context("opening SCM")?;

    let info = ServiceInfo {
        name: OsString::from(&args.name),
        display_name: OsString::from(&args.display_name),
        service_type: ServiceType::OWN_PROCESS,
        start_type,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments: launch_arguments(&args.config),
        dependencies: vec![],
        account_name,
        account_password,
    };

    let service = manager
        .create_service(&info, ServiceAccess::CHANGE_CONFIG)
        .with_context(|| format!("creating service {:?}", args.name))?;
    service
        .set_description("Protocol-translating API gateway for AI agents")
        .context("setting service description")?;

    println!(
        "Installed service {:?} ({}).\nImagePath: \"{}\" service run --config \"{}\"",
        args.name,
        args.display_name,
        exe.display(),
        args.config.display(),
    );
    Ok(())
}

pub fn uninstall(name: &str) -> Result<()> {
    require_admin()?;

    let manager_access = ServiceManagerAccess::CONNECT;
    let manager =
        ServiceManager::local_computer(None::<&str>, manager_access).context("opening SCM")?;

    let service = manager
        .open_service(
            name,
            ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
        )
        .with_context(|| format!("opening service {:?}", name))?;

    // Best-effort stop before delete. Ignore errors — the service may
    // already be stopped, or stop may be racing with delete.
    let _ = service.stop();

    service
        .delete()
        .with_context(|| format!("deleting service {:?}", name))?;
    println!("Uninstalled service {:?}.", name);
    Ok(())
}
