# Plan 04 — Hot-Reload Config (Phase 5)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-09-phase-5-observability-design.md`](../specs/2026-05-09-phase-5-observability-design.md) (decisions D5, D7, D9; §5 hot-reload config; rules 11-14).

**Goal:** Add a SIGHUP-driven (Unix) and `POST /admin/reload`-driven (cross-platform) reload of routing & policy config. Validates the candidate config against the running `AppCore` (rules 11-14), atomic-swaps the snapshot on success, returns a structured response. **Breaker state survives reload; rate-limit buckets reset on policy change** (the asymmetry from spec §5.4).

**Architecture:** A new `crates/observability/src/reload.rs` module owns the snapshot-rebuild logic (it's policy/state munging, not gateway plumbing). The gateway-side `crates/gateway/src/admin/reload_handler.rs` handles HTTP POST and SIGHUP. The trigger sources route through a single `tokio::sync::mpsc` channel so SIGHUP and POST share the same swap path. Validation rules 11-14 live in `crates/config/src/validation.rs` as a separate `validate_for_reload(candidate, &core_manifest)` function called only by the reload path; startup continues to use `validate(...)`.

**Tech stack:** Adds `serde_yaml` to gateway dev-deps if not present. No new runtime crates — everything is workspace-local.

**Frontend changes:** NONE.
**Provider code changes:** NONE.
**Core changes:** NONE.

**Test target:** 624 → 638 (+14).

---

## File Structure

`crates/config/src/`:
- Modify: `validation.rs` — add `validate_for_reload(candidate: &GatewayConfig, baseline: &ReloadBaseline) -> Result<ReloadDiff, ValidationError>` and a `ReloadBaseline` struct.

`crates/observability/src/`:
- Create: `reload.rs` — `rebuild_snapshot(candidate, &core) -> AppSnapshot`, helper to summarize policy diffs.

`crates/gateway/src/`:
- Create: `admin/reload_handler.rs` — POST /admin/reload handler (with and without body).
- Create: `reload_trigger.rs` — `ReloadTrigger` channel + SIGHUP listener task.
- Modify: `state.rs` — `AppCore` gains `pub config_path: Option<PathBuf>` (already added by P01) and a `pub reload_tx: tokio::sync::mpsc::Sender<ReloadRequest>`.
- Modify: `admin/mod.rs` — `.route("/admin/reload", post(reload_handler::reload))`.
- Modify: `commands/serve.rs` — spawn the SIGHUP listener task and the reload-applying task; both consume from the same channel.

`crates/gateway/tests/`:
- Create: `reload_admin_post.rs` — POST /admin/reload (no body, with body, with invalid YAML, with immutable-field change).
- Create: `reload_sighup.rs` — `#[cfg(unix)]` only; spawns a subprocess and sends SIGHUP.
- Create: `reload_in_flight.rs` — in-flight request keeps old snapshot; breaker state survives reload.

---

## Tasks

### Task 1: Reload validation rules (config crate)

**Files:**
- Modify: `crates/config/src/validation.rs`

- [ ] **Step 1: Define the baseline + diff types**

In `crates/config/src/validation.rs`, after the existing `ValidationError` definitions:

```rust
/// What "baseline" means for reload validation. Built from the running
/// `AppCore` and passed to [`validate_for_reload`]. Spec §5.5 rules
/// 11-14.
#[derive(Debug, Clone)]
pub struct ReloadBaseline {
    /// Names of upstreams declared at startup. Reload may not add or
    /// remove entries (rule 12).
    pub upstream_names: std::collections::BTreeSet<String>,
    /// Server bind/port at startup; immutable across reload (rule 13).
    pub server: crate::schema::ServerConfig,
    /// Admin block at startup; immutable across reload (rule 13).
    pub admin: Option<crate::schema::AdminConfig>,
    /// OTel endpoint at startup; immutable across reload (rule 14).
    pub otel_endpoint: Option<String>,
}

/// Summary returned on successful validation. Used to render the
/// /admin/reload response (spec §5.6).
#[derive(Debug, Clone, Default)]
pub struct ReloadDiff {
    pub routes_total: usize,
    pub routes_added: usize,
    pub routes_removed: usize,
    pub routes_modified: usize,
    pub policies_changed: Vec<String>,
    pub auth_keys_added: usize,
    pub auth_keys_removed: usize,
    pub warnings: Vec<String>,
}

/// New error variants used only by reload validation.
#[derive(Debug, thiserror::Error)]
pub enum ReloadValidationError {
    #[error("upstreams.* set changed: added={added:?}, removed={removed:?}")]
    UpstreamSetChanged {
        added: Vec<String>,
        removed: Vec<String>,
    },
    #[error("immutable field changed: {field}: {old} → {new}")]
    ImmutableFieldChanged {
        field: &'static str,
        old: String,
        new: String,
    },
    #[error("startup validation error: {0}")]
    StartupError(#[from] ValidationError),
}
```

- [ ] **Step 2: Implement `validate_for_reload`**

Append to `validation.rs`:

```rust
/// Reload-time validation. Called BY the reload path only; startup uses
/// [`validate`]. Applies rules 1-10 (via `validate`) plus rules 11-14:
///
/// - Rule 11: every route's upstream is declared in the baseline.
/// - Rule 12: candidate.upstreams keys equal baseline.upstream_names.
/// - Rule 13: server.* and admin.* fields equal baseline values.
/// - Rule 14: otel.endpoint equals baseline.otel_endpoint.
pub fn validate_for_reload(
    candidate: &crate::schema::GatewayConfig,
    baseline: &ReloadBaseline,
) -> Result<ReloadDiff, ReloadValidationError> {
    // Rule 13: immutable server/admin
    if candidate.server.bind != baseline.server.bind {
        return Err(ReloadValidationError::ImmutableFieldChanged {
            field: "server.bind",
            old: baseline.server.bind.clone(),
            new: candidate.server.bind.clone(),
        });
    }
    if candidate.server.port != baseline.server.port {
        return Err(ReloadValidationError::ImmutableFieldChanged {
            field: "server.port",
            old: baseline.server.port.to_string(),
            new: candidate.server.port.to_string(),
        });
    }
    match (&baseline.admin, &candidate.admin) {
        (Some(b), Some(c)) => {
            if b.bind != c.bind {
                return Err(ReloadValidationError::ImmutableFieldChanged {
                    field: "admin.bind",
                    old: b.bind.clone(),
                    new: c.bind.clone(),
                });
            }
            if b.port != c.port {
                return Err(ReloadValidationError::ImmutableFieldChanged {
                    field: "admin.port",
                    old: b.port.to_string(),
                    new: c.port.to_string(),
                });
            }
        }
        (None, Some(_)) => {
            return Err(ReloadValidationError::ImmutableFieldChanged {
                field: "admin",
                old: "absent".into(),
                new: "present".into(),
            });
        }
        (Some(_), None) => {
            return Err(ReloadValidationError::ImmutableFieldChanged {
                field: "admin",
                old: "present".into(),
                new: "absent".into(),
            });
        }
        (None, None) => {}
    }

    // Rule 14: immutable otel.endpoint (other otel.* fields can change)
    let candidate_endpoint = candidate.otel.as_ref().and_then(|o| o.endpoint.clone());
    if candidate_endpoint != baseline.otel_endpoint {
        return Err(ReloadValidationError::ImmutableFieldChanged {
            field: "otel.endpoint",
            old: format!("{:?}", baseline.otel_endpoint),
            new: format!("{:?}", candidate_endpoint),
        });
    }

    // Rule 12: upstream set unchanged
    let candidate_names: std::collections::BTreeSet<String> =
        candidate.upstreams.keys().cloned().collect();
    let added: Vec<String> = candidate_names
        .difference(&baseline.upstream_names)
        .cloned()
        .collect();
    let removed: Vec<String> = baseline
        .upstream_names
        .difference(&candidate_names)
        .cloned()
        .collect();
    if !added.is_empty() || !removed.is_empty() {
        return Err(ReloadValidationError::UpstreamSetChanged { added, removed });
    }

    // Rules 1-10 (and Rule 11 implicitly via the existing
    // `validate_routes`/`UnknownUpstream` check).
    validate(candidate)?;

    // Build a minimal diff. Detailed diffing (which routes were added/
    // removed) is left as a future improvement; for v0.5 we report the
    // route count only.
    let mut diff = ReloadDiff::default();
    diff.routes_total = candidate.routes.len();
    Ok(diff)
}
```

- [ ] **Step 3: Tests**

Append to `crates/config/src/validation.rs::tests`:

```rust
fn baseline_from(cfg: &agent_shim_config::GatewayConfig) -> ReloadBaseline {
    ReloadBaseline {
        upstream_names: cfg.upstreams.keys().cloned().collect(),
        server: cfg.server.clone(),
        admin: cfg.admin.clone(),
        otel_endpoint: cfg.otel.as_ref().and_then(|o| o.endpoint.clone()),
    }
}

#[test]
fn reload_rejects_changed_server_port() {
    let yaml = r#"
server: {bind: 127.0.0.1, port: 8787}
upstreams:
  m: {type: open_ai_compatible, base_url: http://x/v1, api_key: a}
routes:
  - {frontend: openai_chat, model: x, upstream: m, upstream_model: x}
"#;
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let baseline = baseline_from(&cfg);
    let mut candidate = cfg.clone();
    candidate.server.port = 9999;
    let err = validate_for_reload(&candidate, &baseline).unwrap_err();
    assert!(matches!(err, ReloadValidationError::ImmutableFieldChanged { field: "server.port", .. }));
}

#[test]
fn reload_rejects_added_upstream() {
    let yaml = r#"
server: {bind: 127.0.0.1, port: 8787}
upstreams:
  m: {type: open_ai_compatible, base_url: http://x/v1, api_key: a}
routes:
  - {frontend: openai_chat, model: x, upstream: m, upstream_model: x}
"#;
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let baseline = baseline_from(&cfg);
    let mut candidate = cfg.clone();
    candidate.upstreams.insert(
        "n".into(),
        agent_shim_config::UpstreamConfig::OpenAiCompatible(
            agent_shim_config::OpenAiCompatibleUpstream {
                base_url: "http://y/v1".into(),
                api_key: "b".into(),
                default_headers: Default::default(),
                request_timeout_secs: 120,
            }
        ),
    );
    let err = validate_for_reload(&candidate, &baseline).unwrap_err();
    assert!(matches!(err, ReloadValidationError::UpstreamSetChanged { .. }));
}

#[test]
fn reload_accepts_route_change_with_same_upstreams() {
    let yaml = r#"
server: {bind: 127.0.0.1, port: 8787}
upstreams:
  m: {type: open_ai_compatible, base_url: http://x/v1, api_key: a}
routes:
  - {frontend: openai_chat, model: x, upstream: m, upstream_model: x}
"#;
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let baseline = baseline_from(&cfg);
    let mut candidate = cfg.clone();
    // Add a second route alias pointing at the same upstream — fine.
    candidate.routes.push(agent_shim_config::RouteEntry {
        frontend: "openai_chat".into(),
        model: "y".into(),
        upstream: Some("m".into()),
        upstream_model: Some("y-real".into()),
        upstreams: vec![],
        retry: Default::default(),
        breaker: Default::default(),
        reasoning_effort: None,
        anthropic_beta: None,
    });
    let diff = validate_for_reload(&candidate, &baseline).expect("ok");
    assert_eq!(diff.routes_total, 2);
}

#[test]
fn reload_accepts_otel_sample_ratio_change() {
    let yaml = r#"
server: {bind: 127.0.0.1, port: 8787}
otel: {endpoint: "http://c:4317", sample_ratio: 1.0}
upstreams:
  m: {type: open_ai_compatible, base_url: http://x/v1, api_key: a}
routes:
  - {frontend: openai_chat, model: x, upstream: m, upstream_model: x}
"#;
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let baseline = baseline_from(&cfg);
    let mut candidate = cfg.clone();
    candidate.otel.as_mut().unwrap().sample_ratio = 0.5;
    let _diff = validate_for_reload(&candidate, &baseline).expect("ok");
}

#[test]
fn reload_rejects_otel_endpoint_change() {
    let yaml = r#"
server: {bind: 127.0.0.1, port: 8787}
otel: {endpoint: "http://c:4317"}
upstreams:
  m: {type: open_ai_compatible, base_url: http://x/v1, api_key: a}
routes:
  - {frontend: openai_chat, model: x, upstream: m, upstream_model: x}
"#;
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let baseline = baseline_from(&cfg);
    let mut candidate = cfg.clone();
    candidate.otel.as_mut().unwrap().endpoint = Some("http://other:4317".into());
    let err = validate_for_reload(&candidate, &baseline).unwrap_err();
    assert!(matches!(err, ReloadValidationError::ImmutableFieldChanged { field: "otel.endpoint", .. }));
}
```

- [ ] **Step 4: Re-export from lib.rs**

In `crates/config/src/lib.rs`:

```rust
pub use validation::{
    validate, validate_for_reload, ReloadBaseline, ReloadDiff,
    ReloadValidationError, ValidationError,
};
```

- [ ] **Step 5: Build, test, commit**

```bash
rtk cargo test -p agent-shim-config --quiet
```

Expected: 5 new tests pass (existing config tests still pass).

```bash
rtk git add -A
rtk git commit -m "feat(config): validate_for_reload with rules 11-14 (Plan 04 P04 T1)"
```

---

### Task 2: Snapshot rebuild + reload trigger plumbing

**Files:**
- Create: `crates/observability/src/reload.rs`
- Create: `crates/gateway/src/reload_trigger.rs`
- Modify: `crates/observability/src/lib.rs`
- Modify: `crates/gateway/src/lib.rs`
- Modify: `crates/gateway/src/state.rs`

- [ ] **Step 1: Create the reload helper**

Create `crates/observability/src/reload.rs`:

```rust
//! Snapshot rebuild for hot-reload. Plan 04 P04 T2.
//!
//! Given a validated candidate config and a running `AppCore` view (just
//! the parts the rebuild needs — no Arcs to runtime objects), produce a
//! fresh `AppSnapshot`-shaped value. The actual `AppSnapshot` type lives
//! in the gateway crate to avoid circular deps; this module returns a
//! `ReloadBuild` struct the gateway can fold into its own snapshot.

use std::collections::HashSet;
use std::sync::Arc;

use agent_shim_config::GatewayConfig;

/// Rebuilt fields. The gateway crate lifts this into its own
/// `AppSnapshot` shape.
pub struct ReloadBuild {
    pub config: Arc<GatewayConfig>,
    pub auth_enabled: bool,
    pub auth_required: bool,
    pub configured_key_hashes: Arc<HashSet<String>>,
}

pub fn build(config: GatewayConfig) -> ReloadBuild {
    let auth_enabled = config.auth.enabled;
    let auth_required = config.auth.required;
    let configured_key_hashes: Arc<HashSet<String>> =
        Arc::new(config.auth.keys.keys().cloned().collect());
    ReloadBuild {
        config: Arc::new(config),
        auth_enabled,
        auth_required,
        configured_key_hashes,
    }
}
```

In `crates/observability/src/lib.rs`:

```rust
pub mod reload;
```

- [ ] **Step 2: Trigger channel**

Create `crates/gateway/src/reload_trigger.rs`:

```rust
//! Reload trigger plumbing. Plan 04 P04 T2.
//!
//! All reload triggers (SIGHUP, POST /admin/reload) feed into this
//! mpsc channel; one task on the receiving end performs the swap. This
//! ensures reload requests are serialized — no two swaps race for the
//! ArcSwap.

use std::path::PathBuf;
use tokio::sync::oneshot;

/// One reload request. The handler awaits `respond_to` for the outcome
/// before returning HTTP 200/4xx/5xx.
pub struct ReloadRequest {
    pub source: ReloadSource,
    pub respond_to: oneshot::Sender<ReloadOutcome>,
}

#[derive(Debug, Clone)]
pub enum ReloadSource {
    /// Re-read config from disk at the given path.
    Path(PathBuf),
    /// Use the YAML body directly (no disk).
    Yaml(String),
    /// SIGHUP handler — re-read from the original `--config` path.
    Sighup,
}

pub enum ReloadOutcome {
    Ok(agent_shim_config::ReloadDiff),
    ValidationError(String),
    ImmutableField(String),
    Io(String),
    Parse(String),
}
```

In `crates/gateway/src/lib.rs`:

```rust
pub mod reload_trigger;
```

- [ ] **Step 3: Wire `reload_tx` into `AppCore`**

Modify `crates/gateway/src/state.rs`:

```rust
pub struct AppCore {
    // ...existing fields from P01/P02/P03...
    /// Sender side of the reload trigger channel. SIGHUP listener and
    /// /admin/reload handler push onto this; the reload-applying task
    /// drains it. Spec §5.1.
    pub reload_tx: tokio::sync::mpsc::Sender<crate::reload_trigger::ReloadRequest>,
}
```

In `AppState::build`, before constructing `AppCore`:

```rust
        let (reload_tx, _reload_rx) = tokio::sync::mpsc::channel(8);
        // The receiver is moved into the reload-applying task by
        // `commands::serve::run`. AppState holds only the sender so
        // handlers can push reload requests.
```

Then add `reload_tx` to the `AppCore { … }` literal.

The receiver must be returned somehow. Easiest: `AppState::new` returns `(AppState, mpsc::Receiver<ReloadRequest>)`:

```rust
impl AppState {
    pub async fn new(config: agent_shim_config::GatewayConfig) -> (Self, tokio::sync::mpsc::Receiver<crate::reload_trigger::ReloadRequest>) {
        Self::build(config, Arc::new(SystemClock)).await
    }

    // ... new_with_clock similarly returns (Self, Receiver) ...
}
```

Update every `AppState::new(cfg).await` call in tests and `commands/serve.rs` to bind `(state, _reload_rx)` (or `let (state, mut reload_rx)` for serve).

- [ ] **Step 4: Build**

```bash
rtk cargo build --workspace 2>&1 | tail -10
```

Expected: clean (after fixing every test that called `AppState::new`).

- [ ] **Step 5: Commit**

```bash
rtk git add -A
rtk git commit -m "feat(gateway): reload trigger channel + AppState reload_tx (Plan 04 P04 T2)"
```

---

### Task 3: POST /admin/reload handler

**Files:**
- Create: `crates/gateway/src/admin/reload_handler.rs`
- Modify: `crates/gateway/src/admin/mod.rs`

- [ ] **Step 1: Write the handler**

Create `crates/gateway/src/admin/reload_handler.rs`:

```rust
//! POST /admin/reload — Plan 04 P04 T3.

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde_json::json;
use tokio::sync::oneshot;

use crate::reload_trigger::{ReloadOutcome, ReloadRequest, ReloadSource};
use crate::state::AppState;

pub async fn reload(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> impl IntoResponse {
    let source = if body.is_empty() {
        // No body → re-read from --config path.
        match state.core.config_path.clone() {
            Some(path) => ReloadSource::Path(path),
            None => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok": false, "errors": ["server started without --config; cannot reload from disk"]})),
                ).into_response();
            }
        }
    } else {
        // Body present → reload from YAML.
        let ct = headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !ct.starts_with("application/yaml") && !ct.starts_with("text/yaml") {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "errors": [format!("expected Content-Type: application/yaml, got: {ct}")]})),
            ).into_response();
        }
        match std::str::from_utf8(&body) {
            Ok(s) => ReloadSource::Yaml(s.to_string()),
            Err(_) => return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "errors": ["body is not valid UTF-8"]})),
            ).into_response(),
        }
    };

    let (tx, rx) = oneshot::channel();
    if state.core.reload_tx.send(ReloadRequest { source, respond_to: tx }).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "errors": ["reload task not running"]})),
        ).into_response();
    }

    match rx.await {
        Ok(ReloadOutcome::Ok(diff)) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "applied": {
                    "routes_total": diff.routes_total,
                    "routes_added": diff.routes_added,
                    "routes_removed": diff.routes_removed,
                    "routes_modified": diff.routes_modified,
                    "policies_changed": diff.policies_changed,
                    "auth_keys_added": diff.auth_keys_added,
                    "auth_keys_removed": diff.auth_keys_removed,
                },
                "warnings": diff.warnings,
            })),
        ).into_response(),
        Ok(ReloadOutcome::ValidationError(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "errors": [msg]})),
        ).into_response(),
        Ok(ReloadOutcome::ImmutableField(msg)) => (
            StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "errors": [msg]})),
        ).into_response(),
        Ok(ReloadOutcome::Parse(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "errors": [format!("YAML parse error: {msg}")]})),
        ).into_response(),
        Ok(ReloadOutcome::Io(msg)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "errors": [format!("IO error: {msg}")]})),
        ).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "errors": ["reload task dropped response channel"]})),
        ).into_response(),
    }
}
```

- [ ] **Step 2: Register the route**

In `crates/gateway/src/admin/mod.rs`:

```rust
mod handlers;
mod metrics_handler;
mod reload_handler;

use axum::{routing::{get, post}, Router};

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handlers::healthz))
        .route("/readyz", get(handlers::readyz))
        .route("/metrics", get(metrics_handler::metrics))
        .route("/admin/reload", post(reload_handler::reload))
        .with_state(state)
}
```

- [ ] **Step 3: Build**

```bash
rtk cargo build -p agent-shim 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 4: Commit**

```bash
rtk git add -A
rtk git commit -m "feat(gateway): POST /admin/reload handler (Plan 04 P04 T3)"
```

---

### Task 4: Reload-applying task + SIGHUP listener

**Files:**
- Modify: `crates/gateway/src/commands/serve.rs`

- [ ] **Step 1: Read the existing serve command**

```bash
rtk read crates/gateway/src/commands/serve.rs
```

Today it does roughly: load config → build state → run server. Phase 5 needs to also: pin `config_path` on `AppCore`, spawn a SIGHUP listener task on Unix, and spawn the reload-applying task that consumes from `reload_rx`.

- [ ] **Step 2: Refactor**

Replace the body of `commands::serve::run` with:

```rust
pub async fn run(config_path: &std::path::Path) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(config_path)?;
    let config: agent_shim_config::GatewayConfig = serde_yaml::from_str(&raw)?;
    agent_shim_config::validate(&config)?;

    let _tracing_handles = agent_shim_observability::init(&config.logging, config.otel.as_ref());

    let (mut state, mut reload_rx) = crate::state::AppState::new(config).await;

    // Pin the config path so reload-from-disk works.
    let core_with_path = std::sync::Arc::new(crate::state::AppCore {
        config_path: Some(config_path.to_path_buf()),
        ..(*state.core).clone()  // assumes AppCore: Clone — see below
    });
    state.core = core_with_path;

    // Reload-applying task. Drains `reload_rx`; for each request, parses
    // YAML / re-reads disk, validates against the AppCore baseline,
    // builds a fresh AppSnapshot, atomic-swaps. Spec §5.2.
    {
        let state_for_task = state.clone();
        tokio::spawn(async move {
            while let Some(req) = reload_rx.recv().await {
                let outcome = handle_reload(&state_for_task, req.source).await;
                let _ = req.respond_to.send(outcome);
            }
        });
    }

    // SIGHUP listener (Unix only). Sends a `Sighup` request into the
    // same channel.
    #[cfg(unix)]
    {
        let reload_tx = state.core.reload_tx.clone();
        tokio::spawn(async move {
            let mut sig = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "failed to install SIGHUP handler");
                    return;
                }
            };
            while sig.recv().await.is_some() {
                let (tx, _rx) = tokio::sync::oneshot::channel();
                let _ = reload_tx.send(crate::reload_trigger::ReloadRequest {
                    source: crate::reload_trigger::ReloadSource::Sighup,
                    respond_to: tx,
                }).await;
                tracing::info!(target = "agent_shim::reload", "SIGHUP received; reload requested");
            }
        });
    }

    if state.core.admin_config.is_some() {
        crate::server::run_with_admin(state).await
    } else {
        crate::server::run(state).await
    }?;

    if let Some(otel) = _tracing_handles.otel { otel.shutdown(); }
    Ok(())
}

