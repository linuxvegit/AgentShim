# Plan P02 — Plugin trait + registry skeleton (Phase 7)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-14-plugin-system-design.md`](../specs/2026-05-14-plugin-system-design.md) §3.3, §3.4, §4 (entire), §6.1-6.4. [`ADR-0008`](../../adr/0008-plugin-system.md) decisions (1)-(4).

**Goal:** Create the `agent-shim-plugins` crate. Define the `Plugin` and `PluginFactory` traits with their associated types (`PluginContext`, `HookSet`, `PluginError`, `PluginResult`, `ResponseSummary`, `UpstreamStatus`). Build a `PluginRegistry` skeleton with the per-frontend two-level route plan (§6.1, Q5 option C) and the `invoke()` template (§6.3) that handles timeout / on_error / clone-then-swap / protected-field diff. No plugin kinds are registered yet; no pipeline integration.

**Architecture:** New leaf crate at `crates/plugins/`, depending only on `agent-shim-core` and `agent-shim-tokens`. The public trait surface is `pub` from day one but its docs label it "unstable until v1.0". Registry construction takes a list of `PluginFactory` instances + a parsed config block (passed as serde_json::Value pairs to defer config-crate integration to P03). Registry exposes four `run_*` methods but the gateway is not yet calling them.

**Tech stack:** `async-trait` (already in workspace), `parking_lot` (already), `serde` / `serde_json` (already), `thiserror` (already), `tokio` (already), `futures` (already).

**Frozen-core impact:** None. New crate sits as a peer to `crates/core/`, depends on it but does not modify it. Phase 7's plans collectively diff core to empty.

**Test target:** ~15 unit tests inside the new crate covering: trait default no-op methods, HookSet bitflag arithmetic, registry construction errors, invoke template timeout/skip/fail/abort/protected-field paths, route_index fast-path lookup, wildcard fallback.

---

## File Structure

`crates/plugins/` (new):
- Create: `crates/plugins/Cargo.toml`
- Create: `crates/plugins/src/lib.rs` — crate-level docs + module declarations + public re-exports
- Create: `crates/plugins/src/trait_def.rs` — `Plugin` trait + `PluginFactory` trait + `HookSet` + `Hook` enum
- Create: `crates/plugins/src/context.rs` — `PluginContext`, `ResponseSummary`, `UpstreamStatus`
- Create: `crates/plugins/src/error.rs` — `PluginError`, `PluginResult`, `PluginConfigError`, `OnError`
- Create: `crates/plugins/src/registry.rs` — `PluginRegistry`, `PluginEntry`, `FrontendRoutePlans`, `RouteHookPlan`, construction + lookup
- Create: `crates/plugins/src/invoke.rs` — `invoke()` template + protected-field diff helper
- Create: `crates/plugins/src/builtin/mod.rs` — empty module; P06 fills this in

Workspace:
- Modify: `Cargo.toml` (workspace root) — add `crates/plugins` to `members`.

---

## Tasks

### Task 1: Crate skeleton

**Files:**
- Create: `crates/plugins/Cargo.toml`
- Create: `crates/plugins/src/lib.rs`
- Modify: workspace `Cargo.toml`

- [ ] **Step 1: Write `crates/plugins/Cargo.toml`**

```toml
[package]
name = "agent-shim-plugins"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
agent-shim-core = { path = "../core" }
agent-shim-tokens = { path = "../tokens" }
async-trait.workspace = true
futures.workspace = true
parking_lot.workspace = true
serde = { workspace = true, features = ["derive"] }
serde_json.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "sync", "time"] }
tracing.workspace = true

[dev-dependencies]
pretty_assertions.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "sync", "time", "test-util"] }
```

- [ ] **Step 2: Write the skeleton `crates/plugins/src/lib.rs`**

```rust
//! In-process Rust plugin system for AgentShim. See the design spec at
//! `docs/superpowers/specs/2026-05-14-plugin-system-design.md` and the
//! decision record in `docs/adr/0008-plugin-system.md`.
//!
//! # Stability
//!
//! The public trait surface (`Plugin`, `PluginFactory`, `PluginContext`,
//! `HookSet`, `PluginError`) is **not** semver-stable until AgentShim
//! reaches v1.0. Treat it as you would any other AgentShim public type
//! pre-1.0: contributions welcome, but breaking changes can happen in
//! minor releases.

#![forbid(unsafe_code)]

mod context;
mod error;
mod invoke;
mod registry;
mod trait_def;

pub mod builtin;

pub use context::{PluginContext, ResponseSummary, UpstreamStatus};
pub use error::{OnError, PluginConfigError, PluginError, PluginResult};
pub use registry::{HookTimeouts, PluginEntry, PluginRegistry, RegistryBuildError};
pub use trait_def::{Hook, HookSet, Plugin, PluginFactory};
```

- [ ] **Step 3: Add to workspace members**

Edit the repo-root `Cargo.toml`. Update `members`:

```toml
[workspace]
resolver = "2"
members = ["crates/core", "crates/config", "crates/observability", "crates/observability-derive", "crates/gateway", "crates/frontends", "crates/plugins", "crates/protocol-tests", "crates/providers", "crates/router", "crates/tokens"]
```

(Inserting `"crates/plugins"` before `"crates/protocol-tests"` alphabetically.)

