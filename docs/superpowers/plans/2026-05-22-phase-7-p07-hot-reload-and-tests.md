# Phase 7 P07 — Hot-Reload + Tests + Bench + Release v0.7.0 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land hot-reload for `PluginRegistry` (move from `AppCore` → `AppSnapshot`), close the H5/skip/in-flight test coverage gaps, ship a meaningful zero-overhead micro-bench, and cut v0.7.0 with full docs + CHANGELOG.

**Architecture:** `plugins: Arc<PluginRegistry>` migrates from immutable `AppCore` onto hot-swappable `AppSnapshot`. `handle_reload` calls `PluginRegistry::build` *before* the snapshot store, so a Layer-B failure rejects the entire reload (no partial commit). New `ReloadOutcome::PluginValidation` variant → HTTP 400 + metric label. In-flight isolation is automatic via `snapshot.load_full()` at the top of `pipeline::dispatch`.

**Tech Stack:** Rust 2021, `arc-swap`, `tokio::sync::Barrier`, `eventsource-client` (dev-only), `criterion` (bench-only).

**Source spec:** [`docs/superpowers/specs/2026-05-22-phase-7-p07-hot-reload-and-tests-design.md`](../specs/2026-05-22-phase-7-p07-hot-reload-and-tests-design.md)

**Frozen-core invariant:** `crates/core/` MUST be untouched. Acceptance check at end: `git diff master..HEAD -- crates/core/` empty.

**`rtk` prefix:** ALWAYS prefix bash/git commands with `rtk` for token efficiency. The user requires this.

---

## Task 1: Move `plugins` field from `AppCore` to `AppSnapshot`

**Goal:** Pure refactor. After this task, `plugins` lives on the hot-swappable snapshot, but nothing actually swaps it yet. All existing tests must still pass with byte-identical behavior.

**Files:**
- Modify: `crates/gateway/src/state.rs` — remove field from `AppCore`, add to `AppSnapshot`, update `AppState::build` and `new_with_plugins`.
- Modify: `crates/gateway/src/pipeline.rs` — change 6 reads at lines 580, 605, 838, 854, 916, 1054, 1078 from `state.core.plugins` → `snapshot.plugins` (snapshot is already captured at line 256).
- Modify: `crates/gateway/src/commands/serve.rs:135` — change `state.core.plugins.clone()` → `state.snapshot.load_full().plugins.clone()`.

- [ ] **Step 1: Inventory all `core.plugins` reads**

Run: `rtk grep -rn 'core\.plugins\|core\s*\.plugins' crates/gateway/src/`
Expected: 7 lines total (1 in serve.rs + 6 in pipeline.rs).

- [ ] **Step 2: Modify `AppCore` and `AppSnapshot` in `state.rs`**

Remove the `plugins` field from `AppCore` (currently lines 116-121). Add it to `AppSnapshot` (after `configured_key_hashes`):

```rust
pub struct AppSnapshot {
    pub config: Arc<agent_shim_config::GatewayConfig>,
    pub auth_enabled: bool,
    pub auth_required: bool,
    pub configured_key_hashes: Arc<HashSet<String>>,
    /// Plan 07 P07: plugins now hot-swappable. In-flight requests
    /// capture the snapshot at the top of `pipeline::dispatch`;
    /// subsequent reloads do not perturb their plugin view.
    pub plugins: Arc<agent_shim_plugins::PluginRegistry>,
}
```

Update `AppState::build` so the snapshot construction owns the plugin Arc (instead of placing it on AppCore):

```rust
// inside AppState::build, replace the existing
//     let plugins = match plugin_override { ... }
// block — keep the construction logic, but bind it
// AFTER `AppCore` is constructed, so it can move into the snapshot.

// ... existing AppCore construction (without `plugins:` field) ...

let plugins = match plugin_override {
    Some(p) => p,
    None => {
        let factories = agent_shim_plugins::builtin::builtin_plugins();
        let plugin_specs: Vec<(String, agent_shim_config::plugins::PluginEntry)> = config
            .plugins
            .iter()
            .map(|(name, entry)| (name.clone(), entry.clone()))
            .collect();
        let deps = agent_shim_plugins::FactoryDependencies {
            providers: registry.inner_map(),
        };
        Arc::new(
            agent_shim_plugins::PluginRegistry::build(
                factories,
                &plugin_specs,
                &config.routes,
                deps,
            )
            .map_err(|e| anyhow::anyhow!("plugin registry build failed: {e}"))?,
        )
    }
};

// ... existing snapshot construction ...
let snapshot = Arc::new(ArcSwap::new(Arc::new(AppSnapshot {
    config: Arc::new(config),
    auth_enabled: build.auth_enabled,
    auth_required: build.auth_required,
    configured_key_hashes: build.configured_key_hashes,
    plugins, // ← NEW
})));
```

(Locations to edit are by symbol, not line number — `state.rs` changes shape across tasks.)

- [ ] **Step 3: Modify the 7 read-sites**

In `crates/gateway/src/pipeline.rs`, in `dispatch` (the snapshot is already
bound at the top via `let snapshot = state.snapshot.load_full();`), change
each occurrence as follows:

```rust
// Lines 578-587 (H2):
canonical = snapshot
    .plugins
    .run_on_decoded_request(
        (frontend_kind_for_hooks, model_alias.as_str()),
        &plugin_ctx,
        canonical,
    )
    .await
    .map_err(|e| handler_error_from_plugin_error(e, frontend_kind_for_hooks))?;

// Lines 602-611 (H3):
canonical = snapshot
    .plugins
    .run_on_resolved(
        (frontend_kind_for_hooks, model_alias.as_str()),
        &plugin_ctx,
        canonical,
    )
    .await
    .map_err(|e| handler_error_from_plugin_error(e, frontend_kind_for_hooks))?;

// Line 838 (wrap_stream call site 1):
let upstream_stream = snapshot.plugins.wrap_stream(...);

// Line 854, 916 (registry handles for H7 guards):
registry: Some(snapshot.plugins.clone()),
// (×2)

// Line 1054 (wrap_stream call site 2):
let stream = snapshot.plugins.wrap_stream(...);

// Line 1078 (run_on_response_complete):
snapshot.plugins.run_on_response_complete(...).await;
```

In `crates/gateway/src/commands/serve.rs`, line 135:

```rust
// P05 T11: clone the plugin Arc + shutdown.plugin_flush_secs BEFORE
// moving `state` into axum. After axum drain, no new H7 spawns occur
// (no requests in flight) so flush can run safely on this clone.
// P07: plugins now live on AppSnapshot; capture the current snapshot once.
let plugins = state.snapshot.load_full().plugins.clone();
let flush_secs = state.snapshot.load_full().config.shutdown.plugin_flush_secs;
```

- [ ] **Step 4: Run the full workspace**

Run: `rtk cargo build --workspace`
Expected: PASS — no compile errors.

- [ ] **Step 5: Run the full test suite**

Run: `rtk cargo nextest run --workspace`
Expected: PASS — 912/912 (or current baseline). Behavior is byte-identical
because no swap actually happens yet.

- [ ] **Step 6: Frozen-core check**

Run: `rtk git diff master..HEAD -- crates/core/`
Expected: empty.

- [ ] **Step 7: Commit**

```bash
rtk git add crates/gateway/src/state.rs crates/gateway/src/pipeline.rs crates/gateway/src/commands/serve.rs
rtk git commit -m "refactor(gateway): move PluginRegistry from AppCore to AppSnapshot (P07 T1)"
```

---

## Task 2: Extend `ReloadOutcome` with `PluginValidation` + admin 400 + metric label

**Goal:** Add the failure-path plumbing so T3 has a destination to return. After this task, the type signature exists but `handle_reload` still doesn't call `PluginRegistry::build`.

**Files:**
- Modify: `crates/gateway/src/reload_trigger.rs` — add `PluginValidation(String)` variant.
- Modify: `crates/gateway/src/admin/reload_handler.rs` — add 400 branch.

- [ ] **Step 1: Add the variant**

In `crates/gateway/src/reload_trigger.rs`, after line 40 (`Parse(String)`):

```rust
pub enum ReloadOutcome {
    Ok(agent_shim_config::ReloadDiff),
    ValidationError(String),
    ImmutableField(String),
    Io(String),
    Parse(String),
    /// Plan 07 P07: plugin section failed Layer-B validation (unknown
    /// kind / factory parse error / hook subscription mismatch). The
    /// entire reload is rejected; old plugin registry remains active.
    PluginValidation(String),
}
```

- [ ] **Step 2: Add the admin handler branch**

In `crates/gateway/src/admin/reload_handler.rs`, add a new match arm after the `Parse` arm (around line 122):

```rust
        Ok(ReloadOutcome::PluginValidation(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "errors": [format!("plugin validation error: {msg}")]})),
        )
            .into_response(),
```