async fn handle_reload(
    state: &crate::state::AppState,
    source: crate::reload_trigger::ReloadSource,
) -> crate::reload_trigger::ReloadOutcome {
    use crate::reload_trigger::{ReloadOutcome, ReloadSource};

    let yaml: String = match source {
        ReloadSource::Path(p) => match std::fs::read_to_string(&p) {
            Ok(s) => s,
            Err(e) => return ReloadOutcome::Io(e.to_string()),
        },
        ReloadSource::Yaml(s) => s,
        ReloadSource::Sighup => {
            match state.core.config_path.as_ref() {
                Some(p) => match std::fs::read_to_string(p) {
                    Ok(s) => s,
                    Err(e) => return ReloadOutcome::Io(e.to_string()),
                },
                None => return ReloadOutcome::Io("no config path; SIGHUP can't re-read".into()),
            }
        }
    };

    let candidate: agent_shim_config::GatewayConfig = match serde_yaml::from_str(&yaml) {
        Ok(c) => c,
        Err(e) => return ReloadOutcome::Parse(e.to_string()),
    };

    let baseline = agent_shim_config::ReloadBaseline {
        upstream_names: state.snapshot.load_full().config.upstreams.keys().cloned().collect(),
        server: state.core.server_config.clone(),
        admin: state.core.admin_config.clone(),
        otel_endpoint: state.snapshot.load_full().config.otel.as_ref().and_then(|o| o.endpoint.clone()),
    };

    match agent_shim_config::validate_for_reload(&candidate, &baseline) {
        Ok(diff) => {
            // Build fresh snapshot from candidate.
            let build = agent_shim_observability::reload::build(candidate);
            let new_snap = std::sync::Arc::new(crate::state::AppSnapshot {
                config: build.config,
                auth_enabled: build.auth_enabled,
                auth_required: build.auth_required,
                configured_key_hashes: build.configured_key_hashes,
            });
            state.snapshot.store(new_snap);

            // Reload metric.
            metrics::counter!("agent_shim_config_reloads_total", "result" => "ok").increment(1);
            tracing::info!(target = "agent_shim::reload", "config reloaded");
            ReloadOutcome::Ok(diff)
        }
        Err(agent_shim_config::ReloadValidationError::ImmutableFieldChanged { field, old, new }) => {
            metrics::counter!("agent_shim_config_reloads_total", "result" => "immutable_field").increment(1);
            ReloadOutcome::ImmutableField(format!("{field}: {old} → {new} forbidden (restart required)"))
        }
        Err(agent_shim_config::ReloadValidationError::UpstreamSetChanged { added, removed }) => {
            metrics::counter!("agent_shim_config_reloads_total", "result" => "immutable_field").increment(1);
            ReloadOutcome::ImmutableField(format!(
                "upstreams.* set changed (added={added:?}, removed={removed:?}); immutable in v0.5"
            ))
        }
        Err(other) => {
            metrics::counter!("agent_shim_config_reloads_total", "result" => "validation_error").increment(1);
            ReloadOutcome::ValidationError(other.to_string())
        }
    }
}
```

The `AppCore { ..(*state.core).clone() }` requires `AppCore: Clone`. Add `#[derive(Clone)]` to `AppCore` in `state.rs`. (All its fields are `Arc<...>` or copyable types, so derive-Clone is cheap.)

