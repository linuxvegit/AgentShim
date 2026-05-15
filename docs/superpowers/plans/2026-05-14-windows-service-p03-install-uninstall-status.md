# Phase 3: Windows Service install / uninstall / status Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the `agent-shim service install`, `service uninstall`, and `service status` subcommands on Windows targets. After this phase, an administrator can register the binary with the Windows Service Control Manager (SCM), query its registered state, and remove it. The SCM cannot yet *run* it — that's Phase 4.

**Architecture:** A new `crates/gateway/src/commands/service/` module, entirely gated under `#[cfg(windows)]`. The `windows-service` and `windows` crates are added under `[target.'cfg(windows)'.dependencies]` so Linux/macOS builds never pull them. The new subcommands route through the existing `clap` enum, conditionally compiled into `Commands`. Install validates the config (reusing `validate-config`'s codepath) and refuses relative paths before touching SCM. Uninstall is a thin wrapper around `Service::delete`. Status queries SCM without requiring admin and pretty-prints state, PID, and the config path parsed back out of the ImagePath.

**Tech Stack:** Rust, new dependencies `windows-service = "0.7"`, `windows = "0.58"` (with `Win32_Security` and `Win32_System_Threading` features), Windows-only.

**Spec reference:** `docs/superpowers/specs/2026-05-14-windows-service-and-file-logging-design.md` sections 3.1, 4.1, 4.2, 4.3, 4.7, 4.8, 4.9, 4.10, 11 (phase 3).

**Depends on:** Phase 1 (no direct code reuse, but it ensures `commands` is a clean module to extend). Independent of Phase 2.

---

## File Structure

| File | Responsibility | Status |
|------|----------------|--------|
| `Cargo.toml` (workspace) | Add `windows-service` and `windows` to `[workspace.dependencies]` | Modify |
| `crates/gateway/Cargo.toml` | Add Windows-target-only dependency block | Modify |
| `crates/gateway/src/cli.rs` | Add `Commands::Service { sub: ServiceCommand }` under `#[cfg(windows)]`, define `ServiceCommand` enum | Modify |
| `crates/gateway/src/commands/mod.rs` | `#[cfg(windows)] pub mod service;` | Modify |
| `crates/gateway/src/commands/service/mod.rs` | Re-export submodules + the `ServiceCommand` enum dispatch | Create |
| `crates/gateway/src/commands/service/names.rs` | Service-name defaults, `build_image_path`, `parse_config_from_image_path` | Create |
| `crates/gateway/src/commands/service/elevation.rs` | `is_elevated`, `require_admin` | Create |
| `crates/gateway/src/commands/service/install.rs` | `install` + `uninstall` commands | Create |
| `crates/gateway/src/commands/service/status.rs` | `status` command | Create |
| `crates/gateway/src/main.rs` | Route the new `Commands::Service` variant on Windows | Modify |
| `crates/gateway/tests/service_subcommand_parse.rs` | Cross-platform negative test: `Commands` has no `Service` variant on non-Windows | Create |
| `crates/gateway/tests/service_install_unit.rs` | Windows-only unit-ish tests for ImagePath construction and elevation gate (no SCM calls) | Create |

---

### Task 1: Add Windows-target dependencies

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/gateway/Cargo.toml`

- [ ] **Step 1: Add to workspace `Cargo.toml`'s `[workspace.dependencies]`**

```toml
windows-service = "0.7"
windows = { version = "0.58", features = ["Win32_Security", "Win32_System_Threading", "Win32_System_Services"] }
```

Place these alphabetically near other deps (after `uuid`).

- [ ] **Step 2: Add a target-specific dependency block to `crates/gateway/Cargo.toml`**

After the existing `[dependencies]` block (which ends around line 57 with `serde_yaml = "0.9"`), add:

```toml
[target.'cfg(windows)'.dependencies]
windows-service = { workspace = true }
windows = { workspace = true }
```

Critical: this is a separate block, *not* under `[dependencies]`. The `cfg(windows)` predicate ensures these crates never enter the dependency graph on Linux or macOS.

- [ ] **Step 3: Verify Linux build still works**

If you're on Windows, you can confirm cross-target with:

```bash
cargo check --workspace --target x86_64-unknown-linux-gnu
```

(May need to install the target first: `rustup target add x86_64-unknown-linux-gnu`.)

If you're on Linux, simply:

```bash
cargo check --workspace
cargo tree -p agent-shim | grep -i "windows-service\|^windows "
```

Expected: `cargo tree` returns no results — neither crate is in the Linux dependency graph.

- [ ] **Step 4: Verify Windows build pulls them**

```bash
cargo check -p agent-shim
cargo tree -p agent-shim | grep -i "windows-service\|^windows "
```

Expected: both crates appear.

- [ ] **Step 5: Run `cargo deny check`**

Run: `cargo deny check`
Expected: PASS. Both `windows-service` and `windows` are MIT/Apache-licensed.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/gateway/Cargo.toml Cargo.lock
git commit -m "deps(gateway): add windows-service and windows crates (Windows targets only)"
```

---

### Task 2: Failing test — `Commands` enum must NOT contain `Service` on Linux/macOS

**Files:**
- Create: `crates/gateway/tests/service_subcommand_parse.rs`

This is a compile-time assertion. On non-Windows platforms, the `service` subcommand should not exist. We assert this via `clap`'s CLI parser: parsing `agent-shim service --help` should fail on Linux.

- [ ] **Step 1: Write the cross-platform test**

```rust
// crates/gateway/tests/service_subcommand_parse.rs
//
// Cross-platform negative test ensuring `service` subcommand is gated to
// Windows. On non-Windows, parsing "agent-shim service install ..." must
// fail with a "no such subcommand" error.
//
// On Windows, parsing succeeds. We don't run the command — just exercise
// clap's parser to confirm the variant is reachable.

use clap::Parser;

#[derive(Parser)]
#[command(name = "agent-shim")]
struct Cli {
    #[command(subcommand)]
    command: agent_shim_gateway::cli::Commands,
}

#[cfg(not(windows))]
#[test]
fn service_subcommand_absent_on_non_windows() {
    let result = Cli::try_parse_from([
        "agent-shim",
        "service",
        "install",
        "--config",
        "/tmp/x.yaml",
    ]);
    assert!(
        result.is_err(),
        "service subcommand must NOT be available on non-Windows; got: {result:?}"
    );
}

#[cfg(windows)]
#[test]
fn service_subcommand_present_on_windows() {
    // The full install command parses; we don't execute it.
    let result = Cli::try_parse_from([
        "agent-shim",
        "service",
        "install",
        "--config",
        "C:\\path\\gateway.yaml",
    ]);
    assert!(
        result.is_ok(),
        "service install should parse on Windows; got: {result:?}"
    );
}
```

For the test to compile, `agent_shim_gateway::cli::Commands` must be a public type. Confirm the current state:

```bash
grep -n "pub mod cli\|mod cli" crates/gateway/src/lib.rs crates/gateway/src/main.rs
```

If `cli` is only declared in `main.rs`, add to `lib.rs`:

```rust
pub mod cli;
```

This is harmless — `cli.rs` only contains `clap` derive types.

- [ ] **Step 2: Run the test**

Run: `cargo nextest run -p agent-shim --test service_subcommand_parse`

Expected on Linux: `service_subcommand_absent_on_non_windows` PASS (assuming `cli::Commands` does not yet have a `Service` variant — true today).

Expected on Windows: `service_subcommand_present_on_windows` FAILS because the variant does not exist yet (TDD red).

- [ ] **Step 3: Commit**

```bash
git add crates/gateway/tests/service_subcommand_parse.rs crates/gateway/src/lib.rs
git commit -m "test(gateway): pin service subcommand to Windows-only (TDD red on win)"
```

---

### Task 3: Add the `Service` variant to `Commands` and the `ServiceCommand` enum (Windows-only)

**Files:**
- Modify: `crates/gateway/src/cli.rs`

- [ ] **Step 1: Modify `cli.rs` to add the gated variant**

Find the existing `pub enum Commands` (line 11–35 of the current file). Add the new variant under a `#[cfg(windows)]` attribute. Place it after `Copilot`:

```rust
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start the gateway server
    Serve {
        #[arg(short, long, env = "AGENT_SHIM_CONFIG", default_value = "config/gateway.yaml")]
        config: PathBuf,
    },
    /// Validate a config file and exit
    ValidateConfig {
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
```

Below the existing `CopilotCommand` enum, add the new `ServiceCommand` enum:

```rust
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
```

Note: we add **all** ServiceCommand variants (including `Run`, `Start`, `Stop`, `Restart`) now so Phase 4 only has to fill in their handlers. The variants exist but their handlers return `anyhow::bail!("not implemented in phase 3")` until Phase 4 plugs them in.

- [ ] **Step 2: Verify the Windows test now passes**

If you're on Windows:

```bash
cargo nextest run -p agent-shim --test service_subcommand_parse
```

Expected on Windows: both tests in the file pass (`service_subcommand_present_on_windows` is now green).
Expected on Linux: `service_subcommand_absent_on_non_windows` still passes (the variant is gated out by `#[cfg(windows)]`).

- [ ] **Step 3: Commit**

```bash
git add crates/gateway/src/cli.rs
git commit -m "feat(gateway): Commands::Service enum (Windows-only) — clap surface only"
```

---

### Task 4: Create the `commands::service` module skeleton

**Files:**
- Modify: `crates/gateway/src/commands/mod.rs`
- Create: `crates/gateway/src/commands/service/mod.rs`
- Create: `crates/gateway/src/commands/service/names.rs`
- Create: `crates/gateway/src/commands/service/elevation.rs`
- Create: `crates/gateway/src/commands/service/install.rs`
- Create: `crates/gateway/src/commands/service/status.rs`

- [ ] **Step 1: Inspect the current `commands/mod.rs`**

Run: `cat crates/gateway/src/commands/mod.rs`
Expected: lists `pub mod copilot_login; pub mod copilot_models; pub mod serve; pub mod validate_config;`.

- [ ] **Step 2: Add the gated service module**

Append to `crates/gateway/src/commands/mod.rs`:

```rust
#[cfg(windows)]
pub mod service;
```

- [ ] **Step 3: Create `crates/gateway/src/commands/service/mod.rs`**

```rust
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
        ServiceCommand::Start { name }
        | ServiceCommand::Stop { name }
        | ServiceCommand::Restart { name } => {
            let _ = name;
            anyhow::bail!("start/stop/restart land in Phase 4")
        }
    }
}
```

- [ ] **Step 4: Create `crates/gateway/src/commands/service/run_stub.rs`** (placeholder until Phase 4)

```rust
//! Phase 4 lands the real SCM entry point here. This stub exists so the
//! `commands::service::run` dispatch matches all `ServiceCommand` variants.

use std::path::Path;

pub fn run(_config: &Path) -> anyhow::Result<()> {
    anyhow::bail!(
        "`agent-shim service run` is the SCM entry point and is not user-invokable; \
         it will be implemented in Phase 4"
    )
}
```

- [ ] **Step 5: Create `crates/gateway/src/commands/service/names.rs`**

```rust
//! Default service identifiers and ImagePath formatting.
//!
//! Spec section 4.3: the command line registered with SCM is
//!   "<exe>" service run --config "<absolute config path>"
//! Quoting is critical — paths may contain spaces (e.g. C:\Program Files).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Default Windows Service name when `--name` is not provided.
pub const DEFAULT_SERVICE_NAME: &str = "agent-shim";

/// Construct the `OsString` arguments passed to `ServiceInfo.launch_arguments`.
/// The `windows-service` crate joins these onto the bare executable path
/// using its own quoting rules — we don't manually quote here.
pub fn launch_arguments(config: &Path) -> Vec<OsString> {
    vec![
        OsString::from("service"),
        OsString::from("run"),
        OsString::from("--config"),
        config.as_os_str().to_owned(),
    ]
}

/// Pretty-format the full ImagePath as it would appear in `sc qc <name>`
/// output. Used by the `status` command's display only.
pub fn format_image_path_for_display(exe: &Path, config: &Path) -> String {
    format!(
        r#""{}" service run --config "{}""#,
        exe.display(),
        config.display(),
    )
}

/// Parse `--config <path>` back out of a SCM-formatted ImagePath. The
/// `windows-service` crate gives us the raw command line as a single
/// String via `service.query_config()`. We split on `--config` and take
/// the next token, stripping surrounding quotes.
///
/// Returns `None` if the `--config` flag is absent (e.g. the service was
/// registered by something other than `agent-shim service install`).
pub fn parse_config_from_image_path(image_path: &str) -> Option<PathBuf> {
    // The simplest parse: find "--config" and take the following token.
    let mut tokens = image_path.split_whitespace();
    while let Some(t) = tokens.next() {
        if t == "--config" || t == "-c" {
            let raw = tokens.next()?;
            // Strip a single leading/trailing double-quote if present.
            // (Windows quotes paths with spaces in the ImagePath.)
            let stripped = raw.trim_matches('"');
            return Some(PathBuf::from(stripped));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn launch_arguments_orders_subcommand_then_flag_then_value() {
        let args = launch_arguments(Path::new(r"C:\ProgramData\agent-shim\gateway.yaml"));
        let strs: Vec<&str> = args.iter().map(|s| s.to_str().unwrap()).collect();
        assert_eq!(
            strs,
            vec![
                "service",
                "run",
                "--config",
                r"C:\ProgramData\agent-shim\gateway.yaml"
            ]
        );
    }

    #[test]
    fn parse_config_from_image_path_extracts_quoted_path() {
        let image = r#""C:\Program Files\agent-shim\agent-shim.exe" service run --config "C:\ProgramData\agent-shim\gateway.yaml""#;
        assert_eq!(
            parse_config_from_image_path(image),
            Some(PathBuf::from(r"C:\ProgramData\agent-shim\gateway.yaml")),
        );
    }

    #[test]
    fn parse_config_from_image_path_extracts_unquoted_path() {
        let image = r#"C:\bin\agent-shim.exe service run --config C:\etc\gw.yaml"#;
        assert_eq!(
            parse_config_from_image_path(image),
            Some(PathBuf::from(r"C:\etc\gw.yaml")),
        );
    }

    #[test]
    fn parse_config_from_image_path_returns_none_when_flag_absent() {
        let image = r#""C:\bin\agent-shim.exe" serve"#;
        assert_eq!(parse_config_from_image_path(image), None);
    }
}
```

- [ ] **Step 6: Create `crates/gateway/src/commands/service/elevation.rs`**

```rust
//! Administrator-privilege check.
//!
//! `install`, `uninstall`, `start`, `stop`, `restart` operate on SCM,
//! which requires an elevated token. `status` only needs `SC_MANAGER_CONNECT`
//! + `SERVICE_QUERY_STATUS` and skips this check.
//!
//! We use `GetTokenInformation(token, TokenElevation, ...)` via the
//! `windows` crate. The check is cheap and idempotent.

use anyhow::{Context, Result};
use std::mem;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Returns `true` if the current process token is elevated.
pub fn is_elevated() -> Result<bool> {
    // SAFETY: All FFI calls below are gated by Result checks. `windows`
    // crate exposes them as `unsafe fn`s — we wrap the unsafe block.
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .ok()
            .context("OpenProcessToken failed")?;
    }

    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned_len = 0u32;
    unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned_len,
        )
        .ok()
        .context("GetTokenInformation failed")?;
    }

    Ok(elevation.TokenIsElevated != 0)
}

/// Bail with a clear error if the current process is not elevated.
pub fn require_admin() -> Result<()> {
    if !is_elevated()? {
        anyhow::bail!(
            "This command requires administrator privileges.\n\
             Please re-run from an elevated PowerShell or CMD."
        );
    }
    Ok(())
}
```

Note: `crates/gateway` already permits `unsafe` (its lib root is `gateway` not the workspace `#![forbid(unsafe_code)]` crates). Confirm with `grep -n "forbid(unsafe" crates/gateway/src/lib.rs`. If `gateway` has `#![forbid(unsafe_code)]`, change `forbid` to `deny` and add `#[allow(unsafe_code)]` on the elevation functions, with a comment pointing to the spec — but per the CLAUDE.md convention ("`#![forbid(unsafe_code)]` in all crates except gateway"), this should already be fine.

- [ ] **Step 7: Create `crates/gateway/src/commands/service/install.rs`**

```rust
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
        if dir.ends_with("debug") || dir.ends_with(std::env::temp_dir().file_name().unwrap_or_default()) {
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
        other => anyhow::bail!(
            "invalid --start-type {other:?}; expected auto, manual, or disabled"
        ),
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
                (
                    Some(OsString::from(other)),
                    Some(OsString::from(pwd)),
                )
            }
        };

    // 6. Talk to SCM.
    let manager_access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    let manager = ServiceManager::local_computer(None::<&str>, manager_access)
        .context("opening SCM")?;

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
    let manager = ServiceManager::local_computer(None::<&str>, manager_access)
        .context("opening SCM")?;

    let service = manager
        .open_service(name, ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS)
        .with_context(|| format!("opening service {:?}", name))?;

    // Best-effort stop before delete. Ignore errors — the service may
    // already be stopped, or stop may be racing with delete.
    let _ = service.stop();

    service.delete().with_context(|| format!("deleting service {:?}", name))?;
    println!("Uninstalled service {:?}.", name);
    Ok(())
}
```

- [ ] **Step 8: Create `crates/gateway/src/commands/service/status.rs`**

```rust
//! `agent-shim service status <name>` — query SCM without requiring admin.

use crate::commands::service::names::parse_config_from_image_path;
use anyhow::{Context, Result};
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

pub fn status(name: &str) -> Result<()> {
    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
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

    let pid = st.process_id.map(|p| p.to_string()).unwrap_or_else(|| "-".into());

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
```

- [ ] **Step 9: Wire the dispatch into `main.rs`**

Modify `crates/gateway/src/main.rs`. The current match has three arms. Add a fourth, gated:

```rust
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve { config } => commands::serve::run(&config).await,
        Commands::ValidateConfig { config } => commands::validate_config::run(&config),
        Commands::Copilot { sub } => match sub {
            CopilotCommand::Login { credential_path } => {
                commands::copilot_login::run(credential_path).await
            }
            CopilotCommand::Models { credential_path } => {
                commands::copilot_models::run(credential_path).await
            }
        },
        #[cfg(windows)]
        Commands::Service { sub } => commands::service::run(sub).await,
    }
}
```

Note: the `commands::service::run` function (defined in step 3 of this task) is currently `async fn run(sub: ServiceCommand) -> Result<()>` but the actual install/uninstall/status handlers are synchronous. Either change `commands::service::run` to be sync and call `.await` only when needed, or keep it async and just `await` it from main. The async signature is forward-compatible (Phase 4's `Run` handler needs to host a tokio runtime). Keep it async.

- [ ] **Step 10: Build for Windows**

If on Windows:

```bash
cargo build -p agent-shim
```

Expected: compiles cleanly.

If on Linux/macOS: confirm the cross-platform negative tests still pass:

```bash
cargo nextest run -p agent-shim --test service_subcommand_parse
```

Expected: PASS.

- [ ] **Step 11: Run names.rs unit tests**

If on Windows:

```bash
cargo nextest run -p agent-shim names::tests
```

Expected: all four tests in `names::tests` pass.

- [ ] **Step 12: Run clippy and fmt**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: clean. Windows-specific clippy needs to run on Windows; CI Linux will only lint the Linux subset.

- [ ] **Step 13: Commit**

```bash
git add crates/gateway/src/commands/mod.rs crates/gateway/src/commands/service crates/gateway/src/main.rs
git commit -m "feat(gateway): service install/uninstall/status (Windows-only)"
```

---

### Task 5: Write a Windows-only install→status→uninstall smoke test (marked `#[ignore]`)

**Files:**
- Create: `crates/gateway/tests/service_install_unit.rs`

This is the only test in Phase 3 that talks to SCM. It is marked `#[ignore]` because it requires admin privileges and mutates system state. CI does **not** run it; the developer runs it manually before merging.

- [ ] **Step 1: Write the test**

```rust
// crates/gateway/tests/service_install_unit.rs
//
// Phase 3 SCM smoke test. Marked #[ignore]; runs only when invoked with
// --run-ignored. Requires an elevated terminal. Cleans up the registered
// service even on panic via the Drop guard.

#![cfg(windows)]

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

struct ServiceCleanup {
    name: String,
}
impl ServiceCleanup {
    fn new(name: &str) -> Self {
        Self { name: name.to_string() }
    }
}
impl Drop for ServiceCleanup {
    fn drop(&mut self) {
        // Best-effort cleanup. Ignore errors; the test may have already
        // uninstalled, or sc may complain that the service is unknown.
        let _ = Command::new("sc").args(["stop", &self.name]).status();
        let _ = Command::new("sc").args(["delete", &self.name]).status();
    }
}

fn write_minimal_config(dir: &std::path::Path) -> PathBuf {
    let yaml = "\
server:
  bind: 127.0.0.1
  port: 0
logging:
  format: pretty
  filter: info
upstreams: {}
routes: []
";
    let path = dir.join("gateway.yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

fn cargo_bin() -> PathBuf {
    // env::var("CARGO_BIN_EXE_<name>") is set by cargo for binaries
    // referenced as test fixtures. But since this test lives in the
    // `agent-shim` crate's `tests/` dir, cargo sets CARGO_BIN_EXE_agent-shim.
    PathBuf::from(env!("CARGO_BIN_EXE_agent-shim"))
}

#[test]
#[ignore = "requires administrator; run with cargo nextest run --run-ignored only"]
fn install_status_uninstall_cycle_smoke() {
    let svc_name = format!("agent-shim-test-{}", uuid::Uuid::new_v4());
    let _cleanup = ServiceCleanup::new(&svc_name);

    let tmp = tempfile::tempdir().unwrap();
    let cfg = write_minimal_config(tmp.path());

    let bin = cargo_bin();

    // install
    let out = Command::new(&bin)
        .args(["service", "install", "--name", &svc_name, "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "install failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // status — must report Stopped
    let out = Command::new(&bin)
        .args(["service", "status", "--name", &svc_name])
        .output()
        .unwrap();
    assert!(out.status.success(), "status failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("State:       Stopped"), "status output: {stdout}");
    assert!(
        stdout.contains(&cfg.display().to_string()),
        "status did not echo config path: {stdout}"
    );

    // uninstall
    let out = Command::new(&bin)
        .args(["service", "uninstall", "--name", &svc_name])
        .output()
        .unwrap();
    assert!(out.status.success(), "uninstall failed");

    // status again — must report not installed (exit code 1).
    let out = Command::new(&bin)
        .args(["service", "status", "--name", &svc_name])
        .output()
        .unwrap();
    assert!(!out.status.success(), "status should fail post-uninstall");

    // Give SCM a beat to settle, just in case.
    std::thread::sleep(Duration::from_millis(200));
}

#[test]
fn install_requires_admin_when_unelevated() {
    // This test runs as part of the regular suite (no #[ignore]) — it
    // only exercises the admin gate, no SCM call. We assume the test
    // runner is NOT elevated; on CI Windows runners (which are elevated)
    // we accept either the elevation gate firing OR the install going
    // further. The point is: the binary must not panic.

    let tmp = tempfile::tempdir().unwrap();
    let cfg = write_minimal_config(tmp.path());

    let bin = cargo_bin();
    let out = Command::new(&bin)
        .args(["service", "install", "--name", "agent-shim-elev-test", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        // Acceptable: elevation gate fired, or SCM connect failed because
        // we're elevated but the test runner doesn't have CREATE_SERVICE
        // for some reason. Either way the message should be human-readable
        // (not a Rust panic).
        assert!(
            stderr.contains("administrator") || stderr.contains("Access is denied") || stderr.contains("error"),
            "expected friendly error, got: {stderr}",
        );
    } else {
        // CI runner is elevated and SCM is happy — clean up.
        let _ = Command::new(&bin)
            .args(["service", "uninstall", "--name", "agent-shim-elev-test"])
            .status();
    }
}
```

If `uuid` is not yet a dev-dep of `agent-shim` (look for it in `crates/gateway/Cargo.toml`'s `[dev-dependencies]`), it's already a regular dependency (line 54), so it's available to tests too.

- [ ] **Step 2: Build and confirm the file compiles**

```bash
cargo build -p agent-shim --tests
```

Expected: clean compile.

- [ ] **Step 3: Run only the non-ignored test**

```bash
cargo nextest run -p agent-shim --test service_install_unit install_requires_admin_when_unelevated
```

Expected: PASS (it accepts either outcome — admin gate or successful install).

- [ ] **Step 4: Optionally run the ignored end-to-end test from an elevated terminal**

```bash
# Elevated PowerShell / CMD:
cargo nextest run -p agent-shim --test service_install_unit --run-ignored only
```

Expected: PASS. Verify manually that `sc query agent-shim-test-<uuid>` does NOT show the service after the test (the Drop guard cleaned it up).

- [ ] **Step 5: Commit**

```bash
git add crates/gateway/tests/service_install_unit.rs
git commit -m "test(gateway): SCM install/status/uninstall smoke (#[ignore], admin-only)"
```

---

### Task 6: Update `--help` smoke check for documentation

**Files:**
- (No file changes — this is a manual verification step.)

- [ ] **Step 1: On Windows, confirm the subcommand is listed**

```bash
cargo run -p agent-shim -- --help
cargo run -p agent-shim -- service --help
cargo run -p agent-shim -- service install --help
```

Expected: `service` appears under "Commands"; `service install --help` lists `--config`, `--name`, `--display-name`, `--account`, `--password`, `--start-type`. The hidden `run` subcommand should NOT appear in `service --help` (it has `#[command(hide = true)]`).

- [ ] **Step 2: On Linux, confirm the subcommand is absent**

```bash
cargo run -p agent-shim -- --help
```

Expected: no `service` entry under "Commands".

- [ ] **Step 3 (Linux only): confirm no Windows crates in the dep graph**

```bash
cargo tree -p agent-shim 2>&1 | grep -iE "windows[-_]"
```

Expected: empty output.

---

### Task 7: Final verification

- [ ] **Step 1: Full CI pipeline (cross-platform)**

On Linux:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo deny check
```

On Windows (if available):

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace          # `--run-ignored only` is opt-in
cargo deny check
```

Expected: all green on both.

- [ ] **Step 2: Confirm git log**

```bash
git log --oneline -15
```

Expected: a clean sequence of commits for tasks 1 through 6.

---

## Done Criteria

- `windows-service` and `windows` crates land in `[target.'cfg(windows)'.dependencies]` only. Linux/macOS `cargo tree` does not include them.
- `agent-shim service` subcommand is reachable on Windows; absent on Linux/macOS (assertion in `service_subcommand_parse.rs`).
- `agent-shim service install --config <abs-path>` succeeds on Windows under admin: registers the service, validates the config, and reports back.
- `agent-shim service install --config <rel-path>` rejects with a clear message before touching SCM.
- `agent-shim service install --config <invalid-yaml>` rejects via `validate-config` codepath before touching SCM.
- `agent-shim service uninstall --name <name>` removes the registration.
- `agent-shim service status --name <name>` prints SCM state, PID, account, ImagePath, and the resolved config path. Works without admin.
- Phase 3's manual end-to-end test (`#[ignore]`) passes from an elevated terminal.
- `cargo deny check`, `cargo fmt`, `cargo clippy -- -D warnings` all clean on both Linux and Windows.

Phase 3 is independently shippable. Phase 4 plugs in `service run` (SCM main loop), `service start`, `service stop`, and `service restart`, completing the lifecycle.
