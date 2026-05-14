# Windows Service Integration and Cross-Platform File Logging

**Status:** Approved (brainstorming complete, ready for implementation planning)
**Date:** 2026-05-14
**Scope:** Add native Windows Service support to `agent-shim`, introduce optional cross-platform file logging, and provide a sample systemd unit for Linux deployment.

## 1. Goals

1. Let users run `agent-shim` as a Windows Service that starts automatically with the OS, with accurate "is it running?" visibility through the Service Control Manager (SCM).
2. Provide a file logging facility on all platforms so operators can read logs without attaching to stdout.
3. Preserve first-class Linux/macOS support: no behavioral change to `agent-shim serve`, no new Linux dependencies, and a documented systemd deployment path.

## 2. Non-Goals

- Rewriting the gateway's runtime model (we reuse the existing tokio runtime, axum server, and `commands::serve::run` core).
- Building log shipping, log search, or log retention beyond simple time-based rotation (`tracing-appender` daily/hourly).
- Multi-instance management UI; same-machine multi-instance is supported by `--name` but not orchestrated.
- macOS launchd integration. Out of scope for this iteration.

## 3. High-Level Architecture

Three independent deliverables:

1. **Windows Service subcommands** (`agent-shim service install|uninstall|start|stop|restart|status|run`). Compiled only on Windows targets. Linux/macOS builds never see this code or its dependencies.
2. **Cross-platform file logging** (`logging.file` config section). New `tracing-subscriber` file layer with daily rotation and asynchronous writes. Benefits all platforms.
3. **Linux systemd example** (`deploy/agent-shim.service`). Static unit file + documentation. No Rust changes.

### 3.1 Platform isolation strategy

`Commands::Service` is gated with `#[cfg(windows)]` so the `service` subcommand does not appear in `agent-shim --help` on Linux/macOS. The `windows-service` and `windows` crates are declared under `[target.'cfg(windows)'.dependencies]`, so non-Windows builds never pull them.

```rust
// crates/gateway/src/cli.rs
pub enum Commands {
    Serve { ... },
    ValidateConfig { ... },
    Copilot { ... },
    #[cfg(windows)]
    Service { sub: ServiceCommand },
}
```

```toml
# crates/gateway/Cargo.toml
[target.'cfg(windows)'.dependencies]
windows-service = "0.7"
windows = { version = "0.58", features = ["Win32_Security", "Win32_System_Threading"] }
```

The file logging layer lives in `crates/observability` and has no platform gates.

### 3.2 Files touched

- `crates/gateway/src/cli.rs` — CLI surface
- `crates/gateway/src/commands/service/` — new module, Windows-only
- `crates/gateway/src/commands/serve.rs` — refactor to expose a core function reusable from both foreground and SCM entry points
- `crates/observability/src/tracing_setup.rs` — file appender layer
- `crates/observability/Cargo.toml` — add `tracing-appender`
- `crates/config/src/schema.rs` — `LoggingConfig::file` field, new `FileLoggingConfig` and `RotationPolicy`
- `deploy/agent-shim.service` — new Linux systemd unit
- `docs/deployment.md`, `docs/configuration.md`, `docs/observability.md`, `README.md`, `CHANGELOG.md` — documentation

## 4. Windows Service Implementation

### 4.1 Module layout

```
crates/gateway/src/commands/service/
├── mod.rs          # ServiceCommand enum, subcommand routing
├── install.rs      # install / uninstall
├── lifecycle.rs    # start / stop / restart / status
├── run.rs          # SCM entry (hidden subcommand)
├── elevation.rs    # administrator privilege check
└── names.rs        # default service name, ImagePath construction
```

The entire module is gated with `#[cfg(windows)]` in `crates/gateway/src/commands/mod.rs` (`#[cfg(windows)] pub mod service;`). Linux/macOS builds do not walk the module tree.

### 4.2 Service metadata defaults