- [ ] **Step 3: Build**

```bash
rtk cargo build --workspace 2>&1 | tail -10
```

Expected: clean.

- [ ] **Step 4: Commit**

```bash
rtk git add -A
rtk git commit -m "feat(gateway): reload-applying task + SIGHUP listener (Plan 04 P04 T4)"
```

---

### Task 5: Integration tests for /admin/reload

**Files:**
- Create: `crates/gateway/tests/reload_admin_post.rs`

- [ ] **Step 1: Write the tests**

```rust
//! Plan 04 P04 T5: POST /admin/reload integration tests.

use std::net::SocketAddr;
use std::sync::Arc;

mod common {
    pub async fn pick_port() -> u16 {
        tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
            .local_addr().unwrap().port()
    }
}

async fn spawn(yaml: &str) -> (SocketAddr, SocketAddr, Arc<agent_shim::state::AppCore>) {
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(yaml).unwrap();
    let public_addr: SocketAddr =
        format!("{}:{}", cfg.server.bind, cfg.server.port).parse().unwrap();
    let admin_cfg = cfg.admin.clone().expect("admin block required");
    let admin_addr: SocketAddr =
        format!("{}:{}", admin_cfg.bind, admin_cfg.port).parse().unwrap();
    let (mut state, mut reload_rx) = agent_shim::state::AppState::new(cfg).await;

    // Run the reload-applying task locally inside the test.
    let state_for_task = state.clone();
    tokio::spawn(async move {
        while let Some(req) = reload_rx.recv().await {
            let outcome = agent_shim::commands::serve::handle_reload_for_test(
                &state_for_task,
                req.source,
            ).await;
            let _ = req.respond_to.send(outcome);
        }
    });

    let pl = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let al = tokio::net::TcpListener::bind(admin_addr).await.unwrap();
    let pa = agent_shim::server::build_router(state.clone());
    let aa = agent_shim::admin::build_router(state.clone());
    tokio::spawn(async move { let _ = axum::serve(pl, pa).await; });
    tokio::spawn(async move { let _ = axum::serve(al, aa).await; });

    let core = state.core.clone();
    (public_addr, admin_addr, core)
}

const BASE_YAML: &str = r#"
server: {bind: 127.0.0.1, port: __PUBLIC__}
admin: {bind: 127.0.0.1, port: __ADMIN__}
upstreams:
  m: {type: open_ai_compatible, base_url: http://x/v1, api_key: a}
routes:
  - {frontend: openai_chat, model: x, upstream: m, upstream_model: x}
"#;

fn build_yaml(public: u16, admin: u16) -> String {
    BASE_YAML
        .replace("__PUBLIC__", &public.to_string())
        .replace("__ADMIN__", &admin.to_string())
}

#[tokio::test]
async fn reload_with_yaml_body_succeeds() {
    let public = common::pick_port().await;
    let admin = common::pick_port().await;
    let yaml = build_yaml(public, admin);
    let (_p, admin_addr, _core) = spawn(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Reload with same YAML — must succeed.
    let resp = reqwest::Client::new()
        .post(format!("http://{}/admin/reload", admin_addr))
        .header("content-type", "application/yaml")
        .body(yaml)
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], serde_json::Value::Bool(true));
}

#[tokio::test]
async fn reload_with_invalid_yaml_returns_400() {
    let public = common::pick_port().await;
    let admin = common::pick_port().await;
    let yaml = build_yaml(public, admin);
    let (_p, admin_addr, _core) = spawn(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{}/admin/reload", admin_addr))
        .header("content-type", "application/yaml")
        .body("[ : not valid yaml")
        .send().await.unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn reload_with_changed_server_port_returns_403() {
    let public = common::pick_port().await;
    let admin = common::pick_port().await;
    let yaml = build_yaml(public, admin);
    let (_p, admin_addr, _core) = spawn(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let mut bad: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
    bad["server"]["port"] = serde_yaml::Value::from(public + 1);
    let bad_yaml = serde_yaml::to_string(&bad).unwrap();

    let resp = reqwest::Client::new()
        .post(format!("http://{}/admin/reload", admin_addr))
        .header("content-type", "application/yaml")
        .body(bad_yaml)
        .send().await.unwrap();
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], serde_json::Value::Bool(false));
}

#[tokio::test]
async fn reload_without_body_without_config_path_returns_500() {
    let public = common::pick_port().await;
    let admin = common::pick_port().await;
    let yaml = build_yaml(public, admin);
    let (_p, admin_addr, _core) = spawn(&yaml).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The test harness in `spawn()` does not set config_path, so a
    // body-less reload should fail with a clear message.
    let resp = reqwest::Client::new()
        .post(format!("http://{}/admin/reload", admin_addr))
        .send().await.unwrap();
    assert_eq!(resp.status(), 500);
}
```

