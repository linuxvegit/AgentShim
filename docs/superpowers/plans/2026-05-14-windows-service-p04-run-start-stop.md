# Phase 4: Windows Service run / start / stop / restart Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete the Windows Service lifecycle. Implement the hidden `agent-shim service run` SCM entry point (the command SCM actually invokes), and the user-facing `start`, `stop`, `restart` commands. After this phase, an administrator can register, start, query, stop, and remove the service through agent-shim's own CLI — and the SCM state accurately reflects "the gateway is bound and serving traffic".

**Architecture:** `service run` registers a SCM control handler and calls `service_dispatcher::start`, which hands the process to SCM. The dispatcher invokes `service_main`, which transitions through `StartPending → Running → StopPending → Stopped`. We share `commands::serve::run_core` (from Phase 1) for the actual server lifecycle, passing it a `watch`-channel-based shutdown future and a callback that fires `SetServiceStatus(Running)` once the TCP listener is bound. Service-mode log fallback (Phase 2) injects a default `logging.file` if the loaded config has none, so operators always have a place to read logs.

**Tech Stack:** Rust, `windows-service` (already added in Phase 3), tokio.

**Spec reference:** sections 4.4, 4.5, 4.6, 5.4, 11 (phase 4).

**Depends on:** Phase 1 (`run_core` signature and behavior), Phase 2 (`logging.file` + `FileLoggingConfig`), Phase 3 (service module scaffolding, CLI variants, elevation check, names helpers).

---

## File Structure