| Field           | Default                                                          | Override flag       |
|-----------------|------------------------------------------------------------------|---------------------|
| Service Name    | `agent-shim`                                                     | `--name`            |
| Display Name    | `AgentShim Gateway`                                              | `--display-name`    |
| Description     | `Protocol-translating API gateway for AI agents`                 | hard-coded          |
| Service Account | `LocalSystem`                                                    | `--account`         |
| Start Type      | `Automatic`                                                      | `--start-type`      |
| Error Control   | `Normal`                                                         | hard-coded          |
| Dependencies    | none                                                             | hard-coded          |

`--account` accepts `LocalSystem`, `NetworkService`, `LocalService`, or `DOMAIN\user` (the latter requires a `--password` flag).

`--start-type` accepts `auto`, `manual`, `disabled`.

### 4.3 ImagePath format

The command line registered with SCM is:

```
"C:\Path\To\agent-shim.exe" service run --config "C:\ProgramData\agent-shim\gateway.yaml"
```

- The executable path is resolved at install time via `std::env::current_exe()`.
- The `--config` path is whatever the user passed to `service install`, after validation. It is stored as an absolute path.
- If the resolved exe path lies under a `target/debug` or temp directory, `install` prints a warning but proceeds (useful during development).

### 4.4 SCM main loop (`run.rs`)

`agent-shim service run --config <path>` is the SCM entry point. It is hidden from `--help` (`clap(hide = true)`) because users should never run it manually.

```rust
fn service_main(args: Vec<OsString>) -> Result<()> {
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;
    status_handle.set_status(SERVICE_STATUS_START_PENDING { wait_hint: 30s, .. })?;

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);

    // event_handler closure (registered above):
    //   ServiceControl::Stop | Shutdown => {
    //     stop_tx.send(true);
    //     status_handle.set_status(StopPending { wait_hint: 30s });
    //   }
    //   ServiceControl::Interrogate => return current status
    //   _ => return NotImplemented

    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(serve_under_scm(config_path, stop_rx, status_handle.clone()));

    status_handle.set_status(if result.is_ok() {
        ServiceStatus::Stopped { exit_code: 0 }
    } else {
        ServiceStatus::Stopped { exit_code: 1 }
    })?;
    Ok(())
}

service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
```

### 4.5 Refactoring `commands::serve::run`

To avoid duplicating logic between the foreground `serve` command and the SCM `run` entry point, `commands::serve::run` is refactored:

```rust
// Public API stays the same.
pub async fn run(config_path: &Path) -> Result<()> {
    run_core(
        load_and_validate_config(config_path)?,
        Some(config_path.to_path_buf()),
        crate::shutdown::shutdown_signal(),
        |_| {}, // on_listening: no-op for foreground
    ).await
}

// New core function, reusable from SCM.
pub async fn run_core(
    cfg: GatewayConfig,
    config_path: Option<PathBuf>,
    shutdown: impl Future<Output = ()> + Send + 'static,
    on_listening: impl FnOnce(SocketAddr) + Send + 'static,
) -> Result<()> {
    // ... existing serve::run body, with two integration points:
    //   1. Pass `shutdown` to axum's with_graceful_shutdown.
    //   2. Call on_listening(addr) immediately after Server::bind succeeds,
    //      before .serve() awaits.
}
```

The SCM caller passes a watch-channel future as `shutdown` and a closure that calls `SetServiceStatus(Running)` as `on_listening`. This implements the "port-bound-then-Running" semantics: SCM reports Running only after the listener is accepting connections.

### 4.6 Graceful shutdown chain

```
SCM "Stop" command
  → event_handler receives ServiceControl::Stop
  → stop_tx.send(true) + SetServiceStatus(StopPending, wait_hint=30s)
  → tokio runtime: axum::Server::with_graceful_shutdown wakes
  → existing OTel shutdown in commands::serve::run runs
  → service_main function returns
  → SetServiceStatus(Stopped, exit_code=0)
```

