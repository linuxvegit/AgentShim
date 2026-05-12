# Plan 01 — Admin Listener + AppState Pivot (Phase 5)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-09-phase-5-observability-design.md`](../specs/2026-05-09-phase-5-observability-design.md) (decisions D3, D7, D10; §2.2 AppCore/AppSnapshot split; §2.3 Admin listener).

**Goal:** Stand up a separate admin listener (default `127.0.0.1:9100`, disabled when absent), move `/healthz` off the public port, add `/readyz`, and pivot `AppState` to the `Arc<AppCore> + Arc<ArcSwap<AppSnapshot>>` shape that Plans 02–04 require. **No metrics, no OTel, no reload yet** — just the foundation that lets the next three plans add their handlers cleanly.

**Architecture:** A new `admin` block in `GatewayConfig`. When present, `commands::serve::run` builds and binds a second `axum::Router` exposing only admin endpoints. The current `AppState` struct stays as the type handlers consume (so Plans 02–04 don't need to change handler signatures), but its fields are reorganized into `core: Arc<AppCore>` (immutable for the process lifetime) and `snapshot: Arc<ArcSwap<AppSnapshot>>` (hot-swappable). All Phase 4 fields move into one of those two halves; nothing is dropped. Hot-swap MACHINERY exists but no swap actually happens in P01 — Plan 04 wires the trigger.

**Tech stack:** Adds `arc-swap = "1"` to the `crates/gateway/Cargo.toml` and workspace `[workspace.dependencies]`. No new infrastructure crates.

**Frontend changes:** NONE.
**Provider code changes:** NONE.
**Core changes:** NONE.

**Test target:** baseline 585 → 591 (+6).

---

## File Structure

`crates/config/src/`:
- Modify: `schema.rs` — add `AdminConfig` struct, add `admin: Option<AdminConfig>` to `GatewayConfig`.
- Modify: `validation.rs` — extend startup `validate()` to reject admin port collision with `server.port` or `0`.

`crates/gateway/src/`:
- Create: `admin/mod.rs` — `pub fn build_router(state: AppState) -> axum::Router` and `pub async fn run(listener, state, shutdown) -> Result<()>`.
- Create: `admin/handlers.rs` — `healthz`, `readyz` handlers (move `healthz` here from `server.rs`).
- Modify: `state.rs` — split current fields into `AppCore` + `AppSnapshot` newtypes; `AppState` becomes a thin wrapper holding both via `Arc<AppCore>` and `Arc<ArcSwap<AppSnapshot>>`.
- Modify: `server.rs` — drop the `healthz` handler (moved); leave a non-admin `/` route returning the same `"ok"` for backwards compat (curl-without-config probes).
- Modify: `commands/serve.rs` — when `state.core.admin_config.is_some()`, build the admin listener and `tokio::select!` it alongside the public listener.
- Modify: `main.rs` — add `mod admin;`.

`crates/gateway/Cargo.toml`:
- Add `arc-swap.workspace = true` to `[dependencies]`.

`Cargo.toml` (workspace):
- Add `arc-swap = "1"` to `[workspace.dependencies]`.

`crates/gateway/tests/`:
- Create: `admin_port.rs` — integration tests for admin listener on/off, readyz, healthz move.

---

## Tasks

### Task 1: AdminConfig schema + workspace dep

**Files:**
- Modify: `crates/config/src/schema.rs`
- Modify: `crates/config/src/validation.rs`
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/gateway/Cargo.toml`

- [ ] **Step 1: Add `arc-swap` to the workspace**

Edit the workspace root `Cargo.toml`. Find the `[workspace.dependencies]` block and add the line in alphabetical order (after `anyhow`):

```toml
arc-swap = "1"
```

- [ ] **Step 2: Wire it into the gateway crate**

Edit `crates/gateway/Cargo.toml`. Find the `[dependencies]` block and add:

```toml
arc-swap.workspace = true
```

- [ ] **Step 3: Write failing schema test**

Add to the bottom of the existing `mod tests` block in `crates/config/src/schema.rs`:

```rust
#[test]
fn admin_config_block_parses() {
    let yaml = r#"
admin:
  bind: 127.0.0.1
  port: 9100
"#;
    let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("parses");
    let admin = cfg.admin.expect("admin block present");
    assert_eq!(admin.bind, "127.0.0.1");
    assert_eq!(admin.port, 9100);
}

#[test]
fn admin_config_absent_means_disabled() {
    let yaml = "server: {bind: 127.0.0.1, port: 8787}";
    let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("parses");
    assert!(cfg.admin.is_none(), "admin must be None when block absent");
}
```

- [ ] **Step 4: Run, expect failure**

```bash
rtk cargo test -p agent-shim-config admin_config --quiet
```

Expected: compile error (`admin` field doesn't exist on `GatewayConfig`).

- [ ] **Step 5: Add `AdminConfig` and the `admin` field**

In `crates/config/src/schema.rs`, after the `ServerConfig` impl block:

```rust
/// Admin/operator HTTP listener — distinct from the request-serving listener
/// (`server.*`). Hosts /metrics, /healthz, /readyz, and /admin/reload.
///
/// **Absent → admin listener is disabled entirely.** No default values are
/// applied; operators opt in by including the `admin:` block in their
/// gateway YAML. This is the conservative default: a fresh v0.5 binary
/// upgraded from v0.4 has no exposed admin surface unless the operator
/// adds the block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    #[serde(default = "default_admin_bind")]
    pub bind: String,
    #[serde(default = "default_admin_port")]
    pub port: u16,
}

