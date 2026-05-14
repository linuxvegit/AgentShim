# Phase 1: Refactor `commands::serve::run` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract a reusable `run_core` function from `commands::serve::run` so both the foreground `serve` command and the future Windows Service SCM entry point can share the same startup, listening, and shutdown logic without duplication.

**Architecture:** `run_core(cfg, config_path, shutdown_signal, on_listening)` becomes the single place that builds `AppState`, spawns the reload-applying task and (on Unix) SIGHUP listener, builds the router, binds the listener, calls back `on_listening(addr)`, and serves until `shutdown_signal` resolves. The existing public `run(config_path)` becomes a thin wrapper that loads/validates config and calls `run_core` with the current foreground defaults (no-op `on_listening`, OS-signal-based shutdown). Behavior is preserved exactly — this is a pure refactor.

**Tech Stack:** Rust, tokio, axum, existing crates (no new dependencies).

**Spec reference:** `docs/superpowers/specs/2026-05-14-windows-service-and-file-logging-design.md` sections 4.5, 4.6, 11 (phase 1).

---

## File Structure

| File | Responsibility | Status |
|------|----------------|--------|
| `crates/gateway/src/commands/serve.rs` | Hosts both `run` (foreground wrapper) and `run_core` (shared core); keeps the existing `handle_reload` function unchanged | Modify |
| `crates/gateway/src/server.rs` | Add a third helper `run_with_admin_on_listeners` so `run_core` can hand pre-bound listeners + a custom shutdown future to the existing dual-listener path; existing `run`/`run_with_admin` retain their behavior for callers we haven't migrated | Modify |
| `crates/gateway/tests/serve_core_smoke.rs` | New integration test that drives `run_core` with custom shutdown + on_listening callbacks and asserts both fire | Create |

---

### Task 1: Add a regression test that captures current foreground startup behavior

**Files:**
- Create: `crates/gateway/tests/serve_smoke_pre_refactor.rs`

Rationale: before any refactor we capture the observable behavior we must not break. The test boots the gateway through `run_on_listener` (existing public API), sends a request to the root `/` endpoint, and shuts it down. This test must pass before AND after the refactor — it is the regression net.

- [ ] **Step 1: Write the test**

```rust
// crates/gateway/tests/serve_smoke_pre_refactor.rs
use agent_shim_config::GatewayConfig;
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use std::collections::BTreeMap;
use tokio::net::TcpListener;

fn minimal_config() -> GatewayConfig {
    use agent_shim_config::schema::{LoggingConfig, ServerConfig};
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

#[tokio::test]
async fn root_endpoint_serves_ok_before_refactor() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (state, _reload_rx) = AppState::new(minimal_config()).await;

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        run_on_listener(listener, state, async { let _ = rx.await; })
            .await
            .unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let resp = reqwest::get(format!("http://{addr}/")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");

    let _ = tx.send(());
    server.await.unwrap();
}
```

- [ ] **Step 2: Run the test to verify it passes against current `master`**