- [ ] **Step 4: Verify build fails cleanly (expected — no source files yet)**

Run: `cargo check -p agent-shim-plugins`
Expected: errors about missing module files (`context.rs`, `error.rs`, etc.). That's correct — we'll add them next.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/plugins/Cargo.toml crates/plugins/src/lib.rs
git commit -m "feat(plugins): crate skeleton (P02 T1)"
```

---

### Task 2: Context types

**Files:**
- Create: `crates/plugins/src/context.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/plugins/src/context.rs`:

```rust
//! Request-level context handed to every plugin hook. Carries
//! request_id, frontend kind, and a route label. Does **not** carry
//! the plugin's own name — that lives in the `invoke()` template
//! locally and only enters logs / spans. (Plan 07 Q4.)

use agent_shim_core::{FrontendKind, RequestId, Usage};

/// Per-request metadata carried into every plugin hook.
///
/// `route_label` is the canonical string `<frontend_kind>/<model_alias>`
/// used by the rate-limit registry and metrics; plugins can read it for
/// logging/audit purposes but should not parse it.
#[derive(Debug, Clone)]
pub struct PluginContext {
    pub request_id: RequestId,
    pub frontend: FrontendKind,
    pub route_label: String,
    // Future-proofing: when this struct grows, add fields here. The
    // type is constructed only inside this crate's registry and the
    // pipeline; plugin code reads through `&PluginContext` so adding
    // fields is non-breaking.
}

/// Summary of a completed request. Handed to `Plugin::on_response_complete`
/// (H7). Carries only what is safe to read after the request finishes —
/// the underlying `CanonicalRequest` is dropped by then.
#[derive(Debug, Clone)]
pub struct ResponseSummary {
    pub usage: Option<Usage>,
    pub elapsed_ms: u64,
    pub upstream_status: UpstreamStatus,
}

/// Why the upstream call ended. Sufficient detail for observation
/// purposes; refined error categorisation is intentionally absent so
/// plugins don't drift into trying to retry or rebrand failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamStatus {
    Success,
    Error,
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{FrontendKind, RequestId};

    #[test]
    fn plugin_context_is_clone() {
        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "anthropic_messages/claude-sonnet".to_string(),
        };
        let _ = ctx.clone(); // compile-time check
    }

    #[test]
    fn response_summary_holds_optional_usage() {
        let summary = ResponseSummary {
            usage: None,
            elapsed_ms: 42,
            upstream_status: UpstreamStatus::Success,
        };
        assert_eq!(summary.elapsed_ms, 42);
        assert_eq!(summary.upstream_status, UpstreamStatus::Success);
        assert!(summary.usage.is_none());
    }
}
```

- [ ] **Step 2: Build**

Run: `cargo check -p agent-shim-plugins`
Expected: more errors about other missing modules. The `context` module compiles cleanly though — confirm by checking the error list does NOT mention `context.rs`.

- [ ] **Step 3: Run the context tests in isolation**

Run: `cargo test -p agent-shim-plugins context 2>&1 | head -50`
Expected: errors due to other modules. The plan is: when we finish Tasks 2-6, all modules compile and tests run.

- [ ] **Step 4: Commit**

```bash
git add crates/plugins/src/context.rs
git commit -m "feat(plugins): PluginContext, ResponseSummary, UpstreamStatus (P02 T2)"
```

---

### Task 3: Error types

**Files:**
- Create: `crates/plugins/src/error.rs`

- [ ] **Step 1: Write the module**

Create `crates/plugins/src/error.rs`:

```rust
//! Error types for the plugin system. Three distinct error categories:
//!
//! - `PluginError::Timeout` / `Failed` — internal plugin failure; honoured
//!   by `on_error` (`skip` or `fail`).
//! - `PluginError::Aborted` — plugin actively rejected the request; maps
//!   to HTTP 400, regardless of `on_error` setting.
//! - `PluginError::ProtectedFieldMutated` — plugin tried to change a
//!   routing-affecting field of `CanonicalRequest`. Treated as `Failed`
//!   for on_error purposes; logs at ERROR.
//!
//! See spec §4.3 / §4.5.

use serde::{Deserialize, Serialize};

/// `on_error` policy for a single plugin. Set per-plugin in YAML.
/// `skip` swallows internal failures (log + metric + continue); `fail`
/// propagates them and aborts the request. `Aborted` is not subject to
/// this knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnError {
    #[default]
    Skip,
    Fail,
}

pub type PluginResult<T> = Result<T, PluginError>;

/// Errors a plugin can return or that the invoke() template synthesises.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// Plugin exceeded its per-hook timeout. The plugin's future is
    /// dropped (cancelled). Honoured by `on_error`.
    #[error("plugin {plugin} timed out after {elapsed_ms}ms in {hook}")]
    Timeout {
        plugin: String,
        hook: &'static str,
        elapsed_ms: u64,
    },

    /// Plugin returned an error from its hook method. Honoured by
    /// `on_error`.
    #[error("plugin {plugin} failed in {hook}: {message}")]
    Failed {
        plugin: String,
        hook: &'static str,
        message: String,
    },

    /// Plugin actively rejected the request. NOT subject to `on_error`.
    /// Maps to HTTP 400 by the gateway pipeline.
    #[error("plugin {plugin} aborted request: {reason}")]
    Aborted { plugin: String, reason: String },

    /// Plugin mutated a routing-affecting field of `CanonicalRequest`.
    /// Detected by the `invoke()` template's post-call diff. Treated as
    /// `Failed` for `on_error` purposes (logs at ERROR; surfaces 502
    /// when `on_error: fail`).
    #[error("plugin {plugin} modified protected field `{field}` in {hook}")]
    ProtectedFieldMutated {
        plugin: String,
        hook: &'static str,
        field: &'static str,
    },
}