fn default_admin_bind() -> String {
    "127.0.0.1".to_string()
}

fn default_admin_port() -> u16 {
    9100
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            bind: default_admin_bind(),
            port: default_admin_port(),
        }
    }
}
```

Then in the `GatewayConfig` struct add the field (after `rate_limit`):

```rust
    pub copilot: Option<CopilotConfig>,
    /// Optional admin listener. When `None`, no admin endpoints are
    /// exposed and no second listener is bound. Plan 01 P01.
    #[serde(default)]
    pub admin: Option<AdminConfig>,
```

(Leave `copilot` where it is; just add `admin` after it.)

- [ ] **Step 6: Re-export from `crates/config/src/lib.rs`**

Find the existing `pub use schema::{ … };` block and add `AdminConfig` to its list (alphabetical, after a sibling type).

- [ ] **Step 7: Run, expect pass**

```bash
rtk cargo test -p agent-shim-config admin_config --quiet
```

Expected: 2 passed.

- [ ] **Step 8: Validation rule — admin port not zero, not equal to server port**

In `crates/config/src/validation.rs`, find the existing `validate()` function. After the existing `server.port == 0` check, append:

```rust
    if let Some(admin) = &cfg.admin {
        if admin.port == 0 {
            return Err(ValidationError::ZeroPort);
        }
        if admin.port == cfg.server.port && admin.bind == cfg.server.bind {
            return Err(ValidationError::InvalidRoute(format!(
                "admin.port {} collides with server.port {} on the same bind {}",
                admin.port, cfg.server.port, admin.bind
            )));
        }
    }
```

- [ ] **Step 9: Add validation tests**

At the end of the `mod tests` block in `validation.rs`:

```rust
#[test]
fn admin_port_zero_rejected() {
    let mut cfg = minimal_cfg();
    cfg.admin = Some(agent_shim_config::AdminConfig { bind: "127.0.0.1".into(), port: 0 });
    assert!(validate(&cfg).is_err());
}

#[test]
fn admin_port_equal_to_server_port_rejected() {
    let mut cfg = minimal_cfg();
    cfg.admin = Some(agent_shim_config::AdminConfig {
        bind: cfg.server.bind.clone(),
        port: cfg.server.port,
    });
    assert!(validate(&cfg).is_err());
}

#[test]
fn admin_port_different_bind_or_port_ok() {
    let mut cfg = minimal_cfg();
    cfg.admin = Some(agent_shim_config::AdminConfig {
        bind: "127.0.0.1".into(),
        port: 9100,
    });
    assert!(validate(&cfg).is_ok());
}
```

If a `minimal_cfg()` helper does not already exist in the tests module, add this above the new tests:

```rust
#[cfg(test)]
fn minimal_cfg() -> agent_shim_config::GatewayConfig {
    serde_yaml::from_str("server: {bind: 127.0.0.1, port: 8787}").unwrap()
}
```

- [ ] **Step 10: Run, expect pass**

```bash
rtk cargo test -p agent-shim-config --quiet
```

Expected: all config tests pass (incl. 5 new ones).

- [ ] **Step 11: Commit**

```bash
rtk git add -A
rtk git commit -m "feat(config): AdminConfig block + admin port validation (Plan 01 P01 T1)"
```

---

### Task 2: AppState pivot to AppCore + AppSnapshot

**Files:**
- Modify: `crates/gateway/src/state.rs`

This task does NOT change behavior — every existing field of `AppState` ends up reachable through the new shape. Plans 02–04 will use the split. Tests in this task are construction-time assertions only.

- [ ] **Step 1: Read the existing state.rs to confirm field layout**

```bash
rtk read crates/gateway/src/state.rs
```

The current `AppState` (as of master `8945514`) holds: `config`, `anthropic`, `openai`, `openai_responses`, `providers`, `resolver`, `resilient_caller`, `breaker_registry`, `limiter_registry`, `auth_enabled`, `auth_required`, `configured_key_hashes`. All of these survive the pivot.

- [ ] **Step 2: Write failing test for the new shape**

Add to the bottom of `crates/gateway/src/state.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_yaml() -> &'static str {
        r#"