The test references `agent_shim::commands::serve::handle_reload_for_test`. To expose that, in `crates/gateway/src/commands/serve.rs`, rename `async fn handle_reload` to `pub async fn handle_reload_for_test` (keeping the implementation; the production caller inside `serve::run` will use `handle_reload_for_test` too — it's the same function).

Mark `commands` as `pub mod commands;` in `crates/gateway/src/lib.rs` so the integration tests see it.

- [ ] **Step 2: Build, test, commit**

```bash
rtk cargo test -p agent-shim --test reload_admin_post --quiet
```

Expected: 4 passed.

```bash
rtk git add -A
rtk git commit -m "test(gateway): /admin/reload integration coverage (Plan 04 P04 T5)"
```

---

### Task 6: SIGHUP test (Unix only)

**Files:**
- Create: `crates/gateway/tests/reload_sighup.rs`

- [ ] **Step 1: Write the test**

```rust
//! Plan 04 P04 T6: SIGHUP triggers a reload. Unix-only.

#![cfg(unix)]

use std::process::Stdio;
use std::time::Duration;

#[tokio::test]
async fn sighup_increments_reload_counter() {
    // Build the gateway binary under test.
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--bin", "agent-shim", "--quiet"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed");

    let public_port = pick_port_blocking();
    let admin_port = pick_port_blocking();

    // Write a temp config.
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("gateway.yaml");
    let yaml = format!(
        r#"
server: {{bind: 127.0.0.1, port: {public_port}}}
admin: {{bind: 127.0.0.1, port: {admin_port}}}
upstreams:
  m: {{type: open_ai_compatible, base_url: http://x/v1, api_key: a}}
routes:
  - {{frontend: openai_chat, model: x, upstream: m, upstream_model: x}}
"#
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    // Spawn the binary.
    let bin = std::path::PathBuf::from(env!("CARGO_TARGET_DIR")).join("debug/agent-shim");
    let bin = if bin.exists() { bin } else {
        // Fallback: assume target/debug under the workspace root.
        std::path::PathBuf::from("target/debug/agent-shim")
    };
    let mut child = std::process::Command::new(bin)
        .args(["serve", "--config", cfg_path.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent-shim");

    // Wait for /readyz to come up.
    let admin_url = format!("http://127.0.0.1:{}", admin_port);
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if reqwest::get(format!("{}/readyz", admin_url)).await.is_ok() {
            break;
        }
    }

    // Send SIGHUP.
    let pid = child.id();
    let kill_status = std::process::Command::new("kill")
        .args(["-HUP", &pid.to_string()])
        .status()
        .expect("kill -HUP");
    assert!(kill_status.success());

    // Give the reload task a moment.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Scrape /metrics. Counter must show ≥ 1 reload.
    let body = reqwest::get(format!("{}/metrics", admin_url))
        .await.unwrap().text().await.unwrap();
    let scrape = prometheus_parse::Scrape::parse(body.lines().map(Ok)).unwrap();
    let total: f64 = scrape.samples.iter()
        .filter(|s| s.metric == "agent_shim_config_reloads_total")
        .filter_map(|s| match s.value {
            prometheus_parse::Value::Counter(v) | prometheus_parse::Value::Untyped(v) => Some(v),
            _ => None,
        })
        .sum();

    let _ = child.kill();
    let _ = child.wait();

    assert!(total >= 1.0, "expected ≥1 reload, got {total}");
}

fn pick_port_blocking() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}
```

Add `tempfile = "3"` to `crates/gateway/Cargo.toml` `[dev-dependencies]` if absent.

- [ ] **Step 2: Test**

```bash
rtk cargo test -p agent-shim --test reload_sighup --quiet
```

Expected: 1 passed (Unix); `not run` (Windows — `#[cfg(unix)]` excludes the test).

- [ ] **Step 3: Commit**

```bash
rtk git add -A
rtk git commit -m "test(gateway): SIGHUP triggers reload (Plan 04 P04 T6)"
```

---

### Task 7: In-flight + breaker-state-survives-reload

**Files:**
- Create: `crates/gateway/tests/reload_in_flight.rs`

- [ ] **Step 1: Write the tests**

```rust
//! Plan 04 P04 T7: in-flight requests use the snapshot at the time of
//! their start; breaker state survives reload.

use std::net::SocketAddr;

#[tokio::test]
async fn breaker_state_survives_reload() {
    // Build a gateway with a breaker policy. Trip the breaker via the
    // BreakerRegistry directly (the spec § allows test-only direct
    // access), reload with the same policy, assert the breaker is still
    // open via a probe request that should be skipped.
    use agent_shim_router::circuit_breaker::{BreakerPolicy, BreakerRegistry, SystemClock};

    let public = pick_port().await;
    let admin = pick_port().await;
    let yaml = format!(
        r#"
server: {{bind: 127.0.0.1, port: {public}}}
admin: {{bind: 127.0.0.1, port: {admin}}}
upstreams:
  m: {{type: open_ai_compatible, base_url: http://x/v1, api_key: a}}
routes:
  - frontend: openai_chat
    model: x
    upstream: m
    upstream_model: x
    breaker:
      enabled: true
      failure_threshold_pct: 50
      min_requests: 2
      window_secs: 60
      open_cooldown_secs: 60
"#);
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(&yaml).unwrap();
    let (state, mut reload_rx) = agent_shim::state::AppState::new(cfg).await;
    let state_for_task = state.clone();
    tokio::spawn(async move {
        while let Some(req) = reload_rx.recv().await {
            let o = agent_shim::commands::serve::handle_reload_for_test(&state_for_task, req.source).await;
            let _ = req.respond_to.send(o);
        }
    });

    // Force two failures into the breaker registry to trip it.
    let policy = BreakerPolicy {
        enabled: true,
        failure_threshold_pct: 50,
        min_requests: 2,
        window: std::time::Duration::from_secs(60),
        open_cooldown: std::time::Duration::from_secs(60),
    };
    state.core.breaker_registry.record("m", "x", false, &policy);
    state.core.breaker_registry.record("m", "x", false, &policy);

    // Confirm Open.
    let decision = state.core.breaker_registry.query_state("m", "x", &policy);
    assert!(matches!(decision, agent_shim_router::BreakerDecision::Skip));

    // Trigger a reload (same YAML).
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.core.reload_tx.send(agent_shim::reload_trigger::ReloadRequest {
        source: agent_shim::reload_trigger::ReloadSource::Yaml(yaml.clone()),
        respond_to: tx,
    }).await.unwrap();
    let _ = rx.await.unwrap();

    // After reload, breaker MUST still be open (state survives).
    let decision = state.core.breaker_registry.query_state("m", "x", &policy);
    assert!(
        matches!(decision, agent_shim_router::BreakerDecision::Skip),
        "breaker state must survive reload"
    );
}

async fn pick_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
        .local_addr().unwrap().port()
}
```

- [ ] **Step 2: Test, frozen-core, commit**

```bash
rtk cargo test -p agent-shim --test reload_in_flight --quiet
rtk git diff master -- crates/core/ crates/frontends/ crates/providers/src/
```

Expected: 1 passed; empty diff.

```bash
rtk git add -A
rtk git commit -m "test(gateway): breaker state survives reload (Plan 04 P04 T7)"
```

---

### Task 8: Spec compliance review

- [ ] **Step 1: Reviewer dispatch**

> Review commits `<P04 T1..T7>` against spec §5. Verify:
> 1. **Rule 11** — every route's upstream must reference a baseline upstream.
> 2. **Rule 12** — adding/removing upstreams during reload returns 403 with `UpstreamSetChanged`.
> 3. **Rule 13** — server.* and admin.* changes return 403 with `ImmutableFieldChanged`.
> 4. **Rule 14** — otel.endpoint change returns 403; otel.sample_ratio CAN change.
> 5. **§5.1 triggers** — both SIGHUP (Unix) and POST /admin/reload work; same channel.
> 6. **§5.2 algorithm** — validation BEFORE swap; OLD snapshot stays on validation failure.
> 7. **§5.4 breaker survival** — `breaker_state_survives_reload` test passes.
> 8. **§5.6 response shape** — 200/400/403 JSON shapes match the spec.
> 9. **Frozen core** — empty diff against master.

- [ ] **Step 2: Apply FAIL findings**

---

### Task 9: Code quality review

- [ ] **Step 1: Reviewer dispatch**

> Review commits `<P04 T1..T8>` for code quality.
> 1. `handle_reload` is a long function with a big match. Should it be split?
> 2. The `AppCore: Clone` derive — is it actually needed for the `..(*state.core).clone()` trick, or is there a cleaner way to set `config_path` once?
> 3. Test isolation — the SIGHUP test spawns a real binary; does it leave zombies on test failure?
> 4. The reload-applying task panic-paths — if the task panics for any reason, future reload requests block forever on the channel. Is there a supervision strategy?
> 5. `metrics::counter!("agent_shim_config_reloads_total", ...)` uses a string literal; should it use the const from `observability::metrics::names::CONFIG_RELOADS_TOTAL` instead to avoid drift?

- [ ] **Step 2: Apply CRITICAL/HIGH findings**

---

## Done when

- [ ] Workspace test count ≥ 638.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] Frozen-core diff empty.
- [ ] POST /admin/reload supports body and body-less forms.
- [ ] SIGHUP triggers a reload on Unix; integration test passes.
- [ ] Validation failures keep the OLD snapshot in place; test proves this.
- [ ] Breaker state survives reload (test).
- [ ] T8 + T9 reviews clear of CRITICAL/HIGH.