/// Errors raised by `PluginFactory::instantiate`. Carry the YAML path
/// of the failure so operators can find it.
#[derive(Debug, thiserror::Error)]
pub enum PluginConfigError {
    #[error("plugin `{plugin}` config: missing required field `{field}` at {path}")]
    MissingField {
        plugin: String,
        field: &'static str,
        path: String,
    },
    #[error("plugin `{plugin}` config: invalid value for `{field}`: {reason}")]
    InvalidValue {
        plugin: String,
        field: &'static str,
        reason: String,
    },
    #[error("plugin `{plugin}` config: deserialization failed: {0}")]
    Deserialize(#[source] serde_json::Error, /* plugin */ String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_error_default_is_skip() {
        assert_eq!(OnError::default(), OnError::Skip);
    }

    #[test]
    fn on_error_yaml_round_trip() {
        let s = serde_yaml::to_string(&OnError::Fail).unwrap();
        assert_eq!(s.trim(), "fail");
        let parsed: OnError = serde_yaml::from_str("skip").unwrap();
        assert_eq!(parsed, OnError::Skip);
    }

    #[test]
    fn plugin_error_display_format() {
        let e = PluginError::Timeout {
            plugin: "foo".to_string(),
            hook: "on_decoded_request",
            elapsed_ms: 51,
        };
        let s = e.to_string();
        assert!(s.contains("foo"));
        assert!(s.contains("51ms"));
        assert!(s.contains("on_decoded_request"));
    }
}
```

- [ ] **Step 2: Add `serde_yaml` to dev-dependencies if not present**

The test above uses `serde_yaml`. Check `crates/plugins/Cargo.toml`'s `[dev-dependencies]` — if not present:

```toml
[dev-dependencies]
pretty_assertions.workspace = true
serde_yaml = "0.9"
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "sync", "time", "test-util"] }
```

Add `serde_yaml = "0.9"` to workspace dependencies in repo-root `Cargo.toml` first if it's used by other crates; otherwise just a direct version is fine.

Check first: `grep -rn "serde_yaml" crates/*/Cargo.toml` — if other crates use `serde_yaml.workspace = true`, mirror that style; if they use a direct version string, do the same here.

- [ ] **Step 3: Commit**

```bash
git add crates/plugins/Cargo.toml crates/plugins/src/error.rs
git commit -m "feat(plugins): PluginError, PluginConfigError, OnError (P02 T3)"
```

---

### Task 4: Trait definitions

**Files:**
- Create: `crates/plugins/src/trait_def.rs`

- [ ] **Step 1: Write the module**

Create `crates/plugins/src/trait_def.rs`:

```rust
//! Plugin and PluginFactory traits. The four hooks (`on_decoded_request`,
//! `on_resolved`, `on_stream_event`, `on_response_complete`) all default
//! to no-op; plugins override only the ones they need.
//!
//! Per spec §4.1: hooks take owned `CanonicalRequest` and return owned
//! `CanonicalRequest`. The registry's `invoke()` template clones the
//! request before each plugin call (clone-then-swap, §6.4) so that an
//! `Err`-returning plugin never leaks a half-modified state.

use std::fmt;

use agent_shim_core::{BackendTarget, CanonicalRequest, StreamEvent};
use async_trait::async_trait;

use crate::context::{PluginContext, ResponseSummary};
use crate::error::{PluginConfigError, PluginResult};

/// One of the four lifecycle hooks. Used as a typed string in logs
/// (`plugin.hook` field) and in `Plugin::hooks()` subscription bitsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hook {
    DecodedRequest,
    Resolved,
    StreamEvent,
    ResponseComplete,
}

impl Hook {
    /// Stable string name. Used as the `plugin.hook` field in logs and
    /// metrics. MUST match the YAML key (`on_decoded_request` etc.).
    pub fn as_str(self) -> &'static str {
        match self {
            Hook::DecodedRequest => "on_decoded_request",
            Hook::Resolved => "on_resolved",
            Hook::StreamEvent => "on_stream_event",
            Hook::ResponseComplete => "on_response_complete",
        }
    }
}

impl fmt::Display for Hook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Bit-flag set of hooks a plugin subscribes to. Returned by
/// `Plugin::hooks()`. The registry consults this at construction time
/// to populate the route plan only on hooks the plugin actually wants.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HookSet(u8);

impl HookSet {
    pub const DECODED_REQUEST: Self = Self(1 << 0);
    pub const RESOLVED: Self = Self(1 << 1);
    pub const STREAM_EVENT: Self = Self(1 << 2);
    pub const RESPONSE_COMPLETE: Self = Self(1 << 3);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub fn contains(self, hook: Hook) -> bool {
        let bit = match hook {
            Hook::DecodedRequest => 1 << 0,
            Hook::Resolved => 1 << 1,
            Hook::StreamEvent => 1 << 2,
            Hook::ResponseComplete => 1 << 3,
        };
        (self.0 & bit) != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for HookSet {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for HookSet {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// The Plugin trait. One impl per plugin **kind**; each kind can be
/// instantiated many times under different names. All hook methods
/// default to no-op so plugins only override what they care about.
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    /// Globally-unique kind name. Matches the YAML `type:` field.
    /// Must be a `&'static str` so logs and spans can use it without
    /// allocation.
    fn kind_name(&self) -> &'static str;

    /// Hook subset this instance subscribes to. Decided at construction
    /// time (after the factory parses the per-plugin config). Returning
    /// an empty set is legal but means the plugin will never run.
    fn hooks(&self) -> HookSet;

    /// H2: after frontend decode, before route resolution.
    async fn on_decoded_request(
        &self,
        _ctx: &PluginContext,
        req: CanonicalRequest,
    ) -> PluginResult<CanonicalRequest> {
        Ok(req)
    }

    /// H3: after route + policy merge, before capability gate.
    async fn on_resolved(
        &self,
        _ctx: &PluginContext,
        req: CanonicalRequest,
        _target: &BackendTarget,
    ) -> PluginResult<CanonicalRequest> {
        Ok(req)
    }

    /// H5: per upstream→client stream event. Returns zero or more
    /// events; empty Vec = drop the event. Identity = `vec![event]`.
    async fn on_stream_event(
        &self,
        _ctx: &PluginContext,
        event: StreamEvent,
    ) -> PluginResult<Vec<StreamEvent>> {
        Ok(vec![event])
    }

    /// H7: after request completion. Fire-and-forget — return value is
    /// logged but does not affect the response. Spawned on the
    /// registry's internal JoinSet (P05); slow H7 plugins can be
    /// flushed at shutdown via `PluginRegistry::flush_pending_h7`.
    async fn on_response_complete(
        &self,
        _ctx: &PluginContext,
        _summary: &ResponseSummary,
    ) -> PluginResult<()> {
        Ok(())
    }
}

/// Factory for constructing `Plugin` instances from a parsed YAML
/// config block. One factory per plugin kind. Registered into
/// `PluginRegistry` at startup.
pub trait PluginFactory: Send + Sync + 'static {
    /// Kind name handled by this factory. Must match
    /// `Plugin::kind_name()` of the constructed instance.
    fn kind_name(&self) -> &'static str;

    /// Build a plugin instance.
    ///
    /// `plugin_name` is the YAML key (e.g. `compressor_for_deepseek`),
    /// used for error messages and logs.
    /// `config` is the raw `config:` map deserialised as a JSON value;
    /// factories own their internal config struct + serde
    /// deserialization.
    fn instantiate(
        &self,
        plugin_name: &str,
        config: serde_json::Value,
    ) -> Result<Box<dyn Plugin>, PluginConfigError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{FrontendKind, RequestId};

    struct NoopPlugin;

    #[async_trait]
    impl Plugin for NoopPlugin {
        fn kind_name(&self) -> &'static str {
            "noop"
        }
        fn hooks(&self) -> HookSet {
            HookSet::empty()
        }
    }

    #[test]
    fn hook_set_bit_or() {
        let s = HookSet::DECODED_REQUEST | HookSet::RESPONSE_COMPLETE;
        assert!(s.contains(Hook::DecodedRequest));
        assert!(!s.contains(Hook::Resolved));
        assert!(!s.contains(Hook::StreamEvent));
        assert!(s.contains(Hook::ResponseComplete));
        assert!(!s.is_empty());
    }

    #[test]
    fn hook_set_empty_is_empty() {
        assert!(HookSet::empty().is_empty());
        assert!(!HookSet::empty().contains(Hook::DecodedRequest));
    }

    #[test]
    fn hook_as_str_matches_yaml_keys() {
        assert_eq!(Hook::DecodedRequest.as_str(), "on_decoded_request");
        assert_eq!(Hook::Resolved.as_str(), "on_resolved");
        assert_eq!(Hook::StreamEvent.as_str(), "on_stream_event");
        assert_eq!(Hook::ResponseComplete.as_str(), "on_response_complete");
    }

    #[tokio::test]
    async fn default_methods_are_no_op() {
        let p = NoopPlugin;
        assert_eq!(p.kind_name(), "noop");
        assert!(p.hooks().is_empty());

        // Defaults return Ok(input)
        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "test/test".to_string(),
        };
        // Cheap stub: just verify the default impls compile & don't err.
        let event = StreamEvent::MessageStop;
        let out = p.on_stream_event(&ctx, event.clone()).await.unwrap();
        assert_eq!(out.len(), 1);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p agent-shim-plugins trait_def`
Expected: 4 tests pass.

If you hit a compile error referencing `StreamEvent::MessageStop`, look up the actual variant name in `crates/core/src/stream.rs` — variant names may have shifted. Pick any unit-data variant that exists and compiles.

- [ ] **Step 3: Commit**

```bash
git add crates/plugins/src/trait_def.rs
git commit -m "feat(plugins): Plugin trait + PluginFactory + Hook/HookSet (P02 T4)"
```

---

### Task 5: Invoke template (timeout + on_error + protected fields)

**Files:**
- Create: `crates/plugins/src/invoke.rs`

- [ ] **Step 1: Write the module**

Create `crates/plugins/src/invoke.rs`:

```rust
//! The `invoke()` template runs around every plugin call. It owns the
//! timeout, the on_error policy, the protected-field diff, and (in P05)
//! the logging + metrics. Plugin authors never see this code — they
//! just `Ok(...)` or `Err(...)` from their hook method, and the
//! template handles the rest.
//!
//! Spec §6.3 / §6.4 / §4.5.

use std::future::Future;
use std::time::{Duration, Instant};

use agent_shim_core::CanonicalRequest;

use crate::context::PluginContext;
use crate::error::{OnError, PluginError, PluginResult};

/// Outcome of a single invocation. Distinct from `PluginResult` because
/// the registry needs to know "swallowed by on_error: skip" (Ok(None))
/// vs "success" (Ok(Some(v))) vs "propagate" (Err).
pub(crate) enum InvokeOutcome<T> {
    /// Plugin ran successfully and returned a value the caller should
    /// apply (e.g. the rewritten CanonicalRequest).
    Success(T),
    /// Plugin failed but on_error: skip swallowed it. Caller keeps
    /// prior state.
    Skipped,
    /// Plugin aborted the request (HTTP 400) or on_error: fail
    /// propagated a failure (HTTP 502). Caller short-circuits.
    Propagate(PluginError),
}

/// Run a single plugin hook with the standard policy envelope.
///
/// `plugin_name`: YAML name of the plugin instance — used only for
/// logging fields, never returned to plugin code.
/// `hook`: stable hook string (`Hook::as_str()`).
/// `timeout_ms`: the per-hook timeout for this plugin.
/// `on_error`: the per-plugin on_error policy.
/// `work`: a future producing `PluginResult<T>`. The factory will
/// typically build this via a closure capturing the plugin's hook
/// method call.
pub(crate) async fn invoke<T, Fut>(
    plugin_name: &str,
    _ctx: &PluginContext,
    hook: &'static str,
    timeout_ms: u64,
    on_error: OnError,
    work: Fut,
) -> InvokeOutcome<T>
where
    Fut: Future<Output = PluginResult<T>>,
{
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_millis(timeout_ms), work).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(Ok(value)) => InvokeOutcome::Success(value),
        Ok(Err(PluginError::Aborted { plugin, reason })) => {
            // Aborted is NEVER subject to on_error — always propagate.
            InvokeOutcome::Propagate(PluginError::Aborted { plugin, reason })
        }
        Ok(Err(PluginError::ProtectedFieldMutated { plugin, hook: h, field })) => {
            // Same — programming error, always propagate (subject to
            // on_error mapping by the registry).
            match on_error {
                OnError::Skip => InvokeOutcome::Skipped,
                OnError::Fail => {
                    InvokeOutcome::Propagate(PluginError::ProtectedFieldMutated {
                        plugin,
                        hook: h,
                        field,
                    })
                }
            }
        }
        Ok(Err(err)) => match on_error {
            OnError::Skip => InvokeOutcome::Skipped,
            OnError::Fail => InvokeOutcome::Propagate(err),
        },
        Err(_) => {
            // Timeout elapsed before the future completed.
            let err = PluginError::Timeout {
                plugin: plugin_name.to_string(),
                hook,
                elapsed_ms,
            };
            match on_error {
                OnError::Skip => InvokeOutcome::Skipped,
                OnError::Fail => InvokeOutcome::Propagate(err),
            }
        }
    }
}

/// Check that a plugin did not mutate any of the four protected
/// fields. Returns `Err(ProtectedFieldMutated)` if it did, naming the
/// first field that changed.
///
/// Protected fields: `id`, `frontend`, `model`, `stream`. The four
/// fields that drive routing, identity, and pipeline branching.
/// (Spec §4.5 / Q3.)
pub(crate) fn check_protected_fields(
    plugin_name: &str,
    hook: &'static str,
    before: &CanonicalRequest,
    after: &CanonicalRequest,
) -> Result<(), PluginError> {
    if before.id != after.id {
        return Err(PluginError::ProtectedFieldMutated {
            plugin: plugin_name.to_string(),
            hook,
            field: "id",
        });
    }
    if before.frontend != after.frontend {
        return Err(PluginError::ProtectedFieldMutated {
            plugin: plugin_name.to_string(),
            hook,
            field: "frontend",
        });
    }
    if before.model != after.model {
        return Err(PluginError::ProtectedFieldMutated {
            plugin: plugin_name.to_string(),
            hook,
            field: "model",
        });
    }
    if before.stream != after.stream {
        return Err(PluginError::ProtectedFieldMutated {
            plugin: plugin_name.to_string(),
            hook,
            field: "stream",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        request::RequestMetadata, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel,
        GenerationOptions, RequestId, ResolvedPolicy,
    };

    fn req() -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: FrontendModel::from("m"),
            },
            model: FrontendModel::from("m"),
            system: vec![],
            messages: vec![],
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

    #[tokio::test]
    async fn invoke_success_returns_value() {
        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        let out: InvokeOutcome<i32> = invoke(
            "test",
            &ctx,
            "on_decoded_request",
            50,
            OnError::Skip,
            async { Ok(42) },
        )
        .await;
        match out {
            InvokeOutcome::Success(v) => assert_eq!(v, 42),
            _ => panic!("expected Success"),
        }
    }

    #[tokio::test]
    async fn invoke_failed_with_skip_returns_skipped() {
        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        let out: InvokeOutcome<i32> = invoke(
            "test",
            &ctx,
            "on_decoded_request",
            50,
            OnError::Skip,
            async {
                Err(PluginError::Failed {
                    plugin: "test".to_string(),
                    hook: "on_decoded_request",
                    message: "boom".to_string(),
                })
            },
        )
        .await;
        assert!(matches!(out, InvokeOutcome::Skipped));
    }

    #[tokio::test]
    async fn invoke_failed_with_fail_returns_propagate() {
        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        let out: InvokeOutcome<i32> = invoke(
            "test",
            &ctx,
            "on_decoded_request",
            50,
            OnError::Fail,
            async {
                Err(PluginError::Failed {
                    plugin: "test".to_string(),
                    hook: "on_decoded_request",
                    message: "boom".to_string(),
                })
            },
        )
        .await;
        assert!(matches!(out, InvokeOutcome::Propagate(_)));
    }

    #[tokio::test]
    async fn invoke_aborted_always_propagates() {
        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        // Even with on_error: Skip, Aborted propagates.
        let out: InvokeOutcome<i32> = invoke(
            "test",
            &ctx,
            "on_decoded_request",
            50,
            OnError::Skip,
            async {
                Err(PluginError::Aborted {
                    plugin: "test".to_string(),
                    reason: "policy".to_string(),
                })
            },
        )
        .await;
        match out {
            InvokeOutcome::Propagate(PluginError::Aborted { .. }) => {}
            _ => panic!("expected Propagate(Aborted)"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn invoke_timeout_with_skip_returns_skipped() {
        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        let out: InvokeOutcome<i32> = invoke(
            "test",
            &ctx,
            "on_decoded_request",
            10, // ms
            OnError::Skip,
            async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(42)
            },
        )
        .await;
        assert!(matches!(out, InvokeOutcome::Skipped));
    }

    #[test]
    fn protected_fields_pass_when_identical() {
        let a = req();
        let b = a.clone();
        assert!(check_protected_fields("p", "h", &a, &b).is_ok());
    }

    #[test]
    fn protected_fields_detect_model_change() {
        let a = req();
        let mut b = a.clone();
        b.model = FrontendModel::from("different");
        let err = check_protected_fields("p", "h", &a, &b).unwrap_err();
        match err {
            PluginError::ProtectedFieldMutated { field, .. } => assert_eq!(field, "model"),
            _ => panic!("expected ProtectedFieldMutated"),
        }
    }

    #[test]
    fn protected_fields_detect_id_change() {
        let a = req();
        let mut b = a.clone();
        b.id = RequestId::new();
        let err = check_protected_fields("p", "h", &a, &b).unwrap_err();
        match err {
            PluginError::ProtectedFieldMutated { field, .. } => assert_eq!(field, "id"),
            _ => panic!("expected ProtectedFieldMutated"),
        }
    }

    #[test]
    fn protected_fields_detect_stream_change() {
        let a = req();
        let mut b = a.clone();
        b.stream = !a.stream;
        let err = check_protected_fields("p", "h", &a, &b).unwrap_err();
        match err {
            PluginError::ProtectedFieldMutated { field, .. } => assert_eq!(field, "stream"),
            _ => panic!("expected ProtectedFieldMutated"),
        }
    }

    #[test]
    fn protected_fields_allow_messages_change() {
        let a = req();
        let mut b = a.clone();
        b.messages = vec![]; // any content change is fine
        b.system = vec![];
        // messages/system aren't protected → no error.
        assert!(check_protected_fields("p", "h", &a, &b).is_ok());
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p agent-shim-plugins invoke`
Expected: 9 tests pass.

If the timeout test (`invoke_timeout_with_skip_returns_skipped`) hangs, ensure `tokio` is configured with the `test-util` feature in `[dev-dependencies]` (see Task 1 Step 1).

- [ ] **Step 3: Commit**

```bash
git add crates/plugins/src/invoke.rs
git commit -m "feat(plugins): invoke() template + protected-field diff (P02 T5)"
```

---

### Task 6: Registry skeleton — types and construction

**Files:**
- Create: `crates/plugins/src/registry.rs`

- [ ] **Step 1: Write the module**

Create `crates/plugins/src/registry.rs`:

```rust
//! `PluginRegistry` — the gateway-facing surface of the plugin
//! system. Built at startup from a list of factories + parsed YAML
//! plugin entries. Owns the route_index used by `pipeline.rs` to
//! decide which plugins (if any) run for each request.
//!
//! Spec §6.1 / §6.2. Construction here is intentionally minimal —
//! the run_* methods come in P04 after pipeline integration is
//! designed; the JoinSet machinery for H7 spawn lifecycle comes in
//! P05.

use std::collections::HashMap;
use std::sync::Arc;

use agent_shim_core::FrontendKind;

use crate::error::{OnError, PluginConfigError};
use crate::trait_def::{Hook, HookSet, Plugin, PluginFactory};

/// Per-hook timeouts for a single plugin. Defaults follow spec §5.4:
/// 50 ms for H2/H3/H7, 5 ms for H5.
#[derive(Debug, Clone, Copy)]
pub struct HookTimeouts {
    pub on_decoded_request: u64,
    pub on_resolved: u64,
    pub on_stream_event: u64,
    pub on_response_complete: u64,
}

impl Default for HookTimeouts {
    fn default() -> Self {
        Self {
            on_decoded_request: 50,
            on_resolved: 50,
            on_stream_event: 5,
            on_response_complete: 50,
        }
    }
}

impl HookTimeouts {
    /// Apply a single value to every hook (the simple YAML form
    /// `timeout_ms: 50`).
    pub fn uniform(ms: u64) -> Self {
        Self {
            on_decoded_request: ms,
            on_resolved: ms,
            on_stream_event: ms,
            on_response_complete: ms,
        }
    }

    pub(crate) fn for_hook(&self, hook: Hook) -> u64 {
        match hook {
            Hook::DecodedRequest => self.on_decoded_request,
            Hook::Resolved => self.on_resolved,
            Hook::StreamEvent => self.on_stream_event,
            Hook::ResponseComplete => self.on_response_complete,
        }
    }
}

/// One entry in the registry, plus the policy knobs bound to it.
pub struct PluginEntry {
    pub name: String,
    pub plugin: Arc<dyn Plugin>,
    pub on_error: OnError,
    pub timeouts: HookTimeouts,
    pub enabled: bool,
}

/// All routes' per-hook ordered plugin lists, grouped by frontend.
/// The fast-path lookup is `plans.get(&frontend)` — when the result
/// is `None` or `is_empty == true`, the wrapper returns identity
/// without examining the inner maps (Q5 option C).
pub(crate) struct FrontendRoutePlans {
    pub(crate) specific: HashMap<String, RouteHookPlan>,
    pub(crate) wildcard: Option<RouteHookPlan>,
    pub(crate) is_empty: bool,
}

#[derive(Default, Clone)]
pub(crate) struct RouteHookPlan {
    pub(crate) on_decoded_request: Vec<Arc<PluginEntry>>,
    pub(crate) on_resolved: Vec<Arc<PluginEntry>>,
    pub(crate) on_stream_event: Vec<Arc<PluginEntry>>,
    pub(crate) on_response_complete: Vec<Arc<PluginEntry>>,
}

impl RouteHookPlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.on_decoded_request.is_empty()
            && self.on_resolved.is_empty()
            && self.on_stream_event.is_empty()
            && self.on_response_complete.is_empty()
    }
}

/// Top-level plugin registry. Constructed once at startup. Immutable
/// thereafter; reload rebuilds the whole thing and arc-swaps (Q12).
pub struct PluginRegistry {
    #[allow(dead_code)] // used by P04 run_* methods
    pub(crate) plugins: HashMap<String, Arc<PluginEntry>>,
    #[allow(dead_code)] // used by P04 run_* methods
    pub(crate) plans: HashMap<FrontendKind, FrontendRoutePlans>,
}

impl PluginRegistry {
    /// Build an empty registry — no plugins, no plans. Used as the
    /// fast-path default in tests and in YAML configs that omit
    /// `plugins:` entirely.
    pub fn empty() -> Self {
        Self {
            plugins: HashMap::new(),
            plans: HashMap::new(),
        }
    }

    /// Lookup helper used by P04's `run_*` methods. Returns the route
    /// plan matching `(frontend, model)` if any plugin actually subscribes
    /// to any hook for that route. Returns `None` for the fast path.
    pub(crate) fn lookup<'a>(
        &'a self,
        frontend: FrontendKind,
        model: &str,
    ) -> Option<&'a RouteHookPlan> {
        let fp = self.plans.get(&frontend)?;
        if fp.is_empty {
            return None;
        }
        // Specific match first, then wildcard.
        if let Some(plan) = fp.specific.get(model) {
            return Some(plan);
        }
        fp.wildcard.as_ref()
    }
}

/// Errors the registry surfaces during construction. Wrapped by
/// `gateway::main` into the boot-time error envelope.
#[derive(Debug, thiserror::Error)]
pub enum RegistryBuildError {
    #[error("plugin `{plugin}` references unknown kind `{kind}` (known: {known:?})")]
    UnknownKind {
        plugin: String,
        kind: String,
        known: Vec<String>,
    },
    #[error("plugin `{plugin}`: {0}")]
    Instantiation(#[source] PluginConfigError, /* plugin name */ String),
    #[error(
        "route `{frontend:?}/{model}` references plugin `{plugin}` on hook `{hook}`, but that \
         plugin does not subscribe to it (subscribed: {subscribed:?})"
    )]
    HookSubscriptionMismatch {
        frontend: FrontendKind,
        model: String,
        plugin: String,
        hook: &'static str,
        subscribed: Vec<&'static str>,
    },
}