server: {bind: 127.0.0.1, port: 8787}
upstreams:
  noop:
    type: open_ai_compatible
    base_url: http://localhost:9999/v1
    api_key: dummy
routes:
  - frontend: openai_chat
    model: x
    upstream: noop
    upstream_model: x
"#
    }

    #[tokio::test]
    async fn appstate_split_into_core_and_snapshot() {
        let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        let state = AppState::new(cfg).await;

        // Core holds the immutable handles.
        assert!(state.core.providers.get("noop").is_some());
        let _registry: &Arc<agent_shim_router::BreakerRegistry> = &state.core.breaker_registry;
        assert!(state.core.admin_config.is_none(), "absent admin block → None");

        // Snapshot holds the policy-bearing config.
        let snap = state.snapshot.load_full();
        assert_eq!(snap.auth_required, false);
        assert!(snap.configured_key_hashes.is_empty());
    }

    #[tokio::test]
    async fn appstate_snapshot_arc_clone_is_cheap() {
        // Arc<ArcSwap<AppSnapshot>> readers must be lock-free.
        let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        let state = AppState::new(cfg).await;
        let snap1 = state.snapshot.load_full();
        let snap2 = state.snapshot.load_full();
        // Same underlying allocation — no swap occurred.
        assert!(Arc::ptr_eq(&snap1, &snap2));
    }
}
```

- [ ] **Step 3: Run, expect failure**

```bash
rtk cargo test -p agent-shim --quiet appstate_split
```

Expected: compile error (`state.core` doesn't exist).

- [ ] **Step 4: Replace `AppState` with the split shape**

Replace the full `AppState` struct, its `new`, `new_with_clock`, and `build` impls in `crates/gateway/src/state.rs` with:

```rust
use arc_swap::ArcSwap;

/// Immutable handles built once at startup. None of these fields can
/// change without restarting the process. See spec §2.2.
pub struct AppCore {
    pub config_path: Option<std::path::PathBuf>,
    pub server_config: agent_shim_config::ServerConfig,
    pub admin_config: Option<agent_shim_config::AdminConfig>,
    pub anthropic: Arc<AnthropicMessages>,
    pub openai: Arc<OpenAiChat>,
    pub openai_responses: Arc<OpenAiResponses>,
    pub providers: Arc<ProviderRegistry>,
    /// Resolves `(frontend, model_alias)` → `BackendTarget`. Owns both the
    /// static route table and the fuzzy model index as internal seams.
    pub resolver: Arc<ModelResolver>,
    /// Resilience-layer entry point.
    pub resilient_caller: Arc<ResilientCaller>,
    /// Per-(provider, model) circuit breakers. State, not policy — survives
    /// reload (spec §2.2).
    pub breaker_registry: Arc<BreakerRegistry>,
    /// Token-bucket rate limiters. The registry holds buckets across reload;
    /// individual buckets are replaced when policy changes (spec §5.4).
    pub limiter_registry: Arc<LimiterRegistry>,
}

/// Hot-swappable policy-bearing snapshot. Plan 04 will swap this on
/// SIGHUP / POST /admin/reload; in P01 it is built once and never replaced.
/// See spec §2.2.
pub struct AppSnapshot {
    pub config: Arc<agent_shim_config::GatewayConfig>,
    pub auth_enabled: bool,
    pub auth_required: bool,
    pub configured_key_hashes: Arc<HashSet<String>>,
}

#[derive(Clone)]
pub struct AppState {
    pub core: Arc<AppCore>,
    pub snapshot: Arc<ArcSwap<AppSnapshot>>,
}