Run: `cargo nextest run -p agent-shim --test serve_smoke_pre_refactor`
Expected: PASS (we're testing the existing path)

- [ ] **Step 3: Commit**

```bash
git add crates/gateway/tests/serve_smoke_pre_refactor.rs
git commit -m "test(gateway): pin foreground serve smoke before serve::run refactor"
```

---

### Task 2: Add a `run_core` test that exercises `on_listening` + custom shutdown

**Files:**
- Create: `crates/gateway/tests/serve_core_smoke.rs`

This test is the new behavior we want. It calls `run_core` (does not exist yet) with a custom shutdown signal (a tokio `oneshot`) and a custom `on_listening` callback that records the bound `SocketAddr`. The test then connects to the recorded addr and verifies the server responds. **This test should fail to compile right now** because `run_core` doesn't exist yet — that's the RED in our TDD cycle.

- [ ] **Step 1: Write the failing test**

```rust
// crates/gateway/tests/serve_core_smoke.rs
//
// New integration test for the run_core refactor. Drives run_core directly
// with a custom shutdown future and on_listening callback, verifies both
// fire correctly.

use agent_shim_config::schema::{LoggingConfig, ServerConfig};
use agent_shim_config::GatewayConfig;
use agent_shim_gateway::commands::serve::run_core;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

fn ephemeral_config() -> GatewayConfig {
    GatewayConfig {
        // port: 0 → OS picks a free port at bind time
        server: ServerConfig { bind: "127.0.0.1".into(), port: 0, keepalive_secs: 60 },
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

#[tokio::test]
async fn run_core_invokes_on_listening_and_respects_custom_shutdown() {
    let cfg = ephemeral_config();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let bound_addr: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));

    let bound_addr_clone = bound_addr.clone();
    let shutdown_fut = async move { let _ = shutdown_rx.await; };
    let on_listening = move |addr: SocketAddr| {
        *bound_addr_clone.lock().unwrap() = Some(addr);
    };

    let server_task = tokio::spawn(async move {
        run_core(cfg, None, shutdown_fut, on_listening).await
    });

    // Poll for the listening address (on_listening should fire within a few
    // milliseconds of binding).
    let addr = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        async {
            loop {
                if let Some(addr) = *bound_addr.lock().unwrap() {
                    return addr;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        },
    )
    .await
    .expect("on_listening never fired");

    // Hit the root endpoint to confirm the server is live.
    let resp = reqwest::get(format!("http://{addr}/")).await.unwrap();
    assert_eq!(resp.status(), 200);

    // Trigger graceful shutdown via our custom signal.
    let _ = shutdown_tx.send(());

    // Server should return Ok within shutdown grace window.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        server_task,
    )
    .await
    .expect("server did not shut down within 5s")
    .expect("server task panicked");

    result.expect("run_core returned an error");
}
```

- [ ] **Step 2: Run the test to verify it fails to compile**

Run: `cargo nextest run -p agent-shim --test serve_core_smoke`
Expected: COMPILE ERROR — `run_core` is not a public item of `agent_shim_gateway::commands::serve` (it doesn't exist yet).

- [ ] **Step 3: Commit the failing test**

```bash
git add crates/gateway/tests/serve_core_smoke.rs
git commit -m "test(gateway): failing test for run_core refactor (TDD red)"
```

---

### Task 3: Make the `commands` module public so the test can reach `run_core`

**Files:**
- Modify: `crates/gateway/src/lib.rs`

Currently `commands` is private (only `main.rs` uses it). The integration test needs `pub use` for `run_core`. The minimal change is to make the `commands::serve` module `pub` through the library.

- [ ] **Step 1: Read the current `lib.rs`**

Run: `cat crates/gateway/src/lib.rs`
Expected: lists the modules currently exported.

- [ ] **Step 2: Edit `crates/gateway/src/lib.rs` to expose `commands::serve`**

Find the block where modules are declared. Add or modify:

```rust
pub mod commands {
    pub mod serve;
    // The remaining subcommands (copilot_login, copilot_models, validate_config)
    // stay internal — they're only invoked from main.rs via the cli enum dispatch.
}
```

If `commands` is already declared in `lib.rs` (vs only in `main.rs`), modify it; if only `main.rs` has `mod commands;`, ADD the block above to `lib.rs`, leaving `main.rs` to keep its own `mod commands;` for the binary-only submodules. The `pub mod commands { pub mod serve; }` form works because Rust allows the same crate to re-declare modules in `lib.rs` and `main.rs` independently (they're separate crate roots).

If the existing `main.rs` declares `mod commands;` and includes `pub mod serve;` inside `commands/mod.rs`, then the simpler fix is to add a single line to `lib.rs`:

```rust
pub mod commands;
```

and ensure `crates/gateway/src/commands/mod.rs` exists and re-exports the submodules. Check by running:

```bash
ls crates/gateway/src/commands/
cat crates/gateway/src/commands/mod.rs
```

Choose whichever variant matches the existing layout. The end state must be: `agent_shim_gateway::commands::serve::run_core` is reachable from integration tests.

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p agent-shim`
Expected: success (no `run_core` reference yet, just the module exposure).

- [ ] **Step 4: Commit**

```bash
git add crates/gateway/src/lib.rs crates/gateway/src/commands/mod.rs
git commit -m "refactor(gateway): expose commands::serve from lib for integration tests"
```

Note: only stage `commands/mod.rs` if you modified it.

---

### Task 4: Introduce `run_core` with signature, no behavior change yet

**Files:**
- Modify: `crates/gateway/src/commands/serve.rs`

We add the function with the new signature but keep the existing `run` implementation untouched. `run_core` will be empty initially (`unimplemented!()`) so the integration test compiles but fails at runtime. This makes the next task — moving logic into it — into a clean diff.

- [ ] **Step 1: Add `run_core` skeleton at the top of `serve.rs`**

Find the existing `pub async fn run(config_path: &Path) -> Result<()>` function (around line 5). Above it, insert:

```rust
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use agent_shim_config::GatewayConfig;

/// Phase 1 P01: shared core of the foreground `serve` command and the
/// (future) Windows Service SCM entry point. Builds the application state,
/// spawns the reload-applying task and (on Unix) the SIGHUP listener,
/// binds the listener(s), invokes `on_listening` with the bound public
/// address once it is accepting connections, then serves until
/// `shutdown_signal` resolves.
///
/// Callers that load a config from disk should pass `config_path =
/// Some(path)` so `AppCore::config_path` is populated and SIGHUP /
/// `POST /admin/reload` (with no body) can re-read the original file.
/// Callers constructing a config in memory (tests, future programmatic
/// embedding) pass `None`.
pub async fn run_core<S, L>(
    cfg: GatewayConfig,
    config_path: Option<PathBuf>,
    shutdown_signal: S,
    on_listening: L,
) -> anyhow::Result<()>
where
    S: Future<Output = ()> + Send + 'static,
    L: FnOnce(SocketAddr) + Send + 'static,
{
    let _ = (cfg, config_path, shutdown_signal, on_listening);
    unimplemented!("run_core body lands in next task")
}
```

- [ ] **Step 2: Verify compilation and the new integration test still fails (now at runtime, not compile time)**

Run: `cargo build -p agent-shim --tests`
Expected: compiles cleanly.

Run: `cargo nextest run -p agent-shim --test serve_core_smoke`
Expected: test panics with `not implemented: run_core body lands in next task` — proving the test reaches the function.

- [ ] **Step 3: Commit**

```bash
git add crates/gateway/src/commands/serve.rs
git commit -m "refactor(gateway): introduce run_core signature (body in next commit)"
```

---

### Task 5: Move the body of `run` into `run_core`, replace `run` with a thin wrapper

**Files:**
- Modify: `crates/gateway/src/commands/serve.rs`
- Modify: `crates/gateway/src/server.rs` — add `run_with_admin_on_listeners` helper

This is the largest task. We migrate the body of the existing `run` function (everything between line 5 and line 83 of the current `serve.rs`) into `run_core`, replacing the calls to `crate::server::run` / `crate::server::run_with_admin` with versions that accept a custom shutdown future and pre-bound listeners.

- [ ] **Step 1: Add `run_on_listener_with_shutdown` and `run_with_admin_on_listeners` to `server.rs`**

`server.rs` already has `run_on_listener(listener, state, shutdown)` — that's exactly the public listener path for the no-admin case. We need a sibling for the dual-listener path. Find the end of `run_with_admin` (around line 88) and after it add:

```rust
/// Phase 1 P01: dual-listener variant accepting pre-bound listeners and a
/// shared shutdown future. Mirrors `run_with_admin` but lets `run_core`
/// invoke `on_listening` immediately after the public listener is bound.
pub async fn run_with_admin_on_listeners(
    public_listener: tokio::net::TcpListener,
    admin_listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let public_app = build_router(state.clone());
    let admin_app = crate::admin::build_router(state);

    let token = tokio_util::sync::CancellationToken::new();
    {
        let token = token.clone();
        let signal_task: tokio::task::JoinHandle<()> = tokio::spawn(async move {
            shutdown.await;
            token.cancel();
        });
        drop(signal_task);
    }

    let public_shutdown = {
        let token = token.clone();
        async move { token.cancelled().await }
    };
    let admin_shutdown = {
        let token = token.clone();
        async move { token.cancelled().await }
    };

    tokio::select! {
        res = axum::serve(public_listener, public_app)
            .with_graceful_shutdown(public_shutdown) => res?,
        res = axum::serve(admin_listener, admin_app)
            .with_graceful_shutdown(admin_shutdown) => res?,
    }
    Ok(())
}
```

- [ ] **Step 2: Rewrite the body of `run_core` in `commands/serve.rs`**

Replace the `unimplemented!()` placeholder. The new body mirrors the existing `run` function but with custom shutdown + on_listening hooks:

```rust
pub async fn run_core<S, L>(
    cfg: GatewayConfig,
    config_path: Option<PathBuf>,
    shutdown_signal: S,
    on_listening: L,
) -> anyhow::Result<()>
where
    S: Future<Output = ()> + Send + 'static,
    L: FnOnce(SocketAddr) + Send + 'static,
{
    agent_shim_config::validate(&cfg)
        .map_err(|e| anyhow::anyhow!("Config validation failed: {}", e))?;

    let tracing_handles = agent_shim_observability::init(&cfg.logging, cfg.otel.as_ref());
    let (mut state, mut reload_rx) = crate::state::AppState::new(cfg).await;

    // Pin the config path onto AppCore so SIGHUP / POST /admin/reload
    // (with no body) can re-read the original file. AppState::new builds
    // AppCore without a path; we fork a fresh one with the path filled in.
    let core_with_path = std::sync::Arc::new(crate::state::AppCore {
        config_path: config_path.clone(),
        ..(*state.core).clone()
    });
    state.core = core_with_path;

    // Reload-applying task: serializes all ArcSwap swaps.
    {
        let state_for_task = state.clone();
        tokio::spawn(async move {
            while let Some(req) = reload_rx.recv().await {
                let outcome = handle_reload(&state_for_task, req.source).await;
                let _ = req.respond_to.send(outcome);
            }
        });
    }

    // SIGHUP listener (Unix only). Each SIGHUP enqueues a
    // ReloadSource::Sighup so reload semantics match POST /admin/reload
    // with no body.
    #[cfg(unix)]
    {
        let reload_tx = state.core.reload_tx.clone();
        tokio::spawn(async move {
            let mut sig = match tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::hangup(),
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "failed to install SIGHUP handler");
                    return;
                }
            };
            while sig.recv().await.is_some() {
                let (tx, _rx) = tokio::sync::oneshot::channel();
                let _ = reload_tx
                    .send(crate::reload_trigger::ReloadRequest {
                        source: crate::reload_trigger::ReloadSource::Sighup,
                        respond_to: tx,
                    })
                    .await;
                tracing::info!(
                    target = "agent_shim::reload",
                    "SIGHUP received; reload requested"
                );
            }
        });
    }

    // Bind listener(s) eagerly so `on_listening` can fire with the real
    // bound address before we hand control to axum::serve.
    let public_bind: SocketAddr = format!(
        "{}:{}",
        state.core.server_config.bind, state.core.server_config.port
    )
    .parse()?;
    let public_listener = tokio::net::TcpListener::bind(public_bind).await?;
    let public_local = public_listener.local_addr()?;
    tracing::info!("Listening on {} (public)", public_local);
    on_listening(public_local);

    let result = if let Some(admin_cfg) = state.core.admin_config.clone() {
        let admin_bind: SocketAddr =
            format!("{}:{}", admin_cfg.bind, admin_cfg.port).parse()?;
        let admin_listener = tokio::net::TcpListener::bind(admin_bind).await?;
        tracing::info!("Listening on {} (admin)", admin_listener.local_addr()?);
        crate::server::run_with_admin_on_listeners(
            public_listener,
            admin_listener,
            state,
            shutdown_signal,
        )
        .await
    } else {
        crate::server::run_on_listener(public_listener, state, shutdown_signal).await
    };

    if let Some(otel) = tracing_handles.otel {
        otel.shutdown();
    }
    result
}
```

- [ ] **Step 3: Replace the body of the existing `run` function with a thin wrapper**

The current `pub async fn run(config_path: &Path) -> Result<()>` becomes:

```rust
pub async fn run(config_path: &Path) -> anyhow::Result<()> {
    let cfg = agent_shim_config::load_from_path(config_path)
        .map_err(|e| anyhow::anyhow!("Failed to load config: {}", e))?;
    run_core(
        cfg,
        Some(config_path.to_path_buf()),
        crate::shutdown::shutdown_signal(),
        |_addr| {}, // foreground: no-op on listening
    )
    .await
}
```

Delete the duplicated logic that previously lived in `run`. Keep `handle_reload` untouched — it stays a free function in this module.

- [ ] **Step 4: Run both integration tests**

Run: `cargo nextest run -p agent-shim --test serve_core_smoke --test serve_smoke_pre_refactor`
Expected: BOTH pass.

- [ ] **Step 5: Run the full test suite to confirm no regressions**

Run: `cargo nextest run --workspace`
Expected: all green.

- [ ] **Step 6: Run clippy and fmt**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/gateway/src/commands/serve.rs crates/gateway/src/server.rs
git commit -m "refactor(gateway): move serve::run body into reusable run_core (TDD green)"
```