This reuses the existing graceful shutdown path. No new shutdown mechanism is introduced.

### 4.7 `status` command output

```
$ agent-shim service status
Service:     agent-shim
Display:     AgentShim Gateway
State:       Running
PID:         12345
Start Type:  Automatic
Account:     LocalSystem
ImagePath:   "C:\Program Files\agent-shim\agent-shim.exe" service run --config "C:\ProgramData\agent-shim\gateway.yaml"
Config:      C:\ProgramData\agent-shim\gateway.yaml
```

- PID comes from `QueryServiceStatusEx` (`SERVICE_STATUS_PROCESS.dwProcessId`).
- Config path is parsed from ImagePath by splitting args and locating `--config`.
- `status` does not require administrator privileges (`SC_MANAGER_CONNECT` + `SERVICE_QUERY_STATUS` only).

### 4.8 Elevation check

`install`, `uninstall`, `start`, `stop`, `restart` require administrator privileges. The check happens at the top of each command:

```rust
pub fn require_admin() -> anyhow::Result<()> {
    if !is_elevated()? {
        anyhow::bail!(
            "This command requires administrator privileges.\n\
             Please re-run from an elevated PowerShell or CMD."
        );
    }
    Ok(())
}
```

`is_elevated` calls `GetTokenInformation(current_process_token, TokenElevation)`.

### 4.9 Install-time validation

`agent-shim service install --config <path>` enforces:

1. The config path is absolute (relative paths rejected with a clear error).
2. The file exists and is readable.
3. The file passes `agent_shim_config::validate` (reuses the existing `validate-config` codepath).
4. If `--account` is `DOMAIN\user`, `--password` must also be provided.

Failures abort install before any SCM call, so a misconfigured service is never registered.

### 4.10 Same-machine multi-instance

`--name` lets operators register multiple instances with different config files:

```
agent-shim service install --name agent-shim-anthropic --config C:\...\anthropic.yaml
agent-shim service install --name agent-shim-openai    --config C:\...\openai.yaml
```

Each instance is independent. Default `--name` is `agent-shim`.

## 5. Cross-Platform File Logging

### 5.1 Configuration schema

```rust
// crates/config/src/schema.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    #[serde(default)]
    pub format: LogFormat,                  // existing, stdout format

    #[serde(default = "default_filter")]
    pub filter: String,                     // existing

    #[serde(default)]
    pub file: Option<FileLoggingConfig>,    // new
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileLoggingConfig {
    /// Log file path. Parent directory is created automatically at startup.
    pub path: PathBuf,

    /// File log format, independent of stdout format. Default: json.
    #[serde(default = "default_file_format")]
    pub format: LogFormat,

    /// Rotation policy. Default: daily.
    #[serde(default)]
    pub rotation: RotationPolicy,

    /// Maximum number of log files retained (current + history). 0 = unlimited.
    /// Default: 7.
    #[serde(default = "default_max_files")]
    pub max_files: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RotationPolicy {
    #[default]
    Daily,
    Hourly,
    Never,
}

fn default_file_format() -> LogFormat { LogFormat::Json }
fn default_max_files() -> usize { 7 }
```

`deny_unknown_fields` preserves project convention; typos fail at startup.

### 5.2 Observability integration