impl AppState {
    pub async fn new(config: agent_shim_config::GatewayConfig) -> Self {
        Self::build(config, Arc::new(SystemClock)).await
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub async fn new_with_clock(
        config: agent_shim_config::GatewayConfig,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::build(config, clock).await
    }

    async fn build(config: agent_shim_config::GatewayConfig, clock: Arc<dyn Clock>) -> Self {
        let keepalive = Duration::from_secs(config.server.keepalive_secs);
        let anthropic = Arc::new(AnthropicMessages {
            keepalive: Some(keepalive),
        });
        let openai = Arc::new(OpenAiChat {
            keepalive: Some(keepalive),
            clock_override: None,
        });
        let openai_responses = Arc::new(OpenAiResponses {
            keepalive: Some(keepalive),
            clock_override: None,
        });

        // Provider construction (verbatim from the previous AppState::build).
        let mut registry = ProviderRegistry::new();
        for (name, upstream) in &config.upstreams {
            match upstream {
                UpstreamConfig::OpenAiCompatible(cfg) => {
                    match openai_compatible::from_config(name, cfg) {
                        Ok(p) => registry.register(name.clone(), Arc::new(p)),
                        Err(e) => tracing::error!("failed to build provider {name}: {e}"),
                    }
                }
                UpstreamConfig::GithubCopilot => {
                    let credential_path = config
                        .copilot
                        .as_ref()
                        .map(|c| expand_tilde(&c.credential_path))
                        .unwrap_or_else(|| {
                            credential_store::default_path()
                                .unwrap_or_else(|_| PathBuf::from("./copilot.json"))
                        });
                    tracing::info!(path = %credential_path.display(), "copilot credential path");
                    match github_copilot::CopilotProvider::spawn(credential_path) {
                        Ok(p) => registry.register(name.clone(), Arc::new(p)),
                        Err(e) => tracing::error!("failed to build Copilot provider {name}: {e}"),
                    }
                }
                UpstreamConfig::Anthropic(cfg) => match anthropic::from_config(name, cfg) {
                    Ok(p) => registry.register(name.clone(), Arc::new(p)),
                    Err(e) => tracing::error!("failed to build Anthropic provider {name}: {e}"),
                },
                UpstreamConfig::Deepseek(cfg) => match deepseek::from_config(name, cfg) {
                    Ok(p) => registry.register(name.clone(), Arc::new(p)),
                    Err(e) => tracing::error!("failed to build Deepseek provider {name}: {e}"),
                },
                UpstreamConfig::Gemini(cfg) => match gemini::from_config(name, cfg) {
                    Ok(p) => registry.register(name.clone(), Arc::new(p)),
                    Err(e) => tracing::error!("failed to build Gemini provider {name}: {e}"),
                },
            }
        }

        let static_router: Arc<dyn Router> = Arc::new(StaticRouter::from_config(&config));
        let mut discovered = std::collections::HashMap::new();
        for (name, provider) in registry.iter() {
            match provider.list_models().await {
                Ok(Some(models)) => {
                    discovered.insert(name.clone(), models);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(provider = %name, error = %e, "model discovery failed, skipping");
                }
            }
        }
        let model_index = Arc::new(ModelIndex::new(discovered));
        let resolver = Arc::new(ModelResolver::new(static_router, model_index));

        let providers = Arc::new(registry);
        let provider_lookup: Arc<dyn ProviderLookup> = Arc::new(GatewayProviderLookup {
            registry: Arc::clone(&providers),
        });
        let breaker_registry = Arc::new(BreakerRegistry::new(clock));
        let limiter_registry = Arc::new(if config.rate_limit.enabled {
            LimiterRegistry::from_config(&config.rate_limit)
        } else {
            LimiterRegistry::disabled()
        });

        let resilient_caller = Arc::new(ResilientCaller::new(
            provider_lookup,
            Arc::clone(&breaker_registry),
            Arc::clone(&limiter_registry),
        ));

        let auth_enabled = config.auth.enabled;
        let auth_required = config.auth.required;
        let configured_key_hashes: Arc<HashSet<String>> =
            Arc::new(config.auth.keys.keys().cloned().collect());

        let server_config = config.server.clone();
        let admin_config = config.admin.clone();

        let snapshot = AppSnapshot {
            config: Arc::new(config),
            auth_enabled,
            auth_required,
            configured_key_hashes,
        };

        let core = AppCore {
            config_path: None,
            server_config,
            admin_config,
            anthropic,
            openai,
            openai_responses,
            providers,
            resolver,
            resilient_caller,
            breaker_registry,
            limiter_registry,
        };

        Self {
            core: Arc::new(core),
            snapshot: Arc::new(ArcSwap::new(Arc::new(snapshot))),
        }
    }
}
```

- [ ] **Step 5: Migrate every callsite that reads the old fields**

The pivot moves field access from `state.X` to `state.core.X` or `state.snapshot.load_full().X`. Run the build to find every callsite:

```bash
rtk cargo build -p agent-shim 2>&1 | head -80
```

Each compiler error points to a use site. Apply this rule:

| Old (Phase 4)               | New (Phase 5)                                |
| --------------------------- | -------------------------------------------- |
| `state.config.*`            | `state.snapshot.load_full().config.*`        |
| `state.anthropic`           | `state.core.anthropic`                       |
| `state.openai`              | `state.core.openai`                          |
| `state.openai_responses`    | `state.core.openai_responses`                |
| `state.providers`           | `state.core.providers`                       |
| `state.resolver`            | `state.core.resolver`                        |
| `state.resilient_caller`    | `state.core.resilient_caller`                |
| `state.breaker_registry`    | `state.core.breaker_registry`                |
| `state.limiter_registry`    | `state.core.limiter_registry`                |
| `state.auth_enabled`        | snapshot capture, see below                  |
| `state.auth_required`       | snapshot capture, see below                  |
| `state.configured_key_hashes` | snapshot capture, see below                |

For `pipeline::dispatch` specifically, hoist a single snapshot read at the top of the function so subsequent code reads from the snapshot rather than re-loading. Open `crates/gateway/src/pipeline.rs` and find the start of `pub async fn dispatch`. After the `let body_bytes = body.len();` line, insert:

```rust
    // Plan 01 P01 T2: capture the policy snapshot once. In-flight requests
    // use this snapshot for their entire lifetime; mid-request reload (Plan
    // 04) does not affect them.
    let snapshot = state.snapshot.load_full();
```

Then mechanically rewrite uses inside `dispatch`:
- `state.auth_enabled` → `snapshot.auth_enabled`
- `state.auth_required` → `snapshot.auth_required`
- `state.configured_key_hashes` → `&snapshot.configured_key_hashes`
- `state.config` → `snapshot.config`
- `state.resolver` → `state.core.resolver`
- `state.providers` → `state.core.providers`
- `state.resilient_caller` → `state.core.resilient_caller`
- `state.anthropic` / `state.openai` / `state.openai_responses` → `state.core.<frontend>`

For other handlers (e.g. `crates/gateway/src/handlers/anthropic_count_tokens.rs`) that touch these fields, apply the same mapping. Use `rtk grep` to find all of them:

```bash
rtk grep -n "state\.(config|anthropic|openai|openai_responses|providers|resolver|resilient_caller|breaker_registry|limiter_registry|auth_enabled|auth_required|configured_key_hashes)" crates/gateway/src/
```

- [ ] **Step 6: Run the build until clean**

```bash
rtk cargo build --workspace 2>&1 | tail -10
```

Expected: clean build.

- [ ] **Step 7: Run the new tests**

```bash
rtk cargo test -p agent-shim --quiet appstate_split appstate_snapshot
```

Expected: 2 passed.

- [ ] **Step 8: Run the full workspace test suite to confirm no regression**

```bash
rtk cargo test --workspace 2>&1 | grep -E "^test result:" | awk -F'[ ;]' '{sum+=$4} END {print "TOTAL:", sum}'
```

Expected: 587 (baseline 585 + 2 new).

- [ ] **Step 9: Verify frozen-core invariant**

```bash
rtk git diff master -- crates/core/ crates/frontends/ crates/providers/src/
```

Expected: empty output.

- [ ] **Step 10: Commit**

```bash
rtk git add -A
rtk git commit -m "refactor(gateway): pivot AppState to AppCore + ArcSwap<AppSnapshot> (Plan 01 P01 T2)"
```

---

### Task 3: Admin router skeleton + healthz move + readyz

**Files:**
- Create: `crates/gateway/src/admin/mod.rs`
- Create: `crates/gateway/src/admin/handlers.rs`
- Modify: `crates/gateway/src/main.rs`
- Modify: `crates/gateway/src/server.rs`

- [ ] **Step 1: Create the admin module skeleton**

Create `crates/gateway/src/admin/mod.rs`:

```rust
//! Admin HTTP listener — Plan 01 P01 T3.
//!
//! Hosts /healthz, /readyz, and (in later plans) /metrics, /admin/reload.
//! Bound to a separate listener from the public request path so operators
//! can firewall it independently.

use axum::{routing::get, Router};

use crate::state::AppState;

mod handlers;

/// Build the admin Router.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handlers::healthz))
        .route("/readyz", get(handlers::readyz))
        .with_state(state)
}