| File | Responsibility | Status |
|------|----------------|--------|
| `crates/gateway/src/commands/service/run.rs` | SCM entry: dispatcher, control handler, status transitions, hosts the tokio runtime that drives `run_core` | Create (replaces `run_stub.rs`) |
| `crates/gateway/src/commands/service/run_stub.rs` | Delete | Delete |
| `crates/gateway/src/commands/service/mod.rs` | Replace `pub mod run_stub;` with `pub mod run;`; route `start/stop/restart` to `lifecycle::` | Modify |
| `crates/gateway/src/commands/service/lifecycle.rs` | `start`, `stop`, `restart` client commands (talk to SCM, don't host server) | Create |
| `crates/gateway/src/commands/service/log_fallback.rs` | Default service log path (`%PROGRAMDATA%\agent-shim\logs\agent-shim.log`), injection helper | Create |
| `crates/gateway/tests/service_lifecycle.rs` | `#[ignore]` end-to-end install → start → healthz → stop → uninstall | Create |
| `crates/gateway/tests/service_log_fallback_unit.rs` | Unit tests for log-fallback resolution | Create |

---

### Task 1: Failing test — service-mode log fallback resolution

**Files:**
- Create: `crates/gateway/tests/service_log_fallback_unit.rs`

We TDD the service-mode `logging.file` fallback before implementing the module. This isolates the "what should the default log path be?" decision from "how does SCM dispatch work?".

- [ ] **Step 1: Write the failing test**

```rust
// crates/gateway/tests/service_log_fallback_unit.rs

#![cfg(windows)]

use agent_shim_config::schema::{FileLoggingConfig, GatewayConfig, LogFormat, RotationPolicy};
use agent_shim_gateway::commands::service::log_fallback::{
    apply_service_log_fallback, default_service_log_path,
};

fn empty_config() -> GatewayConfig {
    use agent_shim_config::schema::{LoggingConfig, ServerConfig};
    use std::collections::BTreeMap;
    GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams: BTreeMap::new(),
        routes: vec![],
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
    }
}

#[test]
fn default_log_path_uses_programdata() {
    let path = default_service_log_path();
    let s = path.to_string_lossy();
    // PROGRAMDATA is set in any sane Windows env. Even on a CI runner.
    assert!(
        s.contains("agent-shim") && s.ends_with("agent-shim.log"),
        "expected agent-shim/logs/agent-shim.log under PROGRAMDATA; got {s:?}"
    );
}

#[test]
fn apply_fallback_injects_when_file_is_none() {
    let mut cfg = empty_config();
    assert!(cfg.logging.file.is_none());
    apply_service_log_fallback(&mut cfg);
    let file = cfg.logging.file.expect("fallback must populate logging.file");
    assert_eq!(file.format, LogFormat::Json);
    assert_eq!(file.rotation, RotationPolicy::Daily);
    assert_eq!(file.max_files, 7);
    assert!(file.path.is_absolute(), "fallback path must be absolute");
}

#[test]
fn apply_fallback_preserves_user_provided_file_config() {
    let mut cfg = empty_config();
    let user_path = std::path::PathBuf::from(r"C:\custom\logs\app.log");
    cfg.logging.file = Some(FileLoggingConfig {
        path: user_path.clone(),
        format: LogFormat::Pretty,
        rotation: RotationPolicy::Hourly,
        max_files: 24,
    });
    apply_service_log_fallback(&mut cfg);
    let file = cfg.logging.file.unwrap();
    assert_eq!(file.path, user_path);
    assert_eq!(file.format, LogFormat::Pretty);
    assert_eq!(file.rotation, RotationPolicy::Hourly);
    assert_eq!(file.max_files, 24);
}
```

- [ ] **Step 2: Run the failing test**

```bash
cargo nextest run -p agent-shim --test service_log_fallback_unit
```

Expected on Windows: COMPILE ERROR — `log_fallback` module doesn't exist yet. On Linux the file is `#![cfg(windows)]`-gated so cargo will skip it.

- [ ] **Step 3: Commit the failing test**

```bash
git add crates/gateway/tests/service_log_fallback_unit.rs
git commit -m "test(gateway): failing tests for service log fallback (TDD red)"
```

---

### Task 2: Implement `log_fallback` module (TDD green)

**Files:**
- Create: `crates/gateway/src/commands/service/log_fallback.rs`
- Modify: `crates/gateway/src/commands/service/mod.rs`

- [ ] **Step 1: Create the module**

```rust
// crates/gateway/src/commands/service/log_fallback.rs
//
// Service-mode logging fallback. The SCM-spawned process has no console,
// so without a file sink the operator has nowhere to read logs. We inject
// a sane default — `%PROGRAMDATA%\\agent-shim\\logs\\agent-shim.log`,
// daily rotation, 7-file retention, JSON format — only when the loaded
// config did not specify `logging.file`. User-provided settings always win.

use agent_shim_config::schema::{
    FileLoggingConfig, GatewayConfig, LogFormat, RotationPolicy,
};
use std::path::PathBuf;

/// Resolve `%PROGRAMDATA%\\agent-shim\\logs\\agent-shim.log`, falling back
/// to `C:\\ProgramData\\` if the env var is somehow unset (it's defined
/// on every supported Windows version, but we don't panic on the off chance).
pub fn default_service_log_path() -> PathBuf {
    let base = std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("agent-shim").join("logs").join("agent-shim.log")
}

/// Mutate the in-memory `GatewayConfig` to add a default file logging
/// configuration if and only if the user did not specify one.
pub fn apply_service_log_fallback(cfg: &mut GatewayConfig) {
    if cfg.logging.file.is_some() {
        return;
    }
    cfg.logging.file = Some(FileLoggingConfig {
        path: default_service_log_path(),
        format: LogFormat::Json,
        rotation: RotationPolicy::Daily,
        max_files: 7,
    });
}
```

- [ ] **Step 2: Register the module in `mod.rs`**

Open `crates/gateway/src/commands/service/mod.rs`. Add to the `pub mod` declarations:

```rust
pub mod log_fallback;
```

- [ ] **Step 3: Verify the test passes**

```bash
cargo nextest run -p agent-shim --test service_log_fallback_unit
```

Expected on Windows: all three tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/gateway/src/commands/service/log_fallback.rs crates/gateway/src/commands/service/mod.rs
git commit -m "feat(gateway): service-mode logging.file fallback to %PROGRAMDATA%"
```

---

### Task 3: Implement the `run` SCM entry point

**Files:**
- Create: `crates/gateway/src/commands/service/run.rs`
- Delete: `crates/gateway/src/commands/service/run_stub.rs`
- Modify: `crates/gateway/src/commands/service/mod.rs`

This is the heart of Phase 4. The function is invoked by SCM, registers a control handler, and drives `run_core` under a tokio runtime. The control handler talks to the runtime via a `tokio::sync::watch` channel.

- [ ] **Step 1: Create the new `run.rs`**

```rust
// crates/gateway/src/commands/service/run.rs
//
// Windows Service entry point. Invoked by SCM via the command line
//   "<exe>" service run --config "<path>"
// Never invoked manually — `--help` hides it.
//
// Flow:
//   1. service_dispatcher::start hands control to SCM and calls service_main.
//   2. service_main:
//      a. Registers a control handler closure.
//      b. Reports StartPending.
//      c. Boots a tokio multi_thread runtime and drives run_core.
//      d. run_core's on_listening callback reports Running.
//      e. On Stop/Shutdown, the control handler triggers the watch channel,
//         which run_core's shutdown future awaits.
//      f. Once run_core returns, report Stopped(exit_code).
//
// Spec sections 4.4, 4.5, 4.6.

use crate::commands::service::log_fallback::apply_service_log_fallback;
use crate::commands::service::names::DEFAULT_SERVICE_NAME;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_service::define_windows_service;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
    ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult,
};
use windows_service::service_dispatcher;