```rust
// crates/observability/src/tracing_setup.rs
pub struct TracingHandles {
    pub otel: Option<OtelHandle>,
    pub file_guard: Option<tracing_appender::non_blocking::WorkerGuard>,  // new
}

pub fn init(log: &LoggingConfig, otel_cfg: Option<&OtelConfig>) -> TracingHandles {
    // existing filter and otel_layer logic unchanged

    let (file_layer, file_guard) = build_file_layer(log.file.as_ref());

    let registry = tracing_subscriber::registry()
        .with(otel_layer)
        .with(file_layer)
        .with(filter);

    // stdout layer unchanged
    match log.format { LogFormat::Json => ..., LogFormat::Pretty => ... }

    TracingHandles { otel: handle, file_guard }
}

fn build_file_layer(cfg: Option<&FileLoggingConfig>)
    -> (Option<Box<dyn Layer<Registry> + Send + Sync>>, Option<WorkerGuard>)
{
    let Some(cfg) = cfg else { return (None, None); };

    // 1. Create parent directory. Fatal on failure (parity with OTel init).
    if let Some(parent) = cfg.path.parent() {
        std::fs::create_dir_all(parent).unwrap_or_else(|e| {
            eprintln!("FATAL: cannot create log directory {parent:?}: {e}");
            std::process::exit(2);
        });
    }

    // 2. RollingFileAppender.
    let rotation = match cfg.rotation {
        RotationPolicy::Daily  => Rotation::DAILY,
        RotationPolicy::Hourly => Rotation::HOURLY,
        RotationPolicy::Never  => Rotation::NEVER,
    };
    let appender = RollingFileAppender::builder()
        .rotation(rotation)
        .max_log_files(if cfg.max_files == 0 { usize::MAX } else { cfg.max_files })
        .filename_prefix(cfg.path.file_name().unwrap().to_string_lossy().into_owned())
        .build(cfg.path.parent().unwrap())
        .unwrap_or_else(|e| {
            eprintln!("FATAL: cannot initialize log file appender: {e}");
            std::process::exit(2);
        });

    // 3. non_blocking wrapper (async write).
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    // 4. Format-specific layer.
    let layer: Box<dyn Layer<Registry> + Send + Sync> = match cfg.format {
        LogFormat::Json   => Box::new(fmt::layer().json().with_writer(non_blocking)),
        LogFormat::Pretty => Box::new(fmt::layer().with_writer(non_blocking)),
    };

    (Some(layer), Some(guard))
}
```

Key invariants:

- `WorkerGuard` must outlive every emitted log event. `TracingHandles` carries it; `commands::serve::run` returns at process exit and the guard drops, flushing the buffer.
- `tracing-appender` is the official tokio-rs/tracing companion crate; no third-party rotation library is introduced.
- Rolled file names follow `tracing-appender` conventions: `agent-shim.log.2026-05-14` (daily), `agent-shim.log.2026-05-14-15` (hourly).

### 5.3 New dependency

```toml
# Workspace Cargo.toml
[workspace.dependencies]
tracing-appender = "0.2"

# crates/observability/Cargo.toml
tracing-appender = { workspace = true }
```

License is MIT, identical to existing tracing-* dependencies. `cargo deny check` should pass without rule changes.

### 5.4 Service-mode fallback

When `agent-shim service run` enters from SCM and the loaded config has no `logging.file` set, the SCM entry injects a default before initializing tracing:

```rust
async fn serve_under_scm(config_path: &Path, ...) -> Result<()> {
    let mut cfg = agent_shim_config::load_from_path(config_path)?;
    agent_shim_config::validate(&cfg)?;

    if cfg.logging.file.is_none() {
        cfg.logging.file = Some(FileLoggingConfig {
            path: default_service_log_path(),
            format: LogFormat::Json,
            rotation: RotationPolicy::Daily,
            max_files: 7,
        });
    }

    run_core(cfg, Some(config_path.to_path_buf()), shutdown_future, on_listening).await
}

fn default_service_log_path() -> PathBuf {
    let base = std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("agent-shim").join("logs").join("agent-shim.log")
}
```

Foreground `serve` does not inject this default — operators running interactively see logs on stdout.

The stdout layer remains active in service mode but writes to a detached handle, which is standard Windows Service behavior. No special handling required.

## 6. Linux systemd Example

`deploy/agent-shim.service`:

```ini
[Unit]
Description=AgentShim Gateway - Protocol-translating API gateway for AI agents
Documentation=https://github.com/anthropics/agent-shim
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=agent-shim
Group=agent-shim
ExecStart=/usr/local/bin/agent-shim serve --config /etc/agent-shim/gateway.yaml
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=5s

NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/log/agent-shim
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
LockPersonality=true
RestrictSUIDSGID=true

LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

`ExecReload` triggers SIGHUP, reusing the existing reload handler in `commands/serve.rs` (`#[cfg(unix)]` block at line 46). No Rust changes required.

Deployment steps (documented in `docs/deployment.md`):

1. Create the `agent-shim` system user: `useradd --system --no-create-home --shell /usr/sbin/nologin agent-shim`
2. Prepare directories: `/etc/agent-shim/` (config, read-only), `/var/log/agent-shim/` (owned by agent-shim user)
3. Install: `sudo cp deploy/agent-shim.service /etc/systemd/system/ && sudo systemctl daemon-reload`
4. Start: `sudo systemctl enable --now agent-shim`
5. Logs: `journalctl -u agent-shim -f`
6. Reload config: `sudo systemctl reload agent-shim`

`logging.file` remains optional on Linux. Operators using journald can omit it; operators wanting a file under `/var/log/agent-shim/agent-shim.log` add it to their config.

## 7. Documentation Updates

| File                          | Change                                                                                       |
|-------------------------------|----------------------------------------------------------------------------------------------|
| `docs/deployment.md`          | Add **Windows Service** section (install/start/stop, UAC notes, directory conventions). Add **Linux systemd** section (sample unit + steps). Existing docker-compose section retained. |
| `docs/observability.md`       | Add **File logging** subsection (YAML fields, rotation behavior, JSON vs pretty, async-write caveats including SIGKILL data loss). |
| `docs/configuration.md`       | Document the new `logging.file` schema.                                                      |
| `README.md`                   | Quick-start cross-reference: "Running as a service? See [docs/deployment.md](docs/deployment.md)." |
| `CHANGELOG.md`                | Under `[Unreleased]`: "Added: Windows Service support (`agent-shim service` subcommands)", "Added: File logging with daily rotation (`logging.file` config)", "Added: Example systemd unit at `deploy/agent-shim.service`". |

`CLAUDE.md` does not change. The service subcommand is a CLI-layer concern; the crate dependency graph is unaffected.

## 8. Testing Strategy

| Test type                       | Platform           | CI?  | Coverage                                                                                  |
|---------------------------------|--------------------|------|-------------------------------------------------------------------------------------------|
| Unit                            | All                | yes  | CLI parse, config schema (round-trip serde for `FileLoggingConfig`/`RotationPolicy`), `ImagePath` string construction, `default_service_log_path` computation. |
| Negative-platform unit          | Linux/macOS        | yes  | `Commands` enum does not have `Service` variant on non-Windows. `cargo tree -p agent-shim` does not show `windows-service` on Linux. |
| File logging integration        | All                | yes  | `tempfile` directory + `logging.file` config → emit logs → file exists, contains expected JSON keys, rotation produces a second file when date advances (use mock time via `tracing-appender` test hooks if available, else just verify single-file write path). |
| Windows Service end-to-end      | Windows only       | no   | `#[ignore]` test in `crates/gateway/tests/service_lifecycle.rs`. Full install → start → query → healthz → stop → uninstall cycle. Run manually with `cargo nextest run --run-ignored only`. |
| Manual acceptance               | Windows + Linux + macOS | no | Checklist in section 9.                                                                  |

### 8.1 End-to-end integration test