// ── Construction ────────────────────────────────────────────────────────────
//
// The full constructor (`PluginRegistry::from_specs`) lives in P03 once
// the config crate exposes parsed plugin/route entries. For P02 we ship
// `empty()` plus the type machinery so that pipeline-integration tests
// in P04 can use `PluginRegistry::empty()` as the no-plugins baseline.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_lookup_returns_none() {
        let r = PluginRegistry::empty();
        assert!(r.lookup(FrontendKind::AnthropicMessages, "anything").is_none());
    }

    #[test]
    fn hook_timeouts_defaults() {
        let t = HookTimeouts::default();
        assert_eq!(t.on_decoded_request, 50);
        assert_eq!(t.on_resolved, 50);
        assert_eq!(t.on_stream_event, 5);
        assert_eq!(t.on_response_complete, 50);
    }

    #[test]
    fn hook_timeouts_uniform() {
        let t = HookTimeouts::uniform(20);
        assert_eq!(t.on_decoded_request, 20);
        assert_eq!(t.on_stream_event, 20);
    }

    #[test]
    fn hook_timeouts_for_hook_lookup() {
        let t = HookTimeouts {
            on_decoded_request: 100,
            on_resolved: 200,
            on_stream_event: 300,
            on_response_complete: 400,
        };
        assert_eq!(t.for_hook(Hook::DecodedRequest), 100);
        assert_eq!(t.for_hook(Hook::Resolved), 200);
        assert_eq!(t.for_hook(Hook::StreamEvent), 300);
        assert_eq!(t.for_hook(Hook::ResponseComplete), 400);
    }

    #[test]
    fn route_hook_plan_default_is_empty() {
        let plan = RouteHookPlan::default();
        assert!(plan.is_empty());
    }
}
```

- [ ] **Step 2: Create the empty `builtin/mod.rs` placeholder**

Plugins get filled in by P06. For P02 we just need the module to exist so the crate compiles:

Create `crates/plugins/src/builtin/mod.rs`:

```rust
//! Built-in plugin kinds. P02 ships only this module declaration;
//! P06 adds `prompt_compressor`, `pii_scrubber`, and `usage_recorder`
//! behind individual Cargo features.