/// Serve the admin router on `listener` until `shutdown` resolves.
pub async fn run(
    listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let app = build_router(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}
```

- [ ] **Step 2: Create the handlers**

Create `crates/gateway/src/admin/handlers.rs`:

```rust
//! Admin endpoint handlers — Plan 01 P01 T3.

use axum::{extract::State, http::StatusCode, response::IntoResponse};

use crate::state::AppState;

/// Liveness — process is up.
pub async fn healthz() -> &'static str {
    "ok"
}

/// Readiness — config loaded, providers initialized, snapshot populated.
///
/// Returns 200 + "ready" when all three are true. Today (P01) this is
/// equivalent to "the server bound" because AppState construction is
/// the readiness gate; future plans (P02 metrics, P03 OTel) may add
/// additional readiness predicates here.
pub async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    let snap = state.snapshot.load_full();
    let providers_ready = !state.core.providers.is_empty();
    let config_loaded = !snap.config.routes.is_empty() || providers_ready;
    if providers_ready && config_loaded {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}
```

- [ ] **Step 3: Wire `mod admin;` in main.rs**

Edit `crates/gateway/src/main.rs`. Add `mod admin;` to the module declarations (alphabetical order):

```rust
mod admin;
mod cli;
mod commands;
mod handlers;
mod pipeline;
mod server;
mod shutdown;
mod state;
```

- [ ] **Step 4: Drop the duplicated healthz from server.rs**

In `crates/gateway/src/server.rs`, the `async fn healthz()` and the `.route("/healthz", get(healthz))` line both move out. Replace:

```rust
async fn healthz() -> &'static str {
    "ok"
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(healthz))
        .route("/healthz", get(healthz))
        .route("/v1/messages", post(handlers::anthropic_messages::handle))