- [ ] **Step 3: Verify metric label is acceptable**

Run: `rtk grep -n 'plugin_validation_error\|config_reloads_total' crates/observability/src/metrics/`
Expected: `agent_shim_config_reloads_total` is a counter with a `result` label. No catalog change required — `metrics-rs` counter labels are free-form strings (Prometheus accepts arbitrary label values).

- [ ] **Step 4: Compile-check**

Run: `rtk cargo build -p agent-shim --tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/gateway/src/reload_trigger.rs crates/gateway/src/admin/reload_handler.rs
rtk git commit -m "feat(gateway): ReloadOutcome::PluginValidation + admin 400 branch (P07 T2)"
```

---

## Task 3: Wire `PluginRegistry::build` into `handle_reload`

**Goal:** Hot-reload actually swaps the registry. After this task, a reload that changes `plugins:` config takes effect on the next request; a reload with broken plugin config is rejected without affecting the running snapshot.

**Files:**
- Modify: `crates/gateway/src/commands/serve.rs` — `handle_reload` rebuilds plugin registry before snapshot store; emits new `ReloadOutcome::PluginValidation` on failure; bundles plugins into the new snapshot.

- [ ] **Step 1: Modify `handle_reload`**

In `crates/gateway/src/commands/serve.rs`, inside the
`Ok(diff) => { ... }` arm of `validate_for_reload` (around lines 248-290),
add the plugin rebuild BEFORE the `state.snapshot.store(new_snap)` call:

```rust
    match agent_shim_config::validate_for_reload(&candidate, &baseline) {
        Ok(diff) => {
            // ── P07 §4.3: Layer B (plugin registry rebuild) ───────────
            // Must happen BEFORE snapshot.store so a failure rejects the
            // entire reload (no partial commit). Provider set is
            // guaranteed immutable across reload (UpstreamSetChanged is
            // upstream of this branch), so borrowing the current
            // ProviderRegistry is safe.
            let plugin_specs: Vec<(String, agent_shim_config::plugins::PluginEntry)> =
                candidate
                    .plugins
                    .iter()
                    .map(|(name, entry)| (name.clone(), entry.clone()))
                    .collect();
            let new_plugins = {
                let factories = agent_shim_plugins::builtin::builtin_plugins();
                let deps = agent_shim_plugins::FactoryDependencies {
                    providers: state.core.providers.inner_map(),
                };
                match agent_shim_plugins::PluginRegistry::build(
                    factories,
                    &plugin_specs,
                    &candidate.routes,
                    deps,
                ) {
                    Ok(r) => Arc::new(r),
                    Err(e) => {
                        metrics::counter!(
                            agent_shim_observability::metrics::names::CONFIG_RELOADS_TOTAL,
                            "result" => "plugin_validation_error",
                        )
                        .increment(1);
                        tracing::warn!(
                            target = "agent_shim::reload",
                            error = %e,
                            "config reload rejected: plugin validation"
                        );
                        return ReloadOutcome::PluginValidation(e.to_string());
                    }
                }
            };

            // Build a fresh snapshot from the candidate config and
            // atomic-swap. In-flight requests captured the OLD snapshot
            // at the top of `pipeline::dispatch` and stay on it (spec
            // §2.2); new requests after this store() see the new one,
            // INCLUDING the new PluginRegistry.
            let build = agent_shim_observability::reload::build(candidate);
            let new_snap = Arc::new(crate::state::AppSnapshot {
                config: build.config,
                auth_enabled: build.auth_enabled,
                auth_required: build.auth_required,
                configured_key_hashes: build.configured_key_hashes,
                plugins: new_plugins,
            });
            state.snapshot.store(new_snap);

            // ... existing limiter rebuild + metrics::counter!(... "ok") + ReloadOutcome::Ok(diff) ...
```

(The rest of the `Ok(diff)` arm — limiter rebuild, metric increment, log, and `ReloadOutcome::Ok(diff)` return — stays unchanged.)

- [ ] **Step 2: Verify build**

Run: `rtk cargo build -p agent-shim`
Expected: PASS.

- [ ] **Step 3: Quick sanity test — run existing reload tests**

Run: `rtk cargo nextest run -p agent-shim reload`
Expected: PASS — existing `reload_*` tests (admin, sighup, in-flight) still pass because they don't change plugin config across reload.

- [ ] **Step 4: Run full workspace**

Run: `rtk cargo nextest run --workspace`
Expected: PASS — 912/912.

- [ ] **Step 5: Frozen-core check**

Run: `rtk git diff master..HEAD -- crates/core/`
Expected: empty.

- [ ] **Step 6: Commit**

```bash
rtk git add crates/gateway/src/commands/serve.rs
rtk git commit -m "feat(gateway): hot-reload PluginRegistry via candidate-build commit-or-rollback (P07 T3)"
```

---

## Task 4: Add `eventsource-client` dev-dep + first H5 stream test (drop alternate events)

**Goal:** Establish the SSE-based testing pattern. Single test: an H5 plugin skips odd-indexed events; client sees only even-indexed events.

**Files:**
- Modify: `crates/gateway/Cargo.toml` — add `eventsource-client` to `[dev-dependencies]`.
- Create: `crates/gateway/tests/plugins_h5_stream.rs`.

- [ ] **Step 1: Add dev-dependency**

In `crates/gateway/Cargo.toml`, under `[dev-dependencies]`:

```toml
eventsource-client = "0.13"
```