define_windows_service!(ffi_service_main, service_main);

// The dispatcher does not pass user CLI arguments through to service_main
// (SCM calls the binary with a fixed ImagePath that was registered at
// install time). We stash the parsed --config path in a process-wide
// static so service_main can reach it.
static CONFIG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Entry point invoked from `commands::service::run` dispatch in `mod.rs`.
/// Stores the config path and hands control to SCM.
pub fn run(config: &std::path::Path) -> anyhow::Result<()> {
    if !config.is_absolute() {
        anyhow::bail!(
            "internal: service run requires an absolute --config path; got {:?}",
            config
        );
    }
    *CONFIG_PATH.lock().unwrap() = Some(config.to_path_buf());

    // Hand control to SCM. This call blocks until the service exits.
    // It returns an error if invoked outside a service context (e.g.
    // user typed `agent-shim service run --config foo.yaml` in a
    // regular terminal), which is exactly the user-facing message
    // we want.
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
    // Pull the config path back out of the static.
    let config = match CONFIG_PATH.lock().unwrap().clone() {
        Some(p) => p,
        None => {
            eprintln!("FATAL: service_main invoked without config path set");
            return;
        }
    };

    if let Err(e) = run_service(config) {
        eprintln!("service_main exited with error: {e}");
    }
}

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

    let status_handle =
        service_control_handler::register(DEFAULT_SERVICE_NAME, event_handler)
            .map_err(|e| anyhow::anyhow!("register control handler: {e}"))?;

    // Report StartPending. SCM gives us 30s before timeout; setting
    // wait_hint to 30s tells SCM "keep waiting".
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::StartPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(30),
        process_id: None,
    })?;

    // Load config first so we can fail fast (before tokio init) if
    // validation rejects.
    let mut cfg = agent_shim_config::load_from_path(&config_path)?;
    agent_shim_config::validate(&cfg)?;
    apply_service_log_fallback(&mut cfg);

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

    // Clone the status handle so both the on_listening callback (which
    // moves it) and the post-serve "Stopped" report (still in this
    // scope) have access. The handle type is Clone-able — under the
    // hood it's a thin wrapper around a Win32 handle.
    let status_for_listening = status_handle.clone();
    let on_listening = move |_addr: std::net::SocketAddr| {
        let _ = status_for_listening.set_service_status(ServiceStatus {
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
        crate::commands::serve::run_core(
            cfg,
            Some(config_path.clone()),
            shutdown_fut,
            on_listening,
        )
        .await
    });

    // Report Stopped regardless of outcome. Win32(0) for clean exit;
    // Win32(1) if run_core returned an error.
    let exit_code = if serve_result.is_ok() {
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

    serve_result.map_err(|e| anyhow::anyhow!("run_core: {e}"))
}
```

Notes on the implementation:

- `ServiceStatusHandle` (returned by `service_control_handler::register`) is `Clone`. We clone it once so the `on_listening` callback can move its copy while the outer function keeps the original for the post-serve "Stopped" report.
- The control handler closure runs on a thread owned by SCM — it must not block. We only send through the watch channel, which is non-blocking.
- We use `tokio::sync::watch` (not `oneshot`) because the on_listening callback and the Stop arrival are independent events: the watch makes it explicit that "the value flipped to true" is the signal.

- [ ] **Step 2: Delete the stub and update `mod.rs`**

Delete `run_stub.rs`:

```bash
rm crates/gateway/src/commands/service/run_stub.rs
```

In `crates/gateway/src/commands/service/mod.rs`:

- Replace `pub mod run_stub;` with `pub mod run;`.
- Change the dispatch arm `ServiceCommand::Run { config } => run_stub::run(&config),` to `ServiceCommand::Run { config } => run::run(&config),`.

`run::run` is **synchronous** because `service_dispatcher::start` blocks until the service exits. That means we cannot `.await` it; the caller (`commands::service::run`, the dispatch function in `mod.rs`) handles this by being async but calling sync via `tokio::task::spawn_blocking` — or by changing the dispatch function's signature.

Simplest fix: keep `commands::service::run` async but handle the sync `Run` arm specially:

```rust
pub async fn run(sub: ServiceCommand) -> anyhow::Result<()> {
    match sub {
        // ... other arms ...
        ServiceCommand::Run { config } => {
            tokio::task::spawn_blocking(move || run::run(&config))
                .await
                .map_err(|e| anyhow::anyhow!("service run task panicked: {e}"))?
        }
        // ...
    }
}
```

Update `commands/service/mod.rs` to match. (The `Start`/`Stop`/`Restart` arms also become non-blocking calls into `lifecycle::*` in task 4 below.)

- [ ] **Step 3: Verify the gateway compiles**

```bash
cargo build -p agent-shim
```

Expected: compiles cleanly on Windows. On Linux, the entire service module is gated out by `#[cfg(windows)]` so nothing changes.

- [ ] **Step 4: Commit**

```bash
git add crates/gateway/src/commands/service/run.rs crates/gateway/src/commands/service/run_stub.rs crates/gateway/src/commands/service/mod.rs
git commit -m "feat(gateway): SCM entry point — service run drives run_core under tokio"
```

---

### Task 4: Implement `start`, `stop`, `restart` client commands

**Files:**
- Create: `crates/gateway/src/commands/service/lifecycle.rs`
- Modify: `crates/gateway/src/commands/service/mod.rs`

These three commands talk to SCM as a client — they don't host the server. `start` issues `StartService`; `stop` issues `ControlService(STOP)` and polls until the state reaches Stopped; `restart` is stop-then-start.

- [ ] **Step 1: Create `lifecycle.rs`**

```rust
// crates/gateway/src/commands/service/lifecycle.rs
//
// Client-side service control: start, stop, restart. These commands
// talk to SCM from a regular CLI invocation; the running service is
// a separate process spawned by SCM via `service run`.

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

    service.start(&[] as &[&std::ffi::OsStr]).context("StartService")?;
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
    // stop first (ignore "already stopped" case)
    stop(name).ok();
    start(name)
}

fn wait_for_state(
    service: &windows_service::service::Service,
    target: ServiceState,
    target_name: &str,
) -> Result<()> {
    let start = Instant::now();
    loop {
        let status = service.query_status().context("query_status while polling")?;
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
```

- [ ] **Step 2: Update `commands/service/mod.rs` dispatch**

Replace the catch-all arm for start/stop/restart:

```rust
pub async fn run(sub: ServiceCommand) -> anyhow::Result<()> {
    match sub {
        ServiceCommand::Install { config, name, display_name, account, password, start_type } => {
            install::install(install::InstallArgs {
                config, name, display_name, account, password, start_type,
            })
        }
        ServiceCommand::Uninstall { name } => install::uninstall(&name),
        ServiceCommand::Status { name } => status::status(&name),
        ServiceCommand::Run { config } => {
            tokio::task::spawn_blocking(move || run::run(&config))
                .await
                .map_err(|e| anyhow::anyhow!("service run task panicked: {e}"))?
        }
        ServiceCommand::Start { name } => lifecycle::start(&name),
        ServiceCommand::Stop { name } => lifecycle::stop(&name),
        ServiceCommand::Restart { name } => lifecycle::restart(&name),
    }
}
```

Add `pub mod lifecycle;` to the module declarations at the top.

- [ ] **Step 3: Build for Windows**

```bash
cargo build -p agent-shim
```

Expected: compiles cleanly.

- [ ] **Step 4: Commit**

```bash
git add crates/gateway/src/commands/service/lifecycle.rs crates/gateway/src/commands/service/mod.rs
git commit -m "feat(gateway): service start/stop/restart client commands"
```

---

### Task 5: End-to-end `#[ignore]` lifecycle test

**Files:**
- Create: `crates/gateway/tests/service_lifecycle.rs`

This is the manual-acceptance integration test from the spec (section 8.1). Marked `#[ignore]`; requires admin; cleans up on panic via `Drop`.

- [ ] **Step 1: Write the test**

```rust
// crates/gateway/tests/service_lifecycle.rs
//
// Phase 4 SCM end-to-end smoke test. install → start → healthz →
// verify log file → stop → uninstall. Marked #[ignore]; admin only.

#![cfg(windows)]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

struct ServiceCleanup { name: String }
impl Drop for ServiceCleanup {
    fn drop(&mut self) {
        let _ = Command::new("sc").args(["stop", &self.name]).status();
        let _ = Command::new("sc").args(["delete", &self.name]).status();
    }
}

fn cargo_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agent-shim"))
}

fn pick_port() -> u16 {
    // Bind to 0, read the assigned port, close the listener.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn write_config(dir: &std::path::Path, port: u16, log_path: &std::path::Path) -> PathBuf {
    let path = dir.join("gateway.yaml");
    let log_path_str = log_path.display().to_string().replace('\\', "\\\\");
    let yaml = format!(
        "server:\n  bind: 127.0.0.1\n  port: {port}\nlogging:\n  format: json\n  filter: info\n  \
         file:\n    path: \"{log_path_str}\"\n    format: json\n    rotation: daily\n    max_files: 7\nupstreams: {{}}\nroutes: []\n"
    );
    std::fs::write(&path, yaml).unwrap();
    path
}

fn wait_for_healthz(port: u16, timeout: Duration) {
    let start = Instant::now();
    let url = format!("http://127.0.0.1:{port}/");
    loop {
        if let Ok(resp) = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap()
            .get(&url)
            .send()
        {
            if resp.status().is_success() {
                return;
            }
        }
        assert!(
            start.elapsed() < timeout,
            "service not responding on port {port} after {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
#[ignore = "requires administrator; run with cargo nextest run --run-ignored only"]
fn service_full_lifecycle() {
    let svc_name = format!("agent-shim-test-{}", uuid::Uuid::new_v4());
    let _cleanup = ServiceCleanup { name: svc_name.clone() };

    let tmp = tempfile::tempdir().unwrap();
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log_path = log_dir.join("agent-shim.log");
    let port = pick_port();
    let cfg = write_config(tmp.path(), port, &log_path);

    let bin = cargo_bin();

    // install
    let out = Command::new(&bin)
        .args(["service", "install", "--name", &svc_name, "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(out.status.success(), "install: {:?} {:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr));

    // start
    let out = Command::new(&bin)
        .args(["service", "start", "--name", &svc_name])
        .output()
        .unwrap();
    assert!(out.status.success(), "start: {:?} {:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr));

    // healthz
    wait_for_healthz(port, Duration::from_secs(15));

    // Verify log file exists. Rolling appender filename is
    // "agent-shim.log.YYYY-MM-DD" — look for ANY file in log_dir starting
    // with "agent-shim.log".
    let entries: Vec<_> = std::fs::read_dir(&log_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("agent-shim.log"))
        .collect();
    assert!(!entries.is_empty(), "no log file produced in {log_dir:?}");

    // stop
    let out = Command::new(&bin)
        .args(["service", "stop", "--name", &svc_name])
        .output()
        .unwrap();
    assert!(out.status.success(), "stop: {:?} {:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr));

    // status should now report Stopped
    let out = Command::new(&bin)
        .args(["service", "status", "--name", &svc_name])
        .output()
        .unwrap();
    assert!(out.status.success(), "status post-stop");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("State:       Stopped"), "expected Stopped, got: {stdout}");

    // uninstall
    let out = Command::new(&bin)
        .args(["service", "uninstall", "--name", &svc_name])
        .output()
        .unwrap();
    assert!(out.status.success(), "uninstall: {:?} {:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr));
}
```

If `reqwest` blocking client isn't a dev-dep of `agent-shim` already, check:

```bash
grep -n reqwest crates/gateway/Cargo.toml
```

The current `dev-dependencies` already pulls `reqwest` (line 60 of Cargo.toml). The blocking feature may need to be added:

```toml
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json", "stream", "blocking"] }
```

Inspect first and only add if needed.

- [ ] **Step 2: Compile the test**

```bash
cargo build -p agent-shim --tests
```

Expected: compiles. If `reqwest::blocking` is missing, add the `blocking` feature and recompile.

- [ ] **Step 3: Run the test from an elevated PowerShell**

```bash
cargo nextest run -p agent-shim --test service_lifecycle --run-ignored only
```

Expected on elevated Windows: PASS. The service is registered with a UUID-based name, started, hit with `/`, stopped, queried, and uninstalled.

If the test fails because the service won't start within 30s, check Event Viewer → Windows Logs → Application for entries from "agent-shim-test-*" to see the actual startup error. Most common issue: the test binary path under `target/debug/` is too long or has odd characters; use a release build (`cargo build --release` then re-run the test with `CARGO_BIN_EXE_agent-shim` pointing at the release exe — nextest already handles this if you use `--release`).

- [ ] **Step 4: Confirm no leftover service after the test**

```bash
sc query agent-shim-test-*
```

Expected: no entries. The `ServiceCleanup::Drop` ran, even on a passing test.

- [ ] **Step 5: Commit**

```bash
git add crates/gateway/tests/service_lifecycle.rs crates/gateway/Cargo.toml
git commit -m "test(gateway): SCM full-lifecycle smoke (install→start→hit→stop, ignored)"
```

(Only stage Cargo.toml if you actually had to add the `blocking` feature.)

---

### Task 6: Verify cross-platform invariants still hold

- [ ] **Step 1: Build for Linux**

If on Windows, cross-compile:

```bash
cargo check --workspace --target x86_64-unknown-linux-gnu
```

If on Linux:

```bash
cargo check --workspace
cargo tree -p agent-shim | grep -i "windows[-_]" || echo "OK: no Windows crates in Linux build"
```

Expected: build passes, no Windows crates leak into the Linux dep graph.

- [ ] **Step 2: Run the full non-ignored test suite on whichever platform you're on**

```bash
cargo nextest run --workspace
```

Expected: green. Phase 1's `serve_core_smoke`, Phase 2's `file_logging_smoke`, Phase 3's `service_subcommand_parse` all pass.

- [ ] **Step 3: Run fmt + clippy + deny**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
```

Expected: all green.

- [ ] **Step 4: Final commit if any fmt/clippy adjustments**

Only if needed:

```bash
git add -u
git commit -m "style: cargo fmt + clippy after phase 4"
```

---

## Done Criteria

- `agent-shim service run --config <abs-path>` is a hidden subcommand. When invoked directly from a terminal (i.e. NOT by SCM), it returns a friendly error explaining that it's the SCM entry.
- `agent-shim service start --name <name>` issues `StartService` and polls until SCM reports `Running`. The SCM `Running` state corresponds to "TCP listener is bound" (port-bind-then-Running semantics).
- `agent-shim service stop --name <name>` issues `ControlService(STOP)` and polls until SCM reports `Stopped`. The actual gateway shutdown reuses `run_core`'s graceful shutdown path (axum graceful shutdown + OTel drain).
- `agent-shim service restart --name <name>` is stop-then-start.
- Service-mode `logging.file` fallback works: a service with no `logging.file` in its config still produces logs at `%PROGRAMDATA%\agent-shim\logs\agent-shim.log` by default.
- The `service_lifecycle.rs` `#[ignore]` integration test passes from an elevated terminal.
- Linux/macOS: build remains clean, no Windows crates in dep graph, no `service` subcommand in `--help`.
- `cargo deny check`, `cargo fmt`, `cargo clippy -- -D warnings` clean on both platforms.

Phase 4 is the lifecycle close-out. Phase 5 packages the Linux systemd example and finalizes documentation, then the feature is ready to ship.