```

with:

```rust
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { "ok" }))
        .route("/v1/messages", post(handlers::anthropic_messages::handle))
```

The public `/` continues to return `"ok"` for backwards-compat probes. `/healthz` is now admin-only.

- [ ] **Step 5: Run the build**

```bash
rtk cargo build -p agent-shim 2>&1 | tail -5
```

Expected: clean build.

- [ ] **Step 6: Commit**

```bash
rtk git add -A
rtk git commit -m "feat(gateway): admin module + healthz/readyz handlers (Plan 01 P01 T3)"
```

---

### Task 4: Bind the admin listener in commands/serve.rs

**Files:**
- Modify: `crates/gateway/src/commands/serve.rs`
- Modify: `crates/gateway/src/server.rs`

- [ ] **Step 1: Read the current serve command**

```bash
rtk read crates/gateway/src/commands/serve.rs
```

Note the call into `server::run`. The new flow needs to:
- Bind the admin listener if `state.core.admin_config.is_some()`
- `tokio::select!` both listeners with shared graceful shutdown
- If admin is disabled, fall back to the existing single-listener path

- [ ] **Step 2: Refactor `server::run` to expose its bound listener**

In `crates/gateway/src/server.rs`, change `pub async fn run(state: AppState)` so it builds the listener but takes the shutdown future as a parameter. Replace its body with:

```rust
/// Start the public-facing server, binding to the address in
/// `state.core.server_config`.
pub async fn run(state: AppState) -> Result<()> {
    let bind: SocketAddr = format!(
        "{}:{}",
        state.core.server_config.bind, state.core.server_config.port
    )
    .parse()?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!("Listening on {} (public)", bind);
    let app = build_router(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}
```

(Note: `state.config.server.*` becomes `state.core.server_config.*` per Task 2's mapping table.)

- [ ] **Step 3: Add a parallel-listener helper**

Append to `crates/gateway/src/server.rs`:

```rust
/// Run BOTH listeners (public + admin) concurrently with shared graceful
/// shutdown. Used when `state.core.admin_config` is `Some`.
pub async fn run_with_admin(state: AppState) -> Result<()> {
    let public_bind: SocketAddr = format!(
        "{}:{}",
        state.core.server_config.bind, state.core.server_config.port
    )
    .parse()?;
    let admin_cfg = state
        .core
        .admin_config
        .clone()
        .expect("run_with_admin called with admin_config = None");
    let admin_bind: SocketAddr = format!("{}:{}", admin_cfg.bind, admin_cfg.port).parse()?;

    let public_listener = tokio::net::TcpListener::bind(public_bind).await?;
    let admin_listener = tokio::net::TcpListener::bind(admin_bind).await?;
    info!("Listening on {} (public)", public_bind);
    info!("Listening on {} (admin)", admin_bind);

    let public_app = build_router(state.clone());
    let admin_app = crate::admin::build_router(state);

    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
    {
        let s = shutdown.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            s.notify_waiters();
        });
    }

    let public_shutdown = {
        let s = shutdown.clone();
        async move { s.notified().await }
    };
    let admin_shutdown = {
        let s = shutdown.clone();
        async move { s.notified().await }
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

- [ ] **Step 4: Branch in `commands/serve.rs`**

Open `crates/gateway/src/commands/serve.rs`. Find the call site that does `server::run(state).await` and replace it with:

```rust
    if state.core.admin_config.is_some() {
        crate::server::run_with_admin(state).await
    } else {
        crate::server::run(state).await
    }
```

- [ ] **Step 5: Run the build**

```bash
rtk cargo build -p agent-shim 2>&1 | tail -5
```

Expected: clean build.

- [ ] **Step 6: Commit**

```bash
rtk git add -A
rtk git commit -m "feat(gateway): bind admin listener alongside public when configured (Plan 01 P01 T4)"
```

---

### Task 5: Integration tests for admin port

**Files:**
- Create: `crates/gateway/tests/admin_port.rs`

- [ ] **Step 1: Write the integration tests**

Create `crates/gateway/tests/admin_port.rs`:

```rust
//! Plan 01 P01 T5: end-to-end tests for the admin listener.

use std::net::SocketAddr;

async fn pick_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

fn config_yaml(public_port: u16, admin_port: Option<u16>) -> String {
    let admin_block = match admin_port {
        Some(p) => format!("\nadmin: {{bind: 127.0.0.1, port: {p}}}"),
        None => String::new(),
    };
    format!(
        r#"
server: {{bind: 127.0.0.1, port: {public_port}}}
upstreams:
  noop:
    type: open_ai_compatible
    base_url: http://localhost:9999/v1
    api_key: dummy
routes:
  - frontend: openai_chat
    model: x
    upstream: noop
    upstream_model: x
{admin_block}
"#
    )
}

async fn spawn_gateway(yaml: &str) -> SocketAddr {
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let public_addr: SocketAddr =
        format!("{}:{}", cfg.server.bind, cfg.server.port).parse().unwrap();
    let state = agent_shim::state::AppState::new(cfg).await;
    let listener = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let actual = listener.local_addr().unwrap();
    let app = agent_shim::server::build_router(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    actual
}

async fn spawn_gateway_with_admin(yaml: &str) -> (SocketAddr, SocketAddr) {
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let public_addr: SocketAddr =
        format!("{}:{}", cfg.server.bind, cfg.server.port).parse().unwrap();
    let admin_cfg = cfg.admin.clone().expect("admin block required for this helper");
    let admin_addr: SocketAddr =
        format!("{}:{}", admin_cfg.bind, admin_cfg.port).parse().unwrap();
    let state = agent_shim::state::AppState::new(cfg).await;

    let public_listener = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let admin_listener = tokio::net::TcpListener::bind(admin_addr).await.unwrap();
    let public_app = agent_shim::server::build_router(state.clone());
    let admin_app = agent_shim::admin::build_router(state);

    tokio::spawn(async move { let _ = axum::serve(public_listener, public_app).await; });
    tokio::spawn(async move { let _ = axum::serve(admin_listener, admin_app).await; });

    (public_addr, admin_addr)
}

#[tokio::test]
async fn admin_disabled_when_block_absent() {
    let public_port = pick_port().await;
    let yaml = config_yaml(public_port, None);
    let public_addr = spawn_gateway(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Public listener responds at /
    let resp = reqwest::get(format!("http://{}/", public_addr)).await.unwrap();
    assert_eq!(resp.status(), 200);

    // /healthz no longer on the public port (was moved to admin in P01 T3)
    let resp = reqwest::get(format!("http://{}/healthz", public_addr)).await.unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn admin_listener_serves_healthz_when_configured() {
    let public_port = pick_port().await;
    let admin_port = pick_port().await;
    let yaml = config_yaml(public_port, Some(admin_port));
    let (_public, admin_addr) = spawn_gateway_with_admin(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let resp = reqwest::get(format!("http://{}/healthz", admin_addr)).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn admin_listener_serves_readyz_when_configured() {
    let public_port = pick_port().await;
    let admin_port = pick_port().await;
    let yaml = config_yaml(public_port, Some(admin_port));
    let (_public, admin_addr) = spawn_gateway_with_admin(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let resp = reqwest::get(format!("http://{}/readyz", admin_addr)).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ready");
}

#[tokio::test]
async fn admin_listener_does_not_expose_v1_endpoints() {
    let public_port = pick_port().await;
    let admin_port = pick_port().await;
    let yaml = config_yaml(public_port, Some(admin_port));
    let (_public, admin_addr) = spawn_gateway_with_admin(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // /v1/* must not be reachable on admin port (security boundary).
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/messages", admin_addr))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
```

- [ ] **Step 2: Make `state`, `server`, and `admin` modules pub for integration tests**

Integration tests live in `crates/gateway/tests/` and only see `pub` items. Edit `crates/gateway/src/main.rs`:

```rust
pub mod admin;
mod cli;
mod commands;
pub mod handlers;
pub mod pipeline;
pub mod server;
mod shutdown;
pub mod state;
```

(Existing modules `handlers`, `pipeline`, `server`, `state` may already be `pub` — leave them as-is if so.)

The crate also needs a `lib.rs` re-export so `agent_shim::admin::build_router` resolves from integration tests. Check whether `crates/gateway/src/lib.rs` exists:

```bash
ls crates/gateway/src/lib.rs
```

If absent, the binary-only crate won't expose modules. Create `crates/gateway/src/lib.rs`:

```rust
//! Library facade exposing internal modules to integration tests in
//! `crates/gateway/tests/`. The actual binary entrypoint stays in
//! `main.rs`.

pub mod admin;
pub mod handlers;
pub mod pipeline;
pub mod server;
pub mod state;
```

And update `crates/gateway/Cargo.toml` to declare the lib target:

```toml
[lib]
name = "agent_shim"
path = "src/lib.rs"

[[bin]]
name = "agent-shim"
path = "src/main.rs"
```

If `[[bin]]` is already declared, leave it; just add the `[lib]` block above it.

In `main.rs`, replace `mod admin;` (etc.) with `use agent_shim::{admin, handlers, pipeline, server, state};` so binary code uses the same module tree as the lib facade. Keep `mod cli; mod commands; mod shutdown;` private (not used by tests).

- [ ] **Step 3: Add `reqwest` to `[dev-dependencies]`**

If `crates/gateway/Cargo.toml` doesn't already have reqwest as a dev-dep, add to `[dev-dependencies]`:

```toml
reqwest = { workspace = true, features = ["rustls-tls"] }
```

(Phase 4 tests already use reqwest, so it's likely already present — verify by reading the file.)

- [ ] **Step 4: Build, expect compile**

```bash
rtk cargo build -p agent-shim --tests 2>&1 | tail -5
```

Expected: clean build.

- [ ] **Step 5: Run the new tests**

```bash
rtk cargo test -p agent-shim --test admin_port --quiet
```

Expected: 4 passed.

- [ ] **Step 6: Run the full workspace suite**

```bash
rtk cargo test --workspace 2>&1 | grep -E "^test result:" | awk -F'[ ;]' '{sum+=$4} END {print "TOTAL:", sum}'
```

Expected: 591 (baseline 585 + 2 from T2 + 4 from T5).

- [ ] **Step 7: Frozen-core check**

```bash
rtk git diff master -- crates/core/ crates/frontends/ crates/providers/src/
```

Expected: empty.

- [ ] **Step 8: Commit**

```bash
rtk git add -A
rtk git commit -m "test(gateway): admin listener integration coverage (Plan 01 P01 T5)"
```

---

### Task 6: Spec compliance review

**Files:** read-only review against the design spec.

- [ ] **Step 1: Reviewer dispatch**

Spawn a fresh subagent (or self-review if executing inline) with this brief:

> Review commits `<P01 T1..T5 commit range>` against the spec at `docs/superpowers/specs/2026-05-09-phase-5-observability-design.md`. Specifically verify:
> 1. **D3** — admin endpoint is on a separate listener, not the public listener.
> 2. **D7** — `AppState` uses `arc-swap` over `Arc<AppSnapshot>`. The snapshot is captured once per request via `state.snapshot.load_full()` at the top of `pipeline::dispatch`.
> 3. **D10** — when `admin.bind` is omitted, no admin listener is bound (test must prove this — `admin_disabled_when_block_absent`).
> 4. **§2.2 split** — every Phase 4 field of `AppState` lives in either `AppCore` or `AppSnapshot`; no field is dropped. `breaker_registry` is in `core` (state, not policy). `auth_required`/`auth_enabled`/`configured_key_hashes` are in `snapshot` (policy).
> 5. **Frozen core** — `git diff master -- crates/core/ crates/frontends/ crates/providers/src/` is empty.
>
> Report: list each item with PASS / FAIL and a one-line justification. Do not write code; only review.

- [ ] **Step 2: Apply any FAIL findings as fix commits**

For each FAIL, write the smallest diff that addresses it. Commit message: `fix(gateway): <one-line reason> (Plan 01 P01 T6 followup)`.

---

### Task 7: Code quality review

**Files:** read-only review.

- [ ] **Step 1: Reviewer dispatch**

Spawn a fresh subagent with this brief:

> Review commits `<P01 T1..T6 commit range>` for code quality only (don't re-check spec compliance). Specifically:
> 1. The new `AppCore` and `AppSnapshot` types — are they reasonable file-level neighbors? Is there a method that should be a free function or vice versa?
> 2. The `run_with_admin` helper in `server.rs` — is the dual-shutdown plumbing the simplest correct shape? Is `tokio::sync::Notify` the right primitive (vs. `oneshot` channels or a `CancellationToken`)?
> 3. Test isolation in `admin_port.rs` — does `pick_port()` actually prevent races between concurrently-running tests, or is there a TOCTOU window?
> 4. Naming — `AppCore` vs `AppSnapshot` clear at every callsite? Is `core` a better field name than (e.g.) `immutable`?
> 5. Are docs (`/// …` comments) accurate where present?
>
> Report findings as a numbered list. CRITICAL/HIGH must be fixed; MEDIUM/LOW are advisory.

- [ ] **Step 2: Apply CRITICAL/HIGH findings as fix commits**

Each fix is a small commit: `refactor(gateway): <reason> (Plan 01 P01 T7 followup)`.

---

## Done when

- [ ] All workspace tests pass (`cargo test --workspace`); count is 591.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] Frozen-core diff empty against `master`.
- [ ] `admin.bind` absent → admin listener does not bind (proven by `admin_disabled_when_block_absent`).
- [ ] `admin.bind` present → `/healthz` and `/readyz` reachable on admin port; `/v1/*` not reachable on admin port.
- [ ] `pipeline::dispatch` captures one `state.snapshot.load_full()` at the top and reuses it through the request.
- [ ] T6 + T7 reviewer rounds complete with no outstanding CRITICAL/HIGH.