(If 0.13 doesn't resolve, try 0.12. Documented current as of writing.)

Run: `rtk cargo build -p agent-shim --tests`
Expected: PASS — `eventsource-client` resolves and compiles.

- [ ] **Step 2: Create the test file with the alternate-drop test**

Create `crates/gateway/tests/plugins_h5_stream.rs`:

```rust
//! Phase 7 P07: H5 (`on_stream_event`) integration tests.
//!
//! Uses `eventsource-client` (dev-dep) to parse the gateway's SSE
//! response body into a sequence of typed events. Avoids hand-rolled
//! SSE frame parsing.
//!
//! Anthropic Messages frontend SSE shape (per `crates/frontends/src/anthropic_messages.rs`):
//!   event: message_start | event: content_block_start | event: content_block_delta
//!   event: content_block_stop | event: message_delta | event: message_stop | event: error

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eventsource_client::{Client, SSE};
use futures::StreamExt;
use serde_json::json;

use agent_shim_core::{
    BackendTarget, CanonicalRequest, CanonicalStream, ContentBlockKind, FrontendKind,
    Message, ResponseId, StopReason, StreamEvent,
};
use agent_shim_plugins::{
    HookSet, OnError, Plugin, PluginContext, PluginEntry, PluginError, PluginRegistry,
    PluginResult, StreamEventDecision,
};
use agent_shim_providers::{BackendProvider, ProviderCapabilities, ProviderError};

/// H5 plugin that skips every odd-indexed event (counter starts at 0).
struct EveryOtherEventSkipper {
    counter: std::sync::Mutex<u64>,
}

#[async_trait]
impl Plugin for EveryOtherEventSkipper {
    fn kind_name(&self) -> &'static str { "every_other_skipper" }
    fn hooks(&self) -> HookSet { HookSet::STREAM_EVENT }
    async fn on_stream_event(
        &self,
        _ctx: &PluginContext,
        event: StreamEvent,
    ) -> PluginResult<StreamEventDecision> {
        let mut c = self.counter.lock().unwrap();
        let n = *c;
        *c += 1;
        if n % 2 == 1 {
            Ok(StreamEventDecision::Skip)
        } else {
            Ok(StreamEventDecision::Forward(event))
        }
    }
}

/// Deterministic provider that streams a fixed event sequence.
struct ScriptedProvider;

#[async_trait]
impl BackendProvider for ScriptedProvider {
    fn name(&self) -> &'static str { "scripted" }
    fn capabilities(&self) -> &ProviderCapabilities { &ProviderCapabilities { streaming: true, ..Default::default() } }
    async fn complete(
        &self,
        _req: CanonicalRequest,
        _target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError> {
        use agent_shim_core::{MessageRole, Usage};
        let events: Vec<Result<StreamEvent, agent_shim_core::StreamError>> = vec![
            Ok(StreamEvent::ResponseStart {
                id: ResponseId::new(),
                model: "scripted-model".to_string(),
                created_at_unix: 0,
            }),
            Ok(StreamEvent::MessageStart { role: MessageRole::Assistant }),
            Ok(StreamEvent::ContentBlockStart { index: 0, kind: ContentBlockKind::Text }),
            Ok(StreamEvent::TextDelta { index: 0, text: "A".into() }),
            Ok(StreamEvent::TextDelta { index: 0, text: "B".into() }),
            Ok(StreamEvent::TextDelta { index: 0, text: "C".into() }),
            Ok(StreamEvent::TextDelta { index: 0, text: "D".into() }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                stop_sequence: None,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

const REQUEST_BODY: &str = r#"{
    "model": "test-model",
    "max_tokens": 16,
    "stream": true,
    "messages": [{"role": "user", "content": "hi"}]
}"#;

fn yaml_for_h5() -> &'static str {
    r#"
server:
  bind: 127.0.0.1
  port: 0
  keepalive_secs: 15
upstreams:
  scripted:
    type: open_ai_compatible
    base_url: "http://placeholder"
    api_key: "k"
    tier: standard
routes:
  - frontend: anthropic_messages
    model: test-model
    upstream: scripted
    upstream_model: test-model
    plugins:
      on_stream_event:
        - skipper
plugins:
  skipper:
    type: every_other_skipper
    on_error: fail
    config: {}
"#
}

#[tokio::test]
async fn h5_drops_alternate_stream_events() {
    use agent_shim_config::GatewayConfig;
    use agent_shim_gateway::{server::run_on_listener, state::AppState};

    // Build a custom PluginRegistry with the test-only factory + plugin.
    let plugin = Arc::new(EveryOtherEventSkipper {
        counter: std::sync::Mutex::new(0),
    });
    let entry = PluginEntry::test_helper("skipper", "every_other_skipper", plugin, OnError::Fail);
    let route_plan =
        agent_shim_plugins::test_helpers::single_route_plan(
            FrontendKind::AnthropicMessages,
            "test-model",
            vec![],
            vec![],
            vec![entry.clone()],
            vec![],
        );
    let registry = Arc::new(PluginRegistry::from_route_plan(route_plan));

    // Replace the upstream provider with our scripted one via gateway test hook.
    let cfg: GatewayConfig = serde_yaml::from_str(yaml_for_h5()).expect("yaml parses");
    let (state, _rx) = AppState::new_with_plugins_and_provider(cfg, registry, "scripted", Arc::new(ScriptedProvider))
        .await
        .expect("AppState::new");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let _server = tokio::spawn(async move {
        let _ = run_on_listener(listener, state, async { let _ = rx.await; }).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let url = format!("http://{}/v1/messages", addr);
    let client = eventsource_client::ClientBuilder::for_url(&url)
        .unwrap()
        .method("POST".into())
        .header("content-type", "application/json")
        .unwrap()
        .body(REQUEST_BODY.into())
        .build();

    let mut stream = client.stream();
    let mut events: Vec<String> = Vec::new();
    while let Some(Ok(SSE::Event(ev))) = stream.next().await {
        events.push(ev.event_type);
        if ev.event_type == "message_stop" || ev.event_type == "error" {
            break;
        }
    }

    let _ = tx.send(());

    // Provider emits 9 events. H5 skipper drops odd indices (1, 3, 5, 7) →
    // 5 events forwarded → SSE event types contain `message_start`,
    // `content_block_start`, `text_delta` only on even positions, etc.
    // Loose assertion: at most 5 SSE events of the canonical types observed.
    assert!(events.len() <= 5, "expected ≤ 5 forwarded events, got {events:?}");
    assert!(events.contains(&"message_start".to_string()) || events.contains(&"message_stop".to_string()),
            "expected at least the message_start or message_stop event survived, got {events:?}");
}
```

**⚠ Note:** The helpers `PluginEntry::test_helper`,
`agent_shim_plugins::test_helpers::single_route_plan`,
`PluginRegistry::from_route_plan`, and
`AppState::new_with_plugins_and_provider` may not exist in current code.
If they don't, T4 widens to include:

1. Adding the minimal test helpers in `crates/plugins/src/test_helpers.rs`
   (a `#[cfg(any(test, feature = "test-helpers"))]` module exposing the
   four functions). If a `test-helpers` feature gate is needed, add it
   under `[features]` in `crates/plugins/Cargo.toml`.
2. Extending `AppState` in `crates/gateway/src/state.rs` with
   `new_with_plugins_and_provider` (parallel to `new_with_plugins` but
   also registers a single provider Arc by name).

If the helpers do exist (built earlier in P05/P06), import them as
written.

To determine which path applies, before Step 2 do:

```bash
rtk grep -rn 'test_helper\|new_with_plugins_and_provider\|from_route_plan\|single_route_plan' crates/plugins/src crates/gateway/src
```

If any are missing, the subagent should propose helpers in a "T4-prep"
commit before writing the H5 test.

- [ ] **Step 3: Run the test**

Run: `rtk cargo nextest run -p agent-shim --test plugins_h5_stream h5_drops_alternate_stream_events`
Expected: PASS.

- [ ] **Step 4: Frozen-core check**

Run: `rtk git diff master..HEAD -- crates/core/`
Expected: empty.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/gateway/Cargo.toml crates/gateway/tests/plugins_h5_stream.rs
# plus any helper crates touched
rtk git commit -m "test(gateway): H5 drops alternate stream events (P07 T4)"
```

---

## Task 5: H5 mid-stream failure emits SSE error frame

**Goal:** Verify the failure path: when H5 returns `Err(...)` with `on_error: fail`, the gateway emits an Anthropic-style `event: error` SSE frame and closes the connection. The client must observe the error frame.

**Files:**
- Modify: `crates/gateway/tests/plugins_h5_stream.rs` — append a new test.

- [ ] **Step 1: Add the failure-injecting plugin + test**

In `crates/gateway/tests/plugins_h5_stream.rs`, append:

```rust
struct FailAfterThreePlugin {
    counter: std::sync::Mutex<u64>,
}

#[async_trait]
impl Plugin for FailAfterThreePlugin {
    fn kind_name(&self) -> &'static str { "fail_after_three" }
    fn hooks(&self) -> HookSet { HookSet::STREAM_EVENT }
    async fn on_stream_event(
        &self,
        _ctx: &PluginContext,
        event: StreamEvent,
    ) -> PluginResult<StreamEventDecision> {
        let mut c = self.counter.lock().unwrap();
        let n = *c;
        *c += 1;
        if n >= 3 {
            Err(PluginError { aborted: false, kind: "synthetic", source: None })
        } else {
            Ok(StreamEventDecision::Forward(event))
        }
    }
}

#[tokio::test]
async fn h5_failure_emits_sse_error_frame_and_closes() {
    use agent_shim_config::GatewayConfig;
    use agent_shim_gateway::{server::run_on_listener, state::AppState};

    let plugin = Arc::new(FailAfterThreePlugin {
        counter: std::sync::Mutex::new(0),
    });
    let entry = PluginEntry::test_helper("failer", "fail_after_three", plugin, OnError::Fail);
    let route_plan =
        agent_shim_plugins::test_helpers::single_route_plan(
            FrontendKind::AnthropicMessages,
            "test-model",
            vec![],
            vec![],
            vec![entry.clone()],
            vec![],
        );
    let registry = Arc::new(PluginRegistry::from_route_plan(route_plan));

    let cfg: GatewayConfig = serde_yaml::from_str(yaml_for_h5()).expect("yaml parses");
    let (state, _rx) = AppState::new_with_plugins_and_provider(cfg, registry, "scripted", Arc::new(ScriptedProvider))
        .await
        .expect("AppState::new");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let _server = tokio::spawn(async move {
        let _ = run_on_listener(listener, state, async { let _ = rx.await; }).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let url = format!("http://{}/v1/messages", addr);
    let client = eventsource_client::ClientBuilder::for_url(&url)
        .unwrap()
        .method("POST".into())
        .header("content-type", "application/json")
        .unwrap()
        .body(REQUEST_BODY.into())
        .build();

    let mut stream = client.stream();
    let mut events: Vec<(String, String)> = Vec::new();
    while let Some(Ok(SSE::Event(ev))) = stream.next().await {
        events.push((ev.event_type.clone(), ev.data.clone()));
        if ev.event_type == "error" || ev.event_type == "message_stop" {
            break;
        }
    }

    let _ = tx.send(());

    // First 3 events should pass; then an error frame.
    assert!(
        events.iter().any(|(k, _)| k == "error"),
        "expected an error frame, got events: {events:?}"
    );
    let error_data: &str = &events
        .iter()
        .find(|(k, _)| k == "error")
        .expect("error frame")
        .1;
    assert!(
        error_data.contains("plugin_failed"),
        "expected plugin_failed in error data, got: {error_data}"
    );
}
```

- [ ] **Step 2: Run the test**

Run: `rtk cargo nextest run -p agent-shim --test plugins_h5_stream h5_failure_emits_sse_error_frame_and_closes`
Expected: PASS.

- [ ] **Step 3: Run all plugins_h5_stream tests**

Run: `rtk cargo nextest run -p agent-shim --test plugins_h5_stream`
Expected: PASS — 2 tests.

- [ ] **Step 4: Frozen-core check**

Run: `rtk git diff master..HEAD -- crates/core/`
Expected: empty.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/gateway/tests/plugins_h5_stream.rs
rtk git commit -m "test(gateway): H5 mid-stream failure emits SSE error frame (P07 T5)"
```

---

## Task 6: `on_error: skip` swallows non-aborted plugin errors

**Goal:** Verify the `OnError::Skip` policy: a plugin returning `Err(_)` with `aborted: false` is silently dropped from the chain; upstream sees the un-modified canonical request and the gateway returns 200.

**Files:**
- Modify: `crates/gateway/tests/plugins_pipeline.rs` — append one test.

- [ ] **Step 1: Add the test**

At the bottom of `crates/gateway/tests/plugins_pipeline.rs`, append:

```rust
struct AlwaysErrorPlugin;

#[async_trait::async_trait]
impl agent_shim_plugins::Plugin for AlwaysErrorPlugin {
    fn kind_name(&self) -> &'static str { "always_error" }
    fn hooks(&self) -> HookSet { HookSet::DECODED_REQUEST }
    async fn on_decoded_request(
        &self,
        _ctx: &agent_shim_plugins::PluginContext,
        req: agent_shim_core::CanonicalRequest,
    ) -> agent_shim_plugins::PluginResult<agent_shim_core::CanonicalRequest> {
        // Borrow the existing PluginError fixture pattern in this test file.
        let _ = req;
        Err(agent_shim_plugins::PluginError {
            aborted: false,
            kind: "synthetic_error",
            source: None,
        })
    }
}

#[tokio::test]
async fn plugin_skip_on_error_returns_200() {
    let plugin = Arc::new(AlwaysErrorPlugin);
    let entry = agent_shim_plugins::PluginEntry::test_helper(
        "skipper",
        "always_error",
        plugin,
        agent_shim_plugins::OnError::Skip,
    );
    let route_plan = agent_shim_plugins::test_helpers::single_route_plan(
        agent_shim_core::FrontendKind::AnthropicMessages,
        "test-model",
        vec![entry],
        vec![],
        vec![],
        vec![],
    );
    let registry = Arc::new(agent_shim_plugins::PluginRegistry::from_route_plan(route_plan));

    // Reuse the existing test scaffolding helpers in plugins_pipeline.rs to
    // stand up gateway + mock provider. The exact helper name depends on
    // what's already in the file — the subagent should reuse the existing
    // pattern from `h2_plugin_fires_and_modifies_prompt`.
    let resp = run_request_with_registry(registry, REQUEST_BODY).await;
    assert_eq!(resp.status, 200, "Skip should swallow the error → 200");
    // The upstream should have received the un-modified prompt (the plugin
    // attempted no mutation before erroring).
    assert!(resp.upstream_body.contains("hi"), "upstream should see original prompt");
}
```

**⚠ Note:** This test refers to a `run_request_with_registry` helper. Inspect `plugins_pipeline.rs` and reuse whichever existing pattern (e.g. the test fixture used by `h2_plugin_fires_and_modifies_prompt`) provides start-server-with-this-registry-and-make-one-request behavior. If no such helper exists, factor one out from `h2_plugin_fires_and_modifies_prompt` into a `fn run_request_with_registry(registry: Arc<PluginRegistry>, body: &str) -> { status: StatusCode, upstream_body: String }` at the top of the file. Either way, do not invent new public APIs in `agent-shim-plugins` for this test.

- [ ] **Step 2: Run the test**

Run: `rtk cargo nextest run -p agent-shim --test plugins_pipeline plugin_skip_on_error_returns_200`
Expected: PASS.

- [ ] **Step 3: Run all plugins_pipeline tests**

Run: `rtk cargo nextest run -p agent-shim --test plugins_pipeline`
Expected: PASS — at least 8 tests now (was 7).

- [ ] **Step 4: Frozen-core check**

Run: `rtk git diff master..HEAD -- crates/core/`
Expected: empty.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/gateway/tests/plugins_pipeline.rs
rtk git commit -m "test(gateway): on_error: skip swallows non-aborted plugin error (P07 T6)"
```

---

## Task 7: Hot-reload happy path — `reload_swaps_plugins_atomically`

**Goal:** End-to-end: gateway starts with plugin config A; reload to config B; subsequent request observes config B's plugin behavior.

**Files:**
- Create: `crates/gateway/tests/plugins_reload.rs`.

- [ ] **Step 1: Create the test file**

Create `crates/gateway/tests/plugins_reload.rs`:

```rust
//! Phase 7 P07: Plugin hot-reload integration tests.
//!
//! Scenarios:
//!   1. `reload_swaps_plugins_atomically` (T7): happy path — reload replaces
//!      a plugin's behavior; subsequent requests see new behavior.
//!   2. `reload_with_bad_plugin_config_rejected` (T8): Layer-B failure
//!      rejects the entire reload; old plugins remain active.
//!   3. `reload_swap_isolates_in_flight_requests` (T9): an in-flight
//!      request bound to the old snapshot sees the old plugin; a sibling
//!      new request sees the new.

use agent_shim_config::GatewayConfig;
use agent_shim_gateway::{server::run_on_listener, state::AppState};

const REQUEST_BODY: &str = r#"{
    "model": "test-model",
    "max_tokens": 16,
    "messages": [{"role": "user", "content": "ping"}]
}"#;

const UPSTREAM_OAI_RESPONSE: &str = r#"{
    "id": "chatcmpl-test",
    "object": "chat.completion",
    "created": 1700000000,
    "model": "test-model",
    "choices": [{
        "index": 0,
        "message": {"role": "assistant", "content": "ok"},
        "finish_reason": "stop"
    }],
    "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
}"#;

fn yaml_v1(upstream_url: &str, marker: &str) -> String {
    format!(
        r#"
server:
  bind: 127.0.0.1
  port: 0
  keepalive_secs: 15
upstreams:
  mocked:
    type: open_ai_compatible
    base_url: "{upstream_url}"
    api_key: "k"
    tier: standard
routes:
  - frontend: anthropic_messages
    model: test-model
    upstream: mocked
    upstream_model: test-model
    plugins:
      on_decoded_request:
        - marker_inj
plugins:
  marker_inj:
    type: pii_scrubber
    on_error: fail
    config:
      inbound:
        - name: marker
          pattern: "ping"
          replacement: "{marker}"
"#
    )
}

#[tokio::test]
async fn reload_swaps_plugins_atomically() {
    let mut upstream = mockito::Server::new_async().await;

    // First request: upstream should see "MARKER-A".
    let mock_a = upstream
        .mock("POST", "/v1/chat/completions")
        .match_body(mockito::Matcher::Regex(r"MARKER-A".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(UPSTREAM_OAI_RESPONSE)
        .expect(1)
        .create_async()
        .await;
    // Second request (after reload): upstream should see "MARKER-B".
    let mock_b = upstream
        .mock("POST", "/v1/chat/completions")
        .match_body(mockito::Matcher::Regex(r"MARKER-B".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(UPSTREAM_OAI_RESPONSE)
        .expect(1)
        .create_async()
        .await;

    let cfg_a: GatewayConfig =
        serde_yaml::from_str(&yaml_v1(&upstream.url(), "MARKER-A")).expect("yaml parses");
    let (state, _rx) = AppState::new(cfg_a).await.expect("AppState::new");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server_state = state.clone();
    let _server = tokio::spawn(async move {
        let _ = run_on_listener(listener, server_state, async { let _ = rx.await; }).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Request 1 → should hit `MARKER-A` mock.
    let url = format!("http://{}/v1/messages", addr);
    let resp_a = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .body(REQUEST_BODY)
        .send()
        .await
        .expect("send 1");
    assert_eq!(resp_a.status(), 200);

    // Now reload with MARKER-B config.
    let yaml_b = yaml_v1(&upstream.url(), "MARKER-B");
    let outcome = agent_shim_gateway::commands::serve::handle_reload(
        &state,
        agent_shim_gateway::reload_trigger::ReloadSource::Yaml(yaml_b),
    )
    .await;
    assert!(
        matches!(
            outcome,
            agent_shim_gateway::reload_trigger::ReloadOutcome::Ok(_)
        ),
        "expected Ok reload, got {outcome:?}",
    );

    // Request 2 → should hit `MARKER-B` mock.
    let resp_b = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .body(REQUEST_BODY)
        .send()
        .await
        .expect("send 2");
    assert_eq!(resp_b.status(), 200);

    let _ = tx.send(());

    mock_a.assert_async().await;
    mock_b.assert_async().await;
}
```

- [ ] **Step 2: Verify `handle_reload` and `ReloadOutcome` are publicly accessible from tests**

Run: `rtk grep -n 'pub use.*commands\|pub mod commands\|pub use.*reload_trigger' crates/gateway/src/lib.rs`

If `commands::serve::handle_reload` is not publicly re-exported from
`agent_shim_gateway`, add a `#[doc(hidden)] pub use commands::serve::handle_reload;`
and `#[doc(hidden)] pub use reload_trigger::{ReloadSource, ReloadOutcome};`
to `crates/gateway/src/lib.rs`. Both with `#[doc(hidden)]` to mark
them as test-only API.

- [ ] **Step 3: Run the test**

Run: `rtk cargo nextest run -p agent-shim --test plugins_reload reload_swaps_plugins_atomically`
Expected: PASS.

- [ ] **Step 4: Frozen-core check**

Run: `rtk git diff master..HEAD -- crates/core/`
Expected: empty.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/gateway/tests/plugins_reload.rs crates/gateway/src/lib.rs
rtk git commit -m "test(gateway): reload_swaps_plugins_atomically (P07 T7)"
```

---

## Task 8: Hot-reload failure path — `reload_with_bad_plugin_config_rejected`

**Goal:** Reload with a YAML that references an unknown plugin kind. Assert: `ReloadOutcome::PluginValidation(...)`, HTTP 400 path, old plugin still active.

**Files:**
- Modify: `crates/gateway/tests/plugins_reload.rs` — append.

- [ ] **Step 1: Add the test**

Append:

```rust
#[tokio::test]
async fn reload_with_bad_plugin_config_rejected() {
    let mut upstream = mockito::Server::new_async().await;

    // Both requests (before AND after the bad reload) must show MARKER-A,
    // proving the reload was atomically rejected.
    let mock_a = upstream
        .mock("POST", "/v1/chat/completions")
        .match_body(mockito::Matcher::Regex(r"MARKER-A".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(UPSTREAM_OAI_RESPONSE)
        .expect(2)
        .create_async()
        .await;

    let cfg_a: GatewayConfig =
        serde_yaml::from_str(&yaml_v1(&upstream.url(), "MARKER-A")).expect("yaml parses");
    let (state, _rx) = AppState::new(cfg_a).await.expect("AppState::new");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server_state = state.clone();
    let _server = tokio::spawn(async move {
        let _ = run_on_listener(listener, server_state, async { let _ = rx.await; }).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let url = format!("http://{}/v1/messages", addr);
    let resp1 = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .body(REQUEST_BODY)
        .send()
        .await
        .expect("send 1");
    assert_eq!(resp1.status(), 200);

    // YAML with unknown plugin kind.
    let bad_yaml = format!(
        r#"
server:
  bind: 127.0.0.1
  port: 0
  keepalive_secs: 15
upstreams:
  mocked:
    type: open_ai_compatible
    base_url: "{}"
    api_key: "k"
    tier: standard
routes:
  - frontend: anthropic_messages
    model: test-model
    upstream: mocked
    upstream_model: test-model
    plugins:
      on_decoded_request:
        - bad
plugins:
  bad:
    type: this_kind_does_not_exist
    on_error: fail
    config: {{}}
"#,
        upstream.url()
    );

    let outcome = agent_shim_gateway::commands::serve::handle_reload(
        &state,
        agent_shim_gateway::reload_trigger::ReloadSource::Yaml(bad_yaml),
    )
    .await;
    match &outcome {
        agent_shim_gateway::reload_trigger::ReloadOutcome::PluginValidation(msg) => {
            assert!(msg.contains("unknown kind") || msg.contains("this_kind_does_not_exist"),
                    "expected unknown kind error, got: {msg}");
        }
        other => panic!("expected PluginValidation, got {other:?}"),
    }

    // After the rejected reload, request 2 still uses the old (MARKER-A) plugin.
    let resp2 = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .body(REQUEST_BODY)
        .send()
        .await
        .expect("send 2");
    assert_eq!(resp2.status(), 200);

    let _ = tx.send(());

    mock_a.assert_async().await;
}
```

- [ ] **Step 2: Run the test**

Run: `rtk cargo nextest run -p agent-shim --test plugins_reload reload_with_bad_plugin_config_rejected`
Expected: PASS.

- [ ] **Step 3: Run all plugins_reload tests**

Run: `rtk cargo nextest run -p agent-shim --test plugins_reload`
Expected: PASS — 2 tests.

- [ ] **Step 4: Frozen-core check**

Run: `rtk git diff master..HEAD -- crates/core/`
Expected: empty.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/gateway/tests/plugins_reload.rs
rtk git commit -m "test(gateway): bad plugin config rejects reload atomically (P07 T8)"
```

---

## Task 9: In-flight isolation under reload (`reload_swap_isolates_in_flight_requests`)

**Goal:** The hardest test. A barrier-synchronized R1 holds onto the old snapshot through a reload; R2 (sent after reload) sees the new plugin behavior; R1 (resumed after R2) still sees the old.

**Files:**
- Modify: `crates/gateway/tests/plugins_reload.rs` — append.

This test needs a custom barrier-holding plugin. Because the existing
built-in plugins don't sleep on a barrier, we register a custom
test-only plugin via the `new_with_plugins` path.

- [ ] **Step 1: Define the barrier plugin + test**

Append to `crates/gateway/tests/plugins_reload.rs`:

```rust
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Barrier;

/// Test-only plugin that mutates the prompt to inject MARKER, then waits
/// on a barrier so the test can synchronously schedule a reload while
/// this request is in-flight.
struct BarrierMarkerPlugin {
    marker: &'static str,
    barrier: Arc<Barrier>,
}

#[async_trait]
impl agent_shim_plugins::Plugin for BarrierMarkerPlugin {
    fn kind_name(&self) -> &'static str { "barrier_marker" }
    fn hooks(&self) -> agent_shim_plugins::HookSet {
        agent_shim_plugins::HookSet::DECODED_REQUEST
    }
    async fn on_decoded_request(
        &self,
        _ctx: &agent_shim_plugins::PluginContext,
        mut req: agent_shim_core::CanonicalRequest,
    ) -> agent_shim_plugins::PluginResult<agent_shim_core::CanonicalRequest> {
        // Inject the marker into the first user message's first text block.
        if let Some(msg) = req.messages.first_mut() {
            if let Some(block) = msg.content.first_mut() {
                if let agent_shim_core::ContentBlock::Text(t) = block {
                    t.text = format!("{}-{}", self.marker, t.text);
                }
            }
        }
        // Wait on the barrier so the test can drive the timeline.
        self.barrier.wait().await;
        Ok(req)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reload_swap_isolates_in_flight_requests() {
    let mut upstream = mockito::Server::new_async().await;

    // R1 must reach the upstream with `ORIGINAL-` prefix.
    let mock_original = upstream
        .mock("POST", "/v1/chat/completions")
        .match_body(mockito::Matcher::Regex(r"ORIGINAL-".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(UPSTREAM_OAI_RESPONSE)
        .expect(1)
        .create_async()
        .await;
    // R2 must reach the upstream with `RELOADED-` prefix.
    let mock_reloaded = upstream
        .mock("POST", "/v1/chat/completions")
        .match_body(mockito::Matcher::Regex(r"RELOADED-".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(UPSTREAM_OAI_RESPONSE)
        .expect(1)
        .create_async()
        .await;

    // Barrier of size 2: plugin + main thread.
    let barrier = Arc::new(Barrier::new(2));

    // Build initial registry with the ORIGINAL plugin.
    let original_plugin = Arc::new(BarrierMarkerPlugin {
        marker: "ORIGINAL",
        barrier: barrier.clone(),
    });
    let original_entry = agent_shim_plugins::PluginEntry::test_helper(
        "marker",
        "barrier_marker",
        original_plugin,
        agent_shim_plugins::OnError::Fail,
    );
    let original_plan = agent_shim_plugins::test_helpers::single_route_plan(
        agent_shim_core::FrontendKind::AnthropicMessages,
        "test-model",
        vec![original_entry],
        vec![],
        vec![],
        vec![],
    );
    let original_registry =
        Arc::new(agent_shim_plugins::PluginRegistry::from_route_plan(original_plan));

    // Minimal config: no plugins block needed because we override the registry.
    let cfg_min = format!(
        r#"
server:
  bind: 127.0.0.1
  port: 0
  keepalive_secs: 15
upstreams:
  mocked:
    type: open_ai_compatible
    base_url: "{}"
    api_key: "k"
    tier: standard
routes:
  - frontend: anthropic_messages
    model: test-model
    upstream: mocked
    upstream_model: test-model
"#,
        upstream.url()
    );
    let cfg: GatewayConfig = serde_yaml::from_str(&cfg_min).expect("yaml parses");
    let (state, _rx) = AppState::new_with_plugins(cfg, original_registry)
        .await
        .expect("AppState::new_with_plugins");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server_state = state.clone();
    let _server = tokio::spawn(async move {
        let _ = run_on_listener(listener, server_state, async { let _ = rx.await; }).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Spawn R1: enters dispatch with the ORIGINAL plugin; plugin blocks on barrier.
    let url = format!("http://{}/v1/messages", addr);
    let url_r1 = url.clone();
    let r1_handle = tokio::spawn(async move {
        reqwest::Client::new()
            .post(&url_r1)
            .header("content-type", "application/json")
            .body(REQUEST_BODY)
            .send()
            .await
            .expect("R1 send")
            .status()
    });
    // Give R1 enough time to enter dispatch and hit the barrier.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Build the RELOADED registry; swap it onto the snapshot directly
    // (bypassing handle_reload's YAML path — we want to keep the test
    // independent of YAML config; the swap mechanism is the same).
    let reloaded_barrier = Arc::new(Barrier::new(1));  // unused; R2 doesn't block
    let reloaded_plugin = Arc::new(BarrierMarkerPlugin {
        marker: "RELOADED",
        barrier: reloaded_barrier,
    });
    let reloaded_entry = agent_shim_plugins::PluginEntry::test_helper(
        "marker",
        "barrier_marker",
        reloaded_plugin,
        agent_shim_plugins::OnError::Fail,
    );
    let reloaded_plan = agent_shim_plugins::test_helpers::single_route_plan(
        agent_shim_core::FrontendKind::AnthropicMessages,
        "test-model",
        vec![reloaded_entry],
        vec![],
        vec![],
        vec![],
    );
    let reloaded_registry =
        Arc::new(agent_shim_plugins::PluginRegistry::from_route_plan(reloaded_plan));

    // Atomic swap of the snapshot's plugins. (Pull current snapshot,
    // build new one with same other fields but new plugins.)
    let current = state.snapshot.load_full();
    let new_snap = Arc::new(agent_shim_gateway::state::AppSnapshot {
        config: current.config.clone(),
        auth_enabled: current.auth_enabled,
        auth_required: current.auth_required,
        configured_key_hashes: current.configured_key_hashes.clone(),
        plugins: reloaded_registry,
    });
    state.snapshot.store(new_snap);

    // R2: sent AFTER the swap; should see RELOADED.
    let r2_status = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .body(REQUEST_BODY)
        .send()
        .await
        .expect("R2 send")
        .status();
    assert_eq!(r2_status, 200);

    // Release the barrier so R1 can resume.
    barrier.wait().await;
    let r1_status = r1_handle.await.expect("R1 join");
    assert_eq!(r1_status, 200);

    let _ = tx.send(());

    // The mocks verify R1 reached the upstream with ORIGINAL prefix
    // (proving it saw the old plugin) and R2 with RELOADED prefix.
    mock_original.assert_async().await;
    mock_reloaded.assert_async().await;
}
```

**⚠ Note:** This test depends on `AppSnapshot` being publicly accessible
(at least under `#[doc(hidden)]`) and on `state.snapshot` being a public
field. Both are true at master HEAD (verified). If clippy complains about
the direct-swap pattern bypassing `handle_reload`, add a comment
explaining the intent.

- [ ] **Step 2: Run the test**

Run: `rtk cargo nextest run -p agent-shim --test plugins_reload reload_swap_isolates_in_flight_requests`
Expected: PASS.

- [ ] **Step 3: Run all plugins_reload tests**

Run: `rtk cargo nextest run -p agent-shim --test plugins_reload`
Expected: PASS — 3 tests.

- [ ] **Step 4: Frozen-core check**

Run: `rtk git diff master..HEAD -- crates/core/`
Expected: empty.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/gateway/tests/plugins_reload.rs
rtk git commit -m "test(gateway): in-flight requests isolated from reload-time plugin swap (P07 T9)"
```

---

## Task 10: Empty-registry micro-benchmark

**Goal:** Replace the placeholder `crates/gateway/benches/gateway_overhead.rs` with a meaningful criterion micro-bench that measures empty-registry hook latency. Document the result snapshot.

**Files:**
- Create: `crates/plugins/benches/empty_registry_overhead.rs`.
- Modify: `crates/plugins/Cargo.toml` — add `[[bench]]` entry + `criterion` dev-dep.
- Create: `crates/plugins/benches/bench_results.md` — snapshot results.

- [ ] **Step 1: Add criterion as a dev-dep**

In `crates/plugins/Cargo.toml`:

```toml
[dev-dependencies]
# ... existing ...
criterion = { version = "0.5", features = ["async_tokio"] }

[[bench]]
name = "empty_registry_overhead"
harness = false
```

- [ ] **Step 2: Create the bench**

Create `crates/plugins/benches/empty_registry_overhead.rs`:

```rust
//! Phase 7 P07 micro-benchmark.
//!
//! Measures the latency of `PluginRegistry::empty().run_on_decoded_request()`
//! — the hot path for production deployments that don't configure plugins.
//! Target: < 1 µs / call on a 2024-era developer laptop.
//!
//! The "zero-overhead" claim in the v0.7.0 CHANGELOG rests on:
//!   1. `lookup()` returns `None` for empty registries (one hashmap miss).
//!   2. The early-return path doesn't allocate or clone the request.
//!
//! Why no e2e bench: the per-request HTTP+serde+axum cost dominates the
//! plugin-hook delta by 100×+. A microbench at the registry-call layer
//! gives a clean signal.

use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

use agent_shim_core::{
    CanonicalRequest, ContentBlock, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel,
    GenerationOptions, Message, RequestId, ResolvedPolicy, TextBlock,
};
use agent_shim_core::request::RequestMetadata;
use agent_shim_plugins::{PluginContext, PluginRegistry};

fn make_request() -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::AnthropicMessages,
            requested_model: FrontendModel::from("bench-model"),
        },
        model: FrontendModel::from("bench-model"),
        system: vec![],
        messages: vec![Message::user(vec![ContentBlock::Text(TextBlock {
            text: "bench prompt".to_string(),
            extensions: Default::default(),
        })])],
        tools: vec![],
        tool_choice: Default::default(),
        generation: GenerationOptions::default(),
        response_format: None,
        stream: false,
        metadata: RequestMetadata::default(),
        inbound_anthropic_headers: vec![],
        resolved_policy: ResolvedPolicy::default(),
        extensions: ExtensionMap::new(),
    }
}

fn make_ctx() -> PluginContext {
    PluginContext::new(
        RequestId::new(),
        FrontendKind::AnthropicMessages,
        "anthropic_messages/bench-model".to_string(),
    )
}

fn bench_empty_registry_h2(c: &mut Criterion) {
    let registry = Arc::new(PluginRegistry::empty());
    let ctx = make_ctx();
    let req = make_request();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("empty_registry");
    group.bench_function(BenchmarkId::new("run_on_decoded_request", "empty"), |b| {
        b.iter(|| {
            let registry = registry.clone();
            let ctx = ctx.clone();
            let req = req.clone();
            rt.block_on(async move {
                registry
                    .run_on_decoded_request(
                        (FrontendKind::AnthropicMessages, "bench-model"),
                        &ctx,
                        req,
                    )
                    .await
                    .unwrap()
            });
        });
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .measurement_time(Duration::from_secs(3))
        .sample_size(50);
    targets = bench_empty_registry_h2
}
criterion_main!(benches);
```

- [ ] **Step 3: Run the bench**

Run: `rtk cargo bench --bench empty_registry_overhead -p agent-shim-plugins -- --quick`
Expected: PASS. Note the median time. Should be < 1 µs (likely a few
hundred ns — `run_on_decoded_request` does a single hashmap lookup and
returns).

- [ ] **Step 4: Snapshot result**

Create `crates/plugins/benches/bench_results.md`:

```markdown
# Benchmark Snapshot

> Last run: <YYYY-MM-DD> on <hostname> / <CPU model>.

## empty_registry/run_on_decoded_request/empty

Median: <fill in from `cargo bench` output>

## Methodology

See `empty_registry_overhead.rs` and the P07 design spec §7.

The "zero-overhead" claim in CHANGELOG v0.7.0 rests on this single
microbench plus the early-return code path in
`crates/plugins/src/registry.rs::run_on_decoded_request` (and siblings).
```

Fill in `<YYYY-MM-DD>`, `<hostname>`, `<CPU model>`, and `<fill in>` with
the actual bench output. The subagent should run the bench, observe the
median, and commit the filled-in markdown.

- [ ] **Step 5: Replace placeholder bench file**

`crates/gateway/benches/gateway_overhead.rs` is a TODO-only file. Either:
- Delete it (preferred — `crates/plugins/benches/` is the right home), or
- Leave it with a comment saying "moved to crates/plugins/benches/".

Subagent's choice. The deletion path requires updating `crates/gateway/Cargo.toml` to remove the `[[bench]]` entry if it exists.

Run: `rtk grep -n 'gateway_overhead\|\[\[bench\]\]' crates/gateway/Cargo.toml`

If a `[[bench]]` entry exists, remove it. Then delete `crates/gateway/benches/gateway_overhead.rs`.

- [ ] **Step 6: Frozen-core check**

Run: `rtk git diff master..HEAD -- crates/core/`
Expected: empty.

- [ ] **Step 7: Commit**

```bash
rtk git add crates/plugins/Cargo.toml crates/plugins/benches/ crates/gateway/Cargo.toml
# (also `git rm crates/gateway/benches/gateway_overhead.rs` if you chose deletion)
rtk git commit -m "bench(plugins): empty registry overhead microbench + snapshot (P07 T10)"
```

---

## Task 11: README + architecture.md Plugin sections

**Goal:** Document the plugin system in user-facing docs.

**Files:**
- Modify: `README.md` — add top-level "Plugins" section.
- Modify: `docs/architecture.md` — add "Phase 7: Plugin system" section.

- [ ] **Step 1: Read the existing README**

Run: `rtk read README.md`

Identify the section anchors. The Plugins section should land after
"Configuration" (or equivalent), before "Operations / Deployment". If
no obvious place exists, append before the LICENSE section.

- [ ] **Step 2: Append the README Plugins section**

Add this section to README.md:

```markdown
## Plugins

AgentShim supports a small, opt-in plugin system for cross-cutting
request shaping: PII scrubbing, prompt compression, usage recording,
and custom logic via the trait surface.

### Hook anchors

| Hook | When | Use cases |
|---|---|---|
| `on_decoded_request` (H2) | After protocol decode, before route resolve | PII redaction, prompt compression |
| `on_resolved` (H3) | After backend target resolution | Target-specific shaping |
| `on_stream_event` (H5) | Per streamed response event | Output filtering, content moderation |
| `on_response_complete` (H7) | After full response received | Usage recording, audit logging |

### Built-in plugins (v1)

- `pii_scrubber` — regex-based PII redaction, inbound and outbound (Plan 07 P06b1).
- `prompt_compressor` — token-aware conversation history compression with three strategies (Plan 07 P06b2).
- `usage_recorder` — request/response token + cost recording to Prometheus or structured logs (Plan 07 P06a).

### Configuration example

```yaml
plugins:
  scrub:
    type: pii_scrubber
    on_error: fail
    config:
      inbound:
        - name: email
          pattern: "[\\w.]+@[\\w.]+\\.[a-z]{2,}"
          replacement: "[REDACTED-EMAIL]"
routes:
  - frontend: anthropic_messages
    model: claude-sonnet-4
    upstream: anthropic_primary
    upstream_model: claude-sonnet-4
    plugins:
      on_decoded_request:
        - scrub
```

Plugin configuration is hot-reloadable via SIGHUP or `POST /admin/reload`
— see "Reload semantics" below. Failed plugin validation atomically
rejects the entire reload; the previous configuration continues running.

### Disabling plugin support

Built-in plugins are behind Cargo features. To exclude them from your build:

```bash
cargo build -p agent-shim --no-default-features --features <subset>
```

Available feature flags: `usage_recorder`, `pii_scrubber`, `prompt_compressor`. All three are enabled by default.

### Writing custom plugins

See `crates/plugins/src/trait_def.rs` for the `Plugin` trait surface and `crates/plugins/src/builtin/` for production-quality example implementations.
```

- [ ] **Step 3: Read and update architecture.md**

Run: `rtk read docs/architecture.md`

Identify the existing Phase section structure (Phase 1 through Phase 6).

- [ ] **Step 4: Append Phase 7 section to architecture.md**

Add this section to `docs/architecture.md` after the Phase 6 section (the
existing convention is reverse-chronological — newest at the bottom):

```markdown
## Phase 7: Plugin system

Phase 7 added a first-class plugin system between the protocol-translation
edges and the chain walker. The design optimizes for two seemingly
competing goals: **zero overhead when no plugins are configured** (the
common case) and **strong observability when they are** (production
operability).

### Hook anchors

Four hooks instrument the request lifecycle:

```
HTTP request ─► decode_request (frontend)
                    │
                    ▼
                ┌─────────┐
                │   H2    │  on_decoded_request: see CanonicalRequest after decode
                └────┬────┘
                     ▼
                resolve route → BackendTarget
                     │
                ┌────┴────┐
                │   H3    │  on_resolved: see resolved BackendTarget
                └────┬────┘
                     ▼
                provider.complete() → CanonicalStream
                     │
                ┌────┴────┐
                │   H5    │  on_stream_event: per-event filter (1-in-N-out)
                └────┬────┘
                     ▼
                encode_stream → HTTP response
                     │
                ┌────┴────┐
                │   H7    │  on_response_complete: spawned after response sent
                └─────────┘
```

### Registry & dispatch

`PluginRegistry` owns the parsed plugin instances + a per-route
subscription plan. `PluginRegistry::build` is called at startup AND on
config reload (see "Hot reload" below). Each hook anchor in
`pipeline::dispatch` does a single hashmap lookup; empty subscription
lists early-return without allocating.

### Hot reload (Phase 7 P07)

`PluginRegistry` is bundled into `AppSnapshot`, the policy-bearing struct
managed by `arc-swap`. The reload flow is:

```
parse YAML
   ↓
validate_for_reload  (Layer A: immutable fields, upstream-set changes)
   ↓
PluginRegistry::build  (Layer B: kind lookup, factory parse, hook subscription)
   ↓
state.snapshot.store(new_snap)   ◄── commit point (atomic)
   ↓
limiter.store(new_limiter)
```

If Layer B fails, the entire reload is rejected and the old snapshot
(with its old plugins) keeps running. There is no partial commit.

In-flight requests bound an `Arc<AppSnapshot>` at the top of `dispatch`
(line 256). The arc-swap of the snapshot does not invalidate that bound
Arc — refcount remains > 0 until the request completes. The result: a
reload mid-request never changes plugin behavior for an in-flight
request, only for the next request that enters `dispatch`. This property
is regression-protected by
`crates/gateway/tests/plugins_reload.rs::reload_swap_isolates_in_flight_requests`.

### Failure semantics

The `Plugin::on_*` trait methods return `PluginResult<T>`, where
`PluginError` carries:
- `aborted: bool` — if true, abort the request (400 BadRequest);
  otherwise, treat per the plugin's `on_error` policy.
- `kind: &'static str` — short category (e.g. "synthetic", "timeout").
- `source: Option<Box<dyn Error>>` — optional cause for logs.

The `on_error` policy lives in the YAML config:
- `on_error: fail` (default) → plugin error short-circuits the chain
  with HTTP 502 (Bad Gateway), unless `aborted=true` (HTTP 400).
- `on_error: skip` → plugin error is swallowed; the chain continues
  with the un-modified request.

Per-hook timeouts are configurable (`timeout_ms`); default is 50 ms for
H2/H3/H7 and 5 ms for H5 (one event tick).
```

- [ ] **Step 5: Frozen-core check**

Run: `rtk git diff master..HEAD -- crates/core/`
Expected: empty.

- [ ] **Step 6: Commit**

```bash
rtk git add README.md docs/architecture.md
rtk git commit -m "docs: plugin system - README + architecture.md (P07 T11)"
```

---

## Task 12: Workspace version bump + CHANGELOG v0.7.0

**Goal:** Ship the release. Cargo workspace version moves to 0.7.0 and CHANGELOG gets a full v0.7.0 section.

**Files:**
- Modify: `Cargo.toml` (workspace root) — `version = "0.7.0"`.
- Modify: `CHANGELOG.md`.
- Modify: `docs/superpowers/plans/2026-05-14-phase-7-p03-p07-outline.md` — mark §P07 as promoted.

- [ ] **Step 1: Bump workspace version**

In the workspace root `Cargo.toml`:

```toml
[workspace.package]
version = "0.7.0"
```

Run: `rtk grep -n 'version' Cargo.toml`
Verify the version line is at line 1 or 2 of the workspace.package table.

- [ ] **Step 2: Verify all crates inherit `version.workspace = true`**

Run: `rtk grep -rn 'version\s*=' crates/*/Cargo.toml | grep -v 'workspace = true'`
Expected: any `version = "x.y.z"` lines listed should belong to dev-dep specifications (e.g. `criterion = { version = "0.5" }`), not to crate metadata. If any crate has a hardcoded crate version, change it to `version.workspace = true`.

- [ ] **Step 3: Read existing CHANGELOG.md**

Run: `rtk read CHANGELOG.md`

Identify the previous release header (likely `## [0.6.1]` or similar).

- [ ] **Step 4: Add the v0.7.0 section**

Add to `CHANGELOG.md` ABOVE the previous release:

```markdown
## [0.7.0] — 2026-05-22

Phase 7 complete. Plugin system goes live.

### Added (plugin system)

- **Plugin trait + registry** (P01-P02): `Plugin` trait surface with four
  hooks (`on_decoded_request`, `on_resolved`, `on_stream_event`,
  `on_response_complete`). `PluginRegistry` with per-route subscription
  plans and per-hook timeouts. Built atop a new `agent-shim-tokens`
  crate (cl100k token counting via tiktoken-rs).
- **Config integration** (P03): `plugins:` top-level YAML block and
  `routes[].plugins:` per-hook lists. Layer-A validation (undeclared
  references, timeout_ms == 0, duplicate names).
- **Pipeline integration** (P04): four anchor points in
  `pipeline::dispatch` wire the hooks at the correct moments. Universal
  `H7Guard` covers all three streaming frontends. `HandlerError::PluginFailed`
  with 400/502 envelope mapping.
- **Observability** (P05): `plugin.invoke` tracing spans,
  `agent_shim_plugin_invocations_total` and
  `agent_shim_plugin_duration_seconds` metrics. Shutdown flush of
  in-flight H7 spawns with `agent_shim_plugin_h7_dropped_at_shutdown_total`
  counter.
- **Registry builder** (P06a): `PluginRegistry::build` constructor
  performing Layer-B validation (kind lookup, factory parse, hook
  subscription mismatch). `FactoryDependencies` lets factories validate
  upstream references at startup.
- **`pii_scrubber`** (P06b1): regex-based PII redaction on H2 (inbound)
  and H5 (outbound). Behind the `pii_scrubber` Cargo feature
  (default-on). Per-rule match counter.
- **`prompt_compressor`** (P06b2): token-aware conversation compression
  with three strategies — `drop_old_turns`, `truncate_to_tokens`,
  `summarize_old_turns` (with upstream call + timeout + fallback).
  Behind the `prompt_compressor` Cargo feature (default-on).
- **`usage_recorder`** (P06a, completed in P05/P06a): on-H7
  Prometheus + log sinks for usage telemetry. Behind the
  `usage_recorder` Cargo feature (default-on).
- **Hot reload** (P07): `PluginRegistry` is bundled into `AppSnapshot`;
  reload-time rebuild via Layer-B validation. Failure rejects the
  entire reload atomically (no partial commit). In-flight requests
  observe the snapshot bound at the top of `dispatch`; subsequent
  reloads do not affect them.
- **Integration tests** (P07): H5 alternate-drop, H5 mid-stream error
  frame, `on_error: skip`, reload happy path, reload bad-config rejection,
  in-flight isolation under reload.
- **Bench** (P07): `empty_registry_overhead` criterion microbench
  protects the zero-overhead-empty-registry property.

### Changed

- `crates/plugins/Cargo.toml`: `agent-shim-tokens` is now optional and
  gated behind the `prompt_compressor` feature.
- `crates/gateway/src/state.rs`: `plugins` field moved from `AppCore`
  (immutable) to `AppSnapshot` (hot-swappable). All seven pipeline
  read-sites updated.
- `crates/gateway/src/reload_trigger.rs`: `ReloadOutcome` gains a
  `PluginValidation(String)` variant. `crates/gateway/src/admin/reload_handler.rs`
  maps it to HTTP 400.
- `agent_shim_config_reloads_total` counter gains a new `result` label
  value: `"plugin_validation_error"`.

### Internal

- New crates: `agent-shim-tokens`, `agent-shim-plugins`.
- `crates/core/` unchanged (frozen-core invariant respected throughout Phase 7).
- New dev-dependency: `eventsource-client` (Phase 7 P07; test-only).
- `crates/plugins/benches/empty_registry_overhead.rs` criterion bench.

### Documentation

- `README.md`: new top-level "Plugins" section.
- `docs/architecture.md`: new "Phase 7: Plugin system" section.

### Migration notes

- `Plugin::on_*` returning `Err(_)` triggers per-plugin `on_error`
  policy. The default `on_error: fail` short-circuits with 502; existing
  pre-v0.7 deployments without any plugin configured see no behavioral
  change (empty registry is a zero-overhead identity path).
- v0.6.x → v0.7.0: no required config changes for deployments without
  plugins. Plugin features can be opted into via the YAML `plugins:`
  block (see README).
```

- [ ] **Step 5: Mark the outline as promoted**

Edit `docs/superpowers/plans/2026-05-14-phase-7-p03-p07-outline.md`:
Replace the line containing the P07 outline title section header with
a link to the full plan and the design spec, e.g.:

```markdown
## P07 — Hot-reload + integration tests + benchmark

**Status:** ✅ Promoted to full plan: [`2026-05-22-phase-7-p07-hot-reload-and-tests.md`](2026-05-22-phase-7-p07-hot-reload-and-tests.md) (full spec at [`../specs/2026-05-22-phase-7-p07-hot-reload-and-tests-design.md`](../specs/2026-05-22-phase-7-p07-hot-reload-and-tests-design.md)).

(Outline below preserved for context but superseded by the full plan.)
```

The rest of the P07 outline content stays for archival purposes.

- [ ] **Step 6: Verify Cargo.lock**

Run: `rtk cargo build --workspace`
Expected: PASS. `Cargo.lock` should be re-generated.

- [ ] **Step 7: Frozen-core check**

Run: `rtk git diff master..HEAD -- crates/core/`
Expected: empty.

- [ ] **Step 8: Commit**

```bash
rtk git add Cargo.toml Cargo.lock CHANGELOG.md docs/superpowers/plans/2026-05-14-phase-7-p03-p07-outline.md
rtk git commit -m "release: v0.7.0 - Phase 7 plugin system + hot-reload (P07 T12)"
```

---

## Task 13: Acceptance gate — fmt + clippy + nextest + frozen-core

**Goal:** Run the full acceptance suite and commit any fmt diffs from the prior tasks.

**Files:** No new code; verification only.

- [ ] **Step 1: Format**

Run: `rtk cargo fmt --all`
Run: `rtk cargo fmt --all -- --check`
Expected: exit 0 (after running fmt and committing any diffs).

- [ ] **Step 2: Clippy with `-D warnings`**

Run: `rtk cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS — zero warnings.

If clippy complains about new code, fix in-place. If it complains
about pre-existing code that just got pulled into a new module, decide:
either fix it, or add a `#[allow(clippy::...)]` with a justifying comment
referring to P07. Do NOT silence warnings that point at real bugs.

- [ ] **Step 3: Full workspace test**

Run: `rtk cargo nextest run --workspace`
Expected: PASS — ≥ 920 tests (912 baseline + 2 H5 + 1 skip + 3 reload + 2 others ≈ ≥ 918 total; bumps depending on exact tally).

- [ ] **Step 4: Frozen-core check**

Run: `rtk git diff master..HEAD -- crates/core/`
Expected: empty.

- [ ] **Step 5: Feature-off build check (still passes for each built-in)**

Run: `rtk cargo build -p agent-shim-plugins --no-default-features`
Expected: PASS — minimum build (no built-ins, just the trait + registry surface).

Run: `rtk cargo build -p agent-shim-plugins --no-default-features --features pii_scrubber`
Run: `rtk cargo build -p agent-shim-plugins --no-default-features --features prompt_compressor`
Run: `rtk cargo build -p agent-shim-plugins --no-default-features --features usage_recorder`
Expected: PASS for each.

- [ ] **Step 6: Bench compile check**

Run: `rtk cargo bench --bench empty_registry_overhead -p agent-shim-plugins --no-run`
Expected: PASS — the bench compiles.

- [ ] **Step 7: Commit fmt cleanup if any**

```bash
rtk git status --short
```

If any `M` lines, fmt diffs need commit:

```bash
rtk git add -u
rtk git commit -m "chore: cargo fmt + clippy cleanup (P07 T13)"
```

If clean, skip the commit.

---

## Acceptance summary

After T13, all of these must hold:

1. `git log --oneline 1f2bfdf..HEAD` shows ~14 commits (12 task commits + 1 spec + 1 plan, plus optional fmt-cleanup).
2. `cargo nextest run --workspace` passes ≥ 920 tests.
3. `cargo fmt --all -- --check` is clean.
4. `cargo clippy --workspace --all-targets -- -D warnings` returns 0.
5. `git diff master..HEAD -- crates/core/` is empty.
6. `AppCore` no longer has `plugins`; `AppSnapshot` does.
7. `ReloadOutcome::PluginValidation` exists; reload endpoint returns 400 on plugin Layer-B failure.
8. Workspace `Cargo.toml` says `version = "0.7.0"`.
9. `CHANGELOG.md` has a complete v0.7.0 section.
10. `README.md` has a Plugins section.
11. `docs/architecture.md` has a Phase 7 section.
12. Outline file `2026-05-14-phase-7-p03-p07-outline.md` marks §P07 as ✅ Promoted.
13. `crates/plugins/benches/empty_registry_overhead.rs` runs successfully via `cargo bench --bench empty_registry_overhead -p agent-shim-plugins`.
14. `crates/plugins/benches/bench_results.md` has filled-in median timing.

When all 14 are satisfied: `git merge --no-ff worktree-phase-7-p07-hot-reload -m "Merge Phase 7 P07 + v0.7.0 release"` into master, then user pushes and tags `v0.7.0`.
