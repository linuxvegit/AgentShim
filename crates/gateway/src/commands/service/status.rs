//! `agent-shim service status <name>` — query SCM without requiring admin.

use crate::commands::service::names::parse_config_from_image_path;
use anyhow::{Context, Result};
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

pub fn status(name: &str) -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("opening SCM (CONNECT)")?;

    let service = match manager.open_service(
        name,
        ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
    ) {
        Ok(s) => s,
        Err(e) => {
            // Service does not exist → print friendly message and exit 1.
            println!("Service {name:?} is not installed ({e}).");
            std::process::exit(1);
        }
    };

    let cfg = service.query_config().context("querying service config")?;
    let st = service.query_status().context("querying service status")?;

    let state_str = match st.current_state {
        ServiceState::Stopped => "Stopped",
        ServiceState::StartPending => "StartPending",
        ServiceState::StopPending => "StopPending",
        ServiceState::Running => "Running",
        ServiceState::ContinuePending => "ContinuePending",
        ServiceState::PausePending => "PausePending",
        ServiceState::Paused => "Paused",
    };

    let pid = st
        .process_id
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".into());

    let image_path_str = cfg.executable_path.to_string_lossy().into_owned();
    let parsed_config = parse_config_from_image_path(&image_path_str)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(not found in ImagePath)".to_string());

    let start_type = format!("{:?}", cfg.start_type);
    let account = cfg
        .account_name
        .as_ref()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "LocalSystem".to_string());

    println!("Service:     {name}");
    println!("Display:     {}", cfg.display_name.to_string_lossy());
    println!("State:       {state_str}");
    println!("PID:         {pid}");
    println!("Start Type:  {start_type}");
    println!("Account:     {account}");
    println!("ImagePath:   {image_path_str}");
    println!("Config:      {parsed_config}");

    Ok(())
}
