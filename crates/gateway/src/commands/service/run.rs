//! Windows Service entry point. Invoked by SCM via the command line
//!   "<exe>" service run --config "<path>"
//! Never invoked manually — `--help` hides it.
//!
//! Flow:
//!   1. `service_dispatcher::start` hands control to SCM and calls
//!      `service_main`.
//!   2. `service_main`:
//!      a. Records last-resort diagnostics on failure (see
//!      `startup_error::record`), since SCM-launched processes have
//!      no console.
//!   3. `run_service` (the wrapper):
//!      a. Registers a control handler closure.
//!      b. Reports `StartPending`.
//!      c. Calls `run_service_inner`, which loads config, validates,
//!      applies the service log fallback, boots tokio, and drives
//!      `run_core`.
//!      d. Reports `Stopped(Win32(0|1))` regardless of inner outcome.
//!      This is the bug fix that prevents the service from sticking in
//!      `StartPending` on config / runtime-init failures.
//!   4. `run_core`'s `on_listening` callback reports `Running`.
//!   5. On `Stop`/`Shutdown`, the control handler triggers the watch
//!      channel, which `run_core`'s shutdown future awaits.
//!
//! Spec sections 4.4, 4.5, 4.6.

use crate::commands::service::log_fallback::apply_service_log_fallback;
use crate::commands::service::names::DEFAULT_SERVICE_NAME;
use crate::commands::service::startup_error;
use agent_shim_config::schema::GatewayConfig;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use windows_service::define_windows_service;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::service_dispatcher;

// Macro from windows-service: generates an `extern "system"` shim
// (`ffi_service_main`) that SCM's StartServiceCtrlDispatcherW expects.
// The shim unpacks the C-style argument vector and calls our Rust
// `service_main(Vec<OsString>)` below.
define_windows_service!(ffi_service_main, service_main);

// The dispatcher does not pass user CLI arguments through to service_main
// (SCM calls the binary with a fixed ImagePath that was registered at
// install time). We stash the parsed --config path in a process-wide
// OnceLock so service_main can reach it.
//
// OnceLock (over Mutex<Option<_>>) gives us "set once, read forever"
// semantics without locking and without poisoning risk.
static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Entry point invoked from `commands::service::run` dispatch in `mod.rs`.
/// Stores the config path and hands control to SCM.
pub fn run(config: &std::path::Path) -> anyhow::Result<()> {
    if !config.is_absolute() {
        anyhow::bail!(
            "internal: service run requires an absolute --config path; got {:?}",
            config
        );
    }
    if CONFIG_PATH.set(config.to_path_buf()).is_err() {
        // set() only fails if already set — the only way that happens is
        // if run() is invoked twice in the same process, which SCM never
        // does. Bail with a clear message instead of overwriting.
        anyhow::bail!("internal: service::run::run called twice (CONFIG_PATH already set)");
    }

    // Hand control to SCM. This call blocks until the service exits.
    // It returns an error if invoked outside a service context (e.g.
    // user typed `agent-shim service run --config foo.yaml` in a
    // regular terminal), which is exactly the user-facing message
    // we want.
    // Note: We pass DEFAULT_SERVICE_NAME ("agent-shim") here, but the
    // service may have been installed under a different --name (e.g.
    // "agent-shim-anthropic" for multi-instance setups). This works
    // because ServiceType::OWN_PROCESS services ignore the name
    // parameter on StartServiceCtrlDispatcher and
    // RegisterServiceCtrlHandlerEx — Windows routes by PID, not name.
    // If we ever switch to SHARE_PROCESS, we'd need to thread the
    // install-time --name through to this call site (probably via the
    // ImagePath parser already used by status::status).
    service_dispatcher::start(DEFAULT_SERVICE_NAME, ffi_service_main).map_err(|e| {
        anyhow::anyhow!(
            "service_dispatcher::start failed: {e}.\n\
             Hint: `agent-shim service run` is invoked by SCM, not from a console. \
             Use `agent-shim service start` from an elevated terminal instead."
        )
    })?;
    Ok(())
}

fn service_main(_arguments: Vec<OsString>) {
    // SCM-launched services have no console attached, so eprintln! lands
    // on a dangling stderr handle. We mirror every failure into the
    // last-resort startup-error log (`%PROGRAMDATA%\agent-shim\logs\
    // agent-shim-startup-error.log`) so operators have something to
    // read when sc query shows Stopped(Win32(1)).
    let config = match CONFIG_PATH.get().cloned() {
        Some(p) => p,
        None => {
            // status_handle is not yet registered here — we can't
            // report Stopped to SCM. The dispatcher will eventually
            // time out and SCM will mark the service Stopped on its
            // own. The startup-error log is our only diagnostic.
            startup_error::record(
                "FATAL: service_main invoked without config path set \
                 (this is an internal bug — please file an issue)",
            );
            eprintln!("FATAL: service_main invoked without config path set");
            return;
        }
    };

    if let Err(e) = run_service(config) {
        // run_service has already reported Stopped(Win32(1)) to SCM —
        // sc query will show "stopped, exit code 1". Use anyhow's
        // alternate format ({e:#}) to capture the full cause chain
        // (config-parse error wraps the underlying serde/figment
        // detail, for example).
        startup_error::record(&format!("service_main exited with error: {e:#}"));
        eprintln!("service_main exited with error: {e}");
    }
}