```rust
// crates/gateway/tests/service_lifecycle.rs
#![cfg(windows)]

#[test]
#[ignore = "requires administrator privileges; run with --run-ignored only"]
fn service_full_lifecycle() {
    let service_name = format!("agent-shim-test-{}", uuid::Uuid::new_v4());
    let _cleanup = ServiceCleanup::new(&service_name);
    let tmp = tempfile::tempdir().unwrap();
    let port = pick_random_port();
    let config_path = write_minimal_config(&tmp, port);
    let log_dir = tmp.path().join("logs");

    run_cli(&[
        "service", "install",
        "--name", &service_name,
        "--config", config_path.to_str().unwrap(),
    ]).success();

    run_cli(&["service", "start", "--name", &service_name]).success();
    wait_for_state(&service_name, ServiceState::Running, Duration::from_secs(30));

    let resp = reqwest::blocking::get(format!("http://127.0.0.1:{port}/healthz"));
    assert!(resp.is_ok(), "healthz unreachable: {resp:?}");

    let log_file = log_dir.join("agent-shim.log");
    wait_for_file(&log_file, Duration::from_secs(5));
    let content = std::fs::read_to_string(&log_file).unwrap();
    assert!(content.contains("\"level\""), "log file is not JSON: {content:?}");

    run_cli(&["service", "stop", "--name", &service_name]).success();
    wait_for_state(&service_name, ServiceState::Stopped, Duration::from_secs(30));

    run_cli(&["service", "uninstall", "--name", &service_name]).success();
}

struct ServiceCleanup { name: String }
impl ServiceCleanup {
    fn new(name: &str) -> Self { Self { name: name.to_string() } }
}
impl Drop for ServiceCleanup {
    fn drop(&mut self) {
        let _ = std::process::Command::new("sc").args(["stop", &self.name]).status();
        let _ = std::process::Command::new("sc").args(["delete", &self.name]).status();
    }
}
```

The cleanup `Drop` runs even if the test panics, so a failed run does not leave a service registered.

## 9. Manual Acceptance Checklist

### Windows (run from elevated PowerShell)

- [ ] `agent-shim service install --config <abs-path>` succeeds; `sc query agent-shim` reports `STOPPED`.
- [ ] `agent-shim service install --config <invalid-path>` fails; no service registered.
- [ ] `agent-shim service install --config <config-with-typo>` fails (validate-config rejection); no service registered.
- [ ] `agent-shim service install --config foo.yaml` (relative path) is rejected with a clear error.
- [ ] Non-elevated terminal invocation of `install` prints "requires administrator privileges" and exits non-zero.
- [ ] `agent-shim service start` causes `sc query` to report `RUNNING`; `netstat -an | findstr :8787` shows LISTEN.
- [ ] `agent-shim service status` prints state, PID, and the config path resolved from ImagePath.
- [ ] Default log file appears at `C:\ProgramData\agent-shim\logs\agent-shim.log` and contains JSON-formatted entries.
- [ ] `agent-shim service stop` returns within 5 seconds; `sc query` reports `STOPPED`; process is gone.
- [ ] Send a sample request through the gateway and verify streaming responses work under service mode.
- [ ] Cross-midnight test: leave the service running across midnight; verify `agent-shim.log.YYYY-MM-DD` is created for the previous day.
- [ ] `agent-shim service restart` performs stop+start cleanly.
- [ ] `agent-shim service uninstall` removes the registration; `sc query agent-shim` reports the service does not exist.
- [ ] Multi-instance test: install two services with different `--name` and different ports; both run independently.

### Linux

- [ ] `cargo build --workspace` succeeds.
- [ ] `cargo tree -p agent-shim | grep windows-service` returns nothing.
- [ ] `agent-shim --help` does not show a `service` subcommand.
- [ ] systemd unit deploys; `systemctl status agent-shim` reports active; `journalctl -u agent-shim` shows logs.
- [ ] `systemctl reload agent-shim` triggers config reload (visible via the `agent_shim::reload` tracing target).
- [ ] With `logging.file` configured, log file appears at the configured path and rotates daily.

### macOS

- [ ] `cargo build --workspace` succeeds; `agent-shim --help` does not show `service`.
- [ ] `logging.file` works: file appears, daily rotation effective.