// Placeholder. P06 adds:
//   #[cfg(feature = "plugin-prompt-compressor")]
//   pub mod prompt_compressor;
// etc., and a `register_builtin_plugins(factories: &mut Vec<...>)` fn.
```

- [ ] **Step 3: Run all tests**

Run: `cargo test -p agent-shim-plugins`
Expected: ~20 tests pass (context: 2, error: 3, trait_def: 4, invoke: 9, registry: 5). Exact count depends on what got added.

- [ ] **Step 4: Run clippy**

Run: `cargo clippy -p agent-shim-plugins --all-targets -- -D warnings`
Expected: clean. The `#[allow(dead_code)]` attributes on `plugins` and `plans` are intentional — they're used by P04.

- [ ] **Step 5: Commit**

```bash
git add crates/plugins/src/registry.rs crates/plugins/src/builtin/mod.rs
git commit -m "feat(plugins): PluginRegistry skeleton + RouteHookPlan + HookTimeouts (P02 T6)"
```

---

### Task 7: Workspace integration check

- [ ] **Step 1: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: all pre-existing tests pass + ~20 new tests from `agent-shim-plugins`. No regressions.

- [ ] **Step 2: Run clippy workspace-wide**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Run rustfmt check**

Run: `cargo fmt --all -- --check`
Expected: clean. If anything is misformatted, run `cargo fmt --all` and commit:

```bash
git add -u
git commit -m "style: cargo fmt (P02 T7)"
```

- [ ] **Step 4: Verify no dependency violations**

Run: `cargo deny check` (if in CI).
Expected: clean — the new crate adds no new external deps.

- [ ] **Step 5: No commit needed for verification**

The plan ends here. P03 picks up: config-crate integration and Layer A validation.

---

## Acceptance criteria

- `crates/plugins/` exists with `lib.rs`, `context.rs`, `error.rs`, `trait_def.rs`, `invoke.rs`, `registry.rs`, `builtin/mod.rs`.
- Public surface includes `Plugin`, `PluginFactory`, `Hook`, `HookSet`, `PluginContext`, `ResponseSummary`, `UpstreamStatus`, `PluginError`, `PluginConfigError`, `OnError`, `PluginRegistry`, `HookTimeouts`, `PluginEntry`, `RegistryBuildError`.
- `PluginRegistry::empty()` works; `lookup()` correctly returns `None` for the empty registry and any unknown frontend.
- `invoke()` template handles: success / failed+skip / failed+fail / aborted / protected-field-mutated / timeout. All paths covered by unit tests.
- `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check` all green.

## Notes for the implementer

- `Plugin::hooks()` returning `HookSet::empty()` is legal (compiles and runs) but useless — the plugin never gets called. P03's Layer A validation flags YAML that references such a plugin on any hook list.
- `RegistryBuildError::HookSubscriptionMismatch` exists in the type system now; the actual emission site is P03 (Layer A) and P06 (Layer B). Don't worry about wiring callers in P02.
- The `pub(crate)` visibility on `FrontendRoutePlans`, `RouteHookPlan`, and `lookup()` is deliberate — these are P04's pipeline-facing API surface, not user-facing. They'll get re-exported via crate-internal callbacks once `run_*` methods land.
- All four hook trait methods have default no-op impls — this is essential. Plugins that subscribe to only H2 should not need to write empty bodies for H3/H5/H7.