/// Load + validate + inject service log fallback, in that order.
///
/// Public-but-pub(crate)-equivalent so the integration test
/// `service_load_validate_unit.rs` can exercise it.
///
/// Failure modes:
///   * file does not exist → `ConfigError::NotFound`
///   * YAML parse failure → `ConfigError::Parse`
///   * `validate()` rejection → `ValidationError`
///
/// Each becomes an `anyhow::Error` whose Display includes the underlying
/// cause, so the startup-error log line is informative.
pub fn load_validate_inject_fallback(config_path: &Path) -> anyhow::Result<GatewayConfig> {
    let mut cfg = agent_shim_config::load_from_path(config_path)?;
    agent_shim_config::validate(&cfg)?;
    apply_service_log_fallback(&mut cfg);
    Ok(cfg)
}

/// Wrapper that always reports `Stopped(Win32(0|1))` to SCM.
///
/// The bug this fixes: previously, every `?` inside `run_service` past
/// the `StartPending` report (config load, validate, tokio build,
/// `run_core`'s 6 internal `?` paths) would return Err to
/// `service_main`, which would `eprintln!` (invisible under SCM) and
/// return. SCM never saw a Stopped report, so the service stuck in
/// "Starting" until SCM's own 30s timeout — and on some Windows
/// versions, required a manual `sc stop` to recover.
///
/// Now: the wrapper registers + reports StartPending, runs `inner`,
/// and unconditionally reports Stopped before returning.
fn run_service(config_path: PathBuf) -> anyhow::Result<()> {
    // Watch channel: sender side lives in the control handler, receiver
    // side becomes run_core's shutdown signal.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let stop_tx = Arc::new(stop_tx);

    // SCM event handler. Runs on a thread SCM owns — keep it short and
    // non-blocking. We do not call SetServiceStatus here; status updates
    // come from the main flow below.
    let handler_stop_tx = stop_tx.clone();
    let event_handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = handler_stop_tx.send(true);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    // See the OWN_PROCESS note in `run()` above for why hardcoding
    // DEFAULT_SERVICE_NAME here is correct.
    //
    // If `register` itself fails (extremely rare — SCM handle setup),
    // we cannot report Stopped because we have no handle. The
    // dispatcher will surface the error to service_main, which writes
    // it to the startup-error log.
    let status_handle = service_control_handler::register(DEFAULT_SERVICE_NAME, event_handler)
        .map_err(|e| anyhow::anyhow!("register control handler: {e}"))?;

    // Report StartPending. SCM gives us 30s before timeout; setting
    // wait_hint to 30s tells SCM "keep waiting".
    //
    // Best-effort: if the SCM round-trip fails here, we can't recover —
    // we have no console to report to, and bailing out would leave SCM
    // thinking the service is still StartPending. Push on and let the
    // final Stopped(exit_code) report (below) be the authoritative
    // outcome.
    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::StartPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(30),
        process_id: None,
    });

    // Run the inner pipeline. ServiceStatusHandle is Copy (a thin
    // Win32 handle wrapper), so we hand a copy to inner for the
    // `on_listening` callback while keeping the original for the
    // post-inner Stopped report. See "OWN_PROCESS note" above.
    let result = run_service_inner(config_path, status_handle, stop_rx);

    // Always report Stopped, regardless of how `inner` resolved.
    // Win32(0) for clean exit; Win32(1) for any error.
    let exit_code = if result.is_ok() {
        ServiceExitCode::Win32(0)
    } else {
        ServiceExitCode::Win32(1)
    };
    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code,
        checkpoint: 0,
        wait_hint: Duration::from_secs(0),
        process_id: None,
    });

    result
}

/// Inner pipeline: load + validate + fallback + tokio + run_core.
///
/// Every error path here flows through to `run_service`'s post-call
/// Stopped report. No `?` inside this function can leak past the
/// wrapper without SCM being told.
fn run_service_inner(
    config_path: PathBuf,
    status_handle: ServiceStatusHandle,
    stop_rx: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    // Ordering invariant: load → validate → apply_service_log_fallback
    // → run_core. The fallback MUST run before run_core because:
    //   (1) run_core calls agent_shim_observability::init, which reads
    //       cfg.logging.file to decide whether to build a file layer
    //       (Phase 2), and
    //   (2) under SCM there is no console — without the fallback, an
    //       operator who installs without a `logging.file` block in
    //       their gateway.yaml gets no logs at all.
    //
    // `load_validate_inject_fallback` is the test-pinned helper for
    // this ordering; see service_load_validate_unit.rs.
    let cfg = load_validate_inject_fallback(&config_path)?;

    // Build a multi-thread tokio runtime. A current-thread runtime would
    // also work, but multi-thread matches the foreground `serve` path so
    // observable behavior under load is identical.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("building tokio runtime: {e}"))?;

    // Shutdown future for run_core.
    let mut shutdown_rx = stop_rx.clone();
    let shutdown_fut = async move {
        // Receiver starts with value=false; wait until it flips.
        // `changed().await` returns Err if the sender is dropped, which
        // we treat as "shut down" too.
        while shutdown_rx.changed().await.is_ok() {
            if *shutdown_rx.borrow() {
                break;
            }
        }
    };

    // `ServiceStatusHandle` is `Copy` (thin wrapper around a Win32 handle),
    // so we can hand a copy to the `on_listening` callback freely.
    let on_listening = move |_addr: std::net::SocketAddr| {
        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(0),
            process_id: None,
        });
    };

    let serve_result = rt.block_on(async {
        crate::commands::serve::run_core(cfg, Some(config_path.clone()), shutdown_fut, on_listening)
            .await
    });

    serve_result.map_err(|e| anyhow::anyhow!("run_core: {e}"))
}