---

### Task 6: Delete the pre-refactor regression test (it is now duplicate)

**Files:**
- Delete: `crates/gateway/tests/serve_smoke_pre_refactor.rs`

The new `serve_core_smoke.rs` covers strictly more behavior (it asserts `on_listening` and custom shutdown). Keeping both is just maintenance overhead.

- [ ] **Step 1: Delete the file**

```bash
rm crates/gateway/tests/serve_smoke_pre_refactor.rs
```

- [ ] **Step 2: Confirm test suite still passes**

Run: `cargo nextest run --workspace`
Expected: all green.

- [ ] **Step 3: Commit**

```bash
git add crates/gateway/tests/serve_smoke_pre_refactor.rs
git commit -m "test(gateway): drop pre-refactor smoke (superseded by serve_core_smoke)"
```

---

### Task 7: Verify deny check and final clean build

- [ ] **Step 1: Run `cargo deny check`**

Run: `cargo deny check`
Expected: PASS (no new dependencies were introduced in this phase).

- [ ] **Step 2: Run the full build + test + lint pipeline as CI would**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

Expected: all green.

- [ ] **Step 3: Confirm git log shows the refactor as a clean sequence**

Run: `git log --oneline -10`
Expected: visible commits in the right order, no merge artifacts.

---

## Done Criteria

- `run_core(cfg, config_path, shutdown, on_listening)` exists in `commands::serve` and is reachable from integration tests.
- `commands::serve::run(config_path)` is implemented as a thin wrapper around `run_core`.
- `crates/gateway/tests/serve_core_smoke.rs` passes, asserting both the `on_listening` callback and custom shutdown future fire correctly.
- All existing tests pass without modification.
- No new dependencies introduced.
- `cargo deny check`, `cargo fmt --check`, and `cargo clippy -- -D warnings` are clean.

Phase 1 is now complete and ready to be merged independently. Phase 2 builds the file-logging feature on top of this refactor; Phase 4 builds the Windows Service SCM entry on top of `run_core`.
