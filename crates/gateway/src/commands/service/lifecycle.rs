//! Client-side service control: start, stop, restart. These commands
//! talk to SCM from a regular CLI invocation; the running service is
//! a separate process spawned by SCM via `service run`.
//!
//! Spec sections 4.5, 4.6.

use crate::commands::service::elevation::require_admin;
use anyhow::{Context, Result};
use std::time::{Duration, Instant};
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const TIMEOUT: Duration = Duration::from_secs(30);

pub fn start(name: &str) -> Result<()> {
    require_admin()?;

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("opening SCM")?;
    let service = manager
        .open_service(name, ServiceAccess::START | ServiceAccess::QUERY_STATUS)
        .with_context(|| format!("opening service {name:?}"))?;

    service
        .start(&[] as &[&std::ffi::OsStr])
        .context("StartService")?;
    wait_for_state(&service, ServiceState::Running, "Running")?;
    println!("Service {name:?} is now running.");
    Ok(())
}

pub fn stop(name: &str) -> Result<()> {
    require_admin()?;

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("opening SCM")?;
    let service = manager
        .open_service(name, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)
        .with_context(|| format!("opening service {name:?}"))?;

    let status = service.query_status().context("query_status before stop")?;
    if status.current_state == ServiceState::Stopped {
        println!("Service {name:?} is already stopped.");
        return Ok(());
    }

    service.stop().context("ControlService(STOP)")?;
    wait_for_state(&service, ServiceState::Stopped, "Stopped")?;
    println!("Service {name:?} stopped.");
    Ok(())
}

pub fn restart(name: &str) -> Result<()> {
    require_admin()?;
    if let Err(e) = stop(name) {
        // A failed stop usually means "already stopped" — proceed with
        // start. Surface the error so operators see WHY stop failed if
        // start subsequently also fails.
        eprintln!("(stop step skipped: {e})");
    }
    start(name)
}

fn wait_for_state(
    service: &windows_service::service::Service,
    target: ServiceState,
    target_name: &str,
) -> Result<()> {
    let start = Instant::now();
    loop {
        let status = service
            .query_status()
            .context("query_status while polling")?;
        if status.current_state == target {
            return Ok(());
        }
        if start.elapsed() >= TIMEOUT {
            anyhow::bail!(
                "timed out after {:?} waiting for service to reach {target_name:?}; \
                 last state: {:?}",
                TIMEOUT,
                status.current_state,
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}