## 10. Risks and Mitigations

| Risk                                                                               | Impact                                                       | Mitigation                                                                                                  |
|------------------------------------------------------------------------------------|--------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------|
| `tracing-appender` path handling on Windows (`\` separators)                       | Filename or rotation bugs                                    | Unit tests use Windows-style paths; CI Windows runner exercises file logging integration test.              |
| SCM 30-second start timeout blocked by slow OTel exporter init                     | Service start fails                                          | `on_listening` fires after `Server::bind` and before serve loop; OTel init errors are already fatal (existing behavior). Document the requirement that the configured OTel endpoint, if any, must be reachable. |
| `windows-service` crate breaking API changes between minor versions                | Compilation breakage on upgrade                              | Pin to `0.7.x`; CI `cargo deny check` includes advisory scanning.                                           |
| `non_blocking` writer drops final log lines on SIGKILL                             | Crash diagnostics may lose last few events                   | Documented in `docs/observability.md`. Normal stop paths (SCM Stop / Ctrl+C / SIGTERM) drop `WorkerGuard` and flush. |
| Operator configures relative `logging.file.path` and runs as a service             | File resolves relative to `C:\Windows\System32`              | `build_file_layer` warns when `path.is_relative()`. Documentation strongly recommends absolute paths.       |
| Backslashes in YAML config paths confuse users                                     | Misconfigured paths                                          | All documentation examples use double-backslash (`C:\\foo`) or forward slashes (`C:/foo`).                  |
| Same-machine multi-instance port collision goes unnoticed                          | Second service appears Running briefly then stops            | `on_listening` runs after a successful `bind`; if `bind` fails, the service is reported Stopped with non-zero exit code. SCM accurately reflects this. |
| Multi-line process management on `tracing-appender` upgrade                        | Future version bumps may change rotation file naming         | Pin `tracing-appender = "0.2"`; document tested version in `CHANGELOG.md`.                                  |

## 11. Implementation Order

Each phase is independently reviewable, mergeable, and useful. Phases 1 and 2 leave the project in a strictly better state even if subsequent phases are deferred.

1. **Phase 1 — Refactor `commands::serve::run`.** Extract `run_core(cfg, config_path, shutdown, on_listening)` from the existing function. Foreground `serve` becomes a thin wrapper. No new dependencies. Existing tests must continue to pass; the refactor is behavior-preserving.
2. **Phase 2 — Cross-platform file logging.** Add `FileLoggingConfig` and `RotationPolicy` to the config schema. Add `build_file_layer` and `file_guard` to `tracing_setup`. Add `tracing-appender` dependency. Integration test in `crates/observability` (or `crates/gateway/tests`) covering write + rotation. Update `docs/observability.md` and `docs/configuration.md`. After this phase, Linux and macOS users have file logging.
3. **Phase 3 — Windows Service install/uninstall/status.** Add `commands/service/` module under `#[cfg(windows)]`. Implement `install` (with `validate-config` precheck), `uninstall`, `status`, and `elevation` check. No SCM main loop yet — these commands only register and query. Unit tests for CLI parse, ImagePath construction, and the Linux negative-build assertion.
4. **Phase 4 — Windows Service run/start/stop/restart.** Implement `commands/service/run.rs` SCM entry calling `run_core` with watch-channel-based shutdown and a `SetServiceStatus(Running)` callback. Implement `start`, `stop`, `restart` client commands. Add service-mode log fallback default. Add the `#[ignore]` end-to-end test.
5. **Phase 5 — Linux systemd example and documentation closeout.** Add `deploy/agent-shim.service`. Update `docs/deployment.md` with Windows + Linux sections. Update `README.md` and `CHANGELOG.md`.

## 12. Open Questions

None at this time. All design decisions were resolved during brainstorming. Implementation may surface details (specific `windows-service` crate API ergonomics, exact rolled-file naming on disk) but these are local choices that do not affect the architecture.
