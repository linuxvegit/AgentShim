# Phase 7 P06b1 — pii_scrubber Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the `pii_scrubber` built-in plugin with H2+H5 regex-based PII redaction, a per-request streaming buffer in `PluginContext.scratch`, eager regex compilation, and one observability counter.

**Architecture:** New plugin file (`crates/plugins/src/builtin/pii_scrubber.rs`) implements H2 (mutates inbound `Text`/`Reasoning` blocks) and H5 (buffered sliding-window scrub of `TextDelta`/`ReasoningDelta`, flushed on non-text events). Per-request state lives in a new `PluginContext.scratch` map. The new `regex` workspace dep is gated behind a `pii_scrubber` Cargo feature, default-on. Counter `agent_shim_plugin_pii_scrubber_matches_total{rule, direction}` is declared in the observability catalog.

**Tech Stack:** Rust 1.85+, `regex` 1.x, `tracing` 0.1, `metrics` (existing observability infra), `parking_lot::RwLock`, `std::any::Any` for typed scratch.

**Source spec:** `docs/superpowers/specs/2026-05-22-phase-7-p06b1-pii-scrubber-design.md` (commit `54f4bbd`).

---

## Pre-flight

Baseline: **855 tests passing** at P06a merge tip (commit `39c3f75`). After P06b1: expect ~876 tests (855 baseline + 21 new = 17 unit + 3 PluginContext.scratch + 1 integration).

Frozen-core invariant: `crates/core/` MUST be untouched. Acceptance check at end: `git diff master..HEAD -- crates/core/` empty.

ALWAYS prefix bash/git commands with `rtk` for token efficiency. Working dir: `Q:/src/AgentShim/.claude/worktrees/phase-7-p06b-builtins`.

---

## Task 1: Workspace `regex` dependency

**Files:**
- Modify: `Cargo.toml` (workspace root)

This task only edits the workspace Cargo.toml. No tests; verified by next task's compilation.

- [ ] **Step 1: Add `regex` to `[workspace.dependencies]`**

Open `Cargo.toml` at the workspace root. Locate `[workspace.dependencies]`. Insert `regex = "1"` in alphabetical position (likely between `reqwest` and `serde` or similar — verify the alphabetical neighbours).

- [ ] **Step 2: Verify workspace metadata parses**

Run: `rtk cargo metadata --format-version 1 --no-deps`
Expected: PASS — workspace metadata emits without error.

- [ ] **Step 3: Commit**

```bash
rtk git add Cargo.toml
rtk git commit -m "build: add regex 1 to workspace dependencies (P06b1 T1)"
```

---

## Task 2: plugins crate `pii_scrubber` feature flag

**Files:**
- Modify: `crates/plugins/Cargo.toml`

- [ ] **Step 1: Add feature + optional regex dep**

Open `crates/plugins/Cargo.toml`. Update `[features]` and `[dependencies]`:

```toml
[features]
default = ["usage_recorder", "pii_scrubber"]
usage_recorder = []
pii_scrubber = ["dep:regex"]
```

In `[dependencies]`, add (alphabetically, after the existing path deps and before `async-trait`):

```toml
regex = { workspace = true, optional = true }
```

- [ ] **Step 2: Verify build**

Run: `rtk cargo build -p agent-shim-plugins`
Expected: PASS — `regex` not yet referenced in code, but feature wiring compiles.

Run: `rtk cargo build -p agent-shim-plugins --no-default-features --features usage_recorder`
Expected: PASS — `pii_scrubber` feature off; `regex` dep NOT pulled in.

- [ ] **Step 3: Commit**

```bash
rtk git add crates/plugins/Cargo.toml Cargo.lock
rtk git commit -m "build(plugins): add pii_scrubber feature + optional regex dep (P06b1 T2)"
```

---

## Task 3: `PluginConfigError::InvalidValue.field` → `String`

**Files:**
- Modify: `crates/plugins/src/error.rs` (change type of `InvalidValue.field`)
- Modify: `crates/plugins/src/invoke.rs` (4 callsites)
- Modify: `crates/plugins/src/registry.rs` (1 callsite)
- Modify: `crates/plugins/src/builtin/usage_recorder.rs` (1 callsite)

Mechanical type change so that pii_scrubber (and future plugins) can use dynamic field paths like `inbound[3].name` without leaking `Box<str>`.

- [ ] **Step 1: Change the type in error.rs**

Open `crates/plugins/src/error.rs`. Find the `InvalidValue` variant (around line 78-82):

```rust
    InvalidValue {
        plugin: String,
        field: &'static str,
        reason: String,
    },
```

Change `field: &'static str` to `field: String`:

```rust
    InvalidValue {
        plugin: String,
        field: String,
        reason: String,
    },
```

- [ ] **Step 2: Run build to surface callsites**

Run: `rtk cargo build -p agent-shim-plugins`
Expected: FAIL — compile errors at each callsite using `field: <literal>`.

- [ ] **Step 3: Update `invoke.rs` callsites**

Open `crates/plugins/src/invoke.rs`. Find each `field: "..."` line (4 occurrences, around lines 213/220/227/234). Append `.to_string()` to each literal:

```rust
            field: "id".to_string(),
```
```rust
            field: "frontend".to_string(),
```
```rust
            field: "model".to_string(),
```
```rust
            field: "stream".to_string(),
```

- [ ] **Step 4: Update `registry.rs` callsite**

Open `crates/plugins/src/registry.rs`. Find `field: "strategy"` (around line 1694). Change to:

```rust
                field: "strategy".to_string(),
```

- [ ] **Step 5: Update `usage_recorder.rs` callsite**

Open `crates/plugins/src/builtin/usage_recorder.rs`. Find `field: "sink"` (around line 103). Change to:

```rust
                field: "sink".to_string(),
```

- [ ] **Step 6: Run build to verify clean**

Run: `rtk cargo build -p agent-shim-plugins`
Expected: PASS — no compile errors.

- [ ] **Step 7: Run plugins tests to verify no behavioural regression**

Run: `rtk cargo nextest run -p agent-shim-plugins`
Expected: PASS — same count as before (54 tests, all pass).

- [ ] **Step 8: Commit**

```bash
rtk git add crates/plugins/src/error.rs crates/plugins/src/invoke.rs crates/plugins/src/registry.rs crates/plugins/src/builtin/usage_recorder.rs
rtk git commit -m "refactor(plugins): PluginConfigError::InvalidValue.field to String (P06b1 T3)"
```

---

## Task 4: `PluginContext::new` + `scratch` field

**Files:**
- Modify: `crates/plugins/src/context.rs` (add `scratch` field + `new` constructor + `scratch_get_or_init` + `ScratchGuard`)

Add the scratch infrastructure to `PluginContext`. Tests in this task verify scratch behaviour; subsequent tasks update the ~19 callsites that struct-init `PluginContext` to use `PluginContext::new(...)` instead.

- [ ] **Step 1: Write failing tests**

Open `crates/plugins/src/context.rs`. Inside the existing `#[cfg(test)] mod tests` block, after the `response_summary_holds_optional_usage` test (or any final test), add:

```rust
    #[test]
    fn scratch_empty_by_default() {
        let ctx = PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/test".to_string(),
        );
        assert_eq!(ctx.scratch.read().len(), 0);
    }

    #[test]
    fn scratch_get_or_init_returns_typed_handle() {
        let ctx = PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/test".to_string(),
        );
        {
            let mut guard = ctx.scratch_get_or_init::<String, _>("plugin_a", || "hello".to_string());
            assert_eq!(&*guard, "hello");
            *guard = "mutated".to_string();
        }
        // Drop the guard before re-locking.
        {
            let guard = ctx.scratch_get_or_init::<String, _>("plugin_a", || "default".to_string());
            assert_eq!(&*guard, "mutated", "second call returns prior value, not re-init");
        }
    }

    #[test]
    fn scratch_cloned_context_shares_map() {
        let ctx = PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/test".to_string(),
        );
        {
            let mut guard = ctx.scratch_get_or_init::<u32, _>("counter", || 0u32);
            *guard = 42;
        }
        let cloned = ctx.clone();
        let guard = cloned.scratch_get_or_init::<u32, _>("counter", || 0u32);
        assert_eq!(*guard, 42, "Arc-clone semantics: scratch is shared with original");
    }
```

- [ ] **Step 2: Run tests to verify failure**

Run: `rtk cargo build -p agent-shim-plugins --tests`
Expected: FAIL — `PluginContext::new`, `scratch`, `scratch_get_or_init` do not exist.

- [ ] **Step 3: Rewrite `crates/plugins/src/context.rs`**

Replace the entire file contents with:

```rust
//! Request-level context handed to every plugin hook. Carries
//! request_id, frontend kind, route label, and a per-request scratch
//! map for plugins that need to keep state across hook invocations.
//!
//! The scratch field was added in P06b1 to support `pii_scrubber`'s H5
//! sliding-window buffer. See
//! `docs/superpowers/specs/2026-05-22-phase-7-p06b1-pii-scrubber-design.md`
//! §4.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use agent_shim_core::{FrontendKind, RequestId, Usage};

/// Per-request metadata carried into every plugin hook.
///
/// `route_label` is the canonical string `<frontend_kind>/<model_alias>`
/// used by the rate-limit registry and metrics; plugins can read it for
/// logging/audit purposes but should not parse it.
///
/// `scratch` is a typed key/value store keyed by plugin `kind_name()`
/// literals. Plugins MUST namespace entries by their own kind to avoid
/// collisions; access goes through `scratch_get_or_init::<T, _>` for
/// type-safe lookups. Cloning a `PluginContext` Arc-shares the same
/// scratch — H5 mutations survive across stream events because the
/// pipeline passes the same `PluginContext` Arc through every hook.
#[derive(Debug, Clone)]
pub struct PluginContext {
    pub request_id: RequestId,
    pub frontend: FrontendKind,
    pub route_label: String,
    pub(crate) scratch: Arc<parking_lot::RwLock<HashMap<&'static str, Box<dyn Any + Send + Sync>>>>,
}

impl PluginContext {
    /// Construct a new `PluginContext` with an empty scratch map.
    pub fn new(request_id: RequestId, frontend: FrontendKind, route_label: String) -> Self {
        Self {
            request_id,
            frontend,
            route_label,
            scratch: Arc::new(parking_lot::RwLock::new(HashMap::new())),
        }
    }

    /// Acquire a typed mutable reference to the per-request scratch slot
    /// for the given plugin kind. The `init` closure runs only when the
    /// slot is empty for this request.
    ///
    /// # Panics
    /// If the slot exists but holds a type other than `T`. This is a
    /// programmer error: two plugins claiming the same kind key. The
    /// registry's `kind_name()` uniqueness check (P06a
    /// `DuplicateFactoryKind`) prevents this in production.
    pub fn scratch_get_or_init<T, F>(&self, kind: &'static str, init: F) -> ScratchGuard<'_, T>
    where
        T: Send + Sync + 'static,
        F: FnOnce() -> T,
    {
        let mut map = self.scratch.write();
        map.entry(kind)
            .or_insert_with(|| Box::new(init()) as Box<dyn Any + Send + Sync>);
        ScratchGuard {
            map,
            kind,
            _t: std::marker::PhantomData,
        }
    }
}

/// RAII handle to a typed scratch slot. Holds the underlying RwLock
/// write guard for as long as it is in scope; subsequent calls to
/// `scratch_get_or_init` on the same `PluginContext` block until this
/// guard is dropped.
pub struct ScratchGuard<'a, T: 'static> {
    map: parking_lot::RwLockWriteGuard<'a, HashMap<&'static str, Box<dyn Any + Send + Sync>>>,
    kind: &'static str,
    _t: std::marker::PhantomData<T>,
}

impl<'a, T: 'static> std::ops::Deref for ScratchGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.map
            .get(self.kind)
            .expect("inserted above")
            .downcast_ref::<T>()
            .unwrap_or_else(|| {
                panic!(
                    "scratch key collision: kind `{}` holds the wrong type",
                    self.kind
                )
            })
    }
}

impl<'a, T: 'static> std::ops::DerefMut for ScratchGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.map
            .get_mut(self.kind)
            .expect("inserted above")
            .downcast_mut::<T>()
            .unwrap_or_else(|| {
                panic!(
                    "scratch key collision: kind `{}` holds the wrong type",
                    self.kind
                )
            })
    }
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

    #[test]
    fn plugin_context_is_clone() {
        let ctx = PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/claude-sonnet".to_string(),
        );
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

    #[test]
    fn scratch_empty_by_default() {
        let ctx = PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/test".to_string(),
        );
        assert_eq!(ctx.scratch.read().len(), 0);
    }

    #[test]
    fn scratch_get_or_init_returns_typed_handle() {
        let ctx = PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/test".to_string(),
        );
        {
            let mut guard = ctx.scratch_get_or_init::<String, _>("plugin_a", || "hello".to_string());
            assert_eq!(&*guard, "hello");
            *guard = "mutated".to_string();
        }
        {
            let guard = ctx.scratch_get_or_init::<String, _>("plugin_a", || "default".to_string());
            assert_eq!(&*guard, "mutated", "second call returns prior value, not re-init");
        }
    }

    #[test]
    fn scratch_cloned_context_shares_map() {
        let ctx = PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/test".to_string(),
        );
        {
            let mut guard = ctx.scratch_get_or_init::<u32, _>("counter", || 0u32);
            *guard = 42;
        }
        let cloned = ctx.clone();
        let guard = cloned.scratch_get_or_init::<u32, _>("counter", || 0u32);
        assert_eq!(*guard, 42, "Arc-clone semantics: scratch is shared with original");
    }
}
```

- [ ] **Step 4: Run tests**

Run: `rtk cargo nextest run -p agent-shim-plugins context::tests`
Expected: PASS — 5 tests (2 pre-existing + 3 new scratch tests).

(Other crate tests will FAIL because callsites still use struct-init. Task 5 fixes those.)

- [ ] **Step 5: Commit**

```bash
rtk git add crates/plugins/src/context.rs
rtk git commit -m "feat(plugins): PluginContext::new + scratch map for per-request state (P06b1 T4)"
```

---

## Task 5: Migrate all `PluginContext { ... }` struct-init callsites

**Files (test files inside plugins crate):**
- Modify: `crates/plugins/src/trait_def.rs` (1 site, line ~222)
- Modify: `crates/plugins/src/invoke.rs` (2 sites, lines ~272-273)
- Modify: `crates/plugins/src/registry.rs` (10 sites)
- Modify: `crates/plugins/src/builtin/usage_recorder.rs` (1 site, line ~265)

**Files (gateway production):**
- Modify: `crates/gateway/src/pipeline.rs` (3 sites, lines ~573, ~772, ~992)

Mechanical conversion from struct-init to `PluginContext::new(...)`. The `scratch` field is `pub(crate)` so external struct-init won't compile anyway — this task is forced by the type signature.

- [ ] **Step 1: Update `crates/plugins/src/trait_def.rs`**

Find the `PluginContext {` block in the test mod (line ~222):

```rust
        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "test/test".to_string(),
        };
```

Replace with:

```rust
        let ctx = PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "test/test".to_string(),
        );
```

- [ ] **Step 2: Update `crates/plugins/src/invoke.rs`**

Find the test helper `fn ctx() -> PluginContext { PluginContext { ... } }` around line 272-279:

```rust
    fn ctx() -> PluginContext {
        PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "test/test".to_string(),
        }
    }
```

Replace with:

```rust
    fn ctx() -> PluginContext {
        PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "test/test".to_string(),
        )
    }
```

- [ ] **Step 3: Update `crates/plugins/src/registry.rs`**

Locate each of the 10 sites at lines ~1011, ~1033, ~1112, ~1133, ~1162, ~1244, ~1327, ~1425, ~1454, ~1528. Each follows the pattern:

```rust
        let ctx = crate::PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: <LITERAL>.to_string(),
        };
```

Convert each to:

```rust
        let ctx = crate::PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            <LITERAL>.to_string(),
        );
```

(Use Read tool to verify each line, then Edit each one. Some may have slightly different formatting — preserve indentation.)

- [ ] **Step 4: Update `crates/plugins/src/builtin/usage_recorder.rs`**

Find the test helper around line 264:

```rust
    fn make_ctx() -> PluginContext {
        PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "anthropic_messages/test-model".to_string(),
        }
    }
```

Replace with:

```rust
    fn make_ctx() -> PluginContext {
        PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/test-model".to_string(),
        )
    }
```

- [ ] **Step 5: Update `crates/gateway/src/pipeline.rs`**

Three production sites at lines ~573, ~772, ~992. Each is `agent_shim_plugins::PluginContext { ... }`. Convert each to `agent_shim_plugins::PluginContext::new(...)`. Read each block first, preserve field-value expressions exactly (e.g. `request_id.clone()` may appear).

Example pattern at line 573:

```rust
    let plugin_ctx = agent_shim_plugins::PluginContext {
        request_id: request_id.clone(),
        frontend: frontend_kind,
        route_label: route_label.clone(),
    };
```

→

```rust
    let plugin_ctx = agent_shim_plugins::PluginContext::new(
        request_id.clone(),
        frontend_kind,
        route_label.clone(),
    );
```

- [ ] **Step 6: Run workspace build**

Run: `rtk cargo build --workspace --tests`
Expected: PASS — no callsite errors.

- [ ] **Step 7: Run full workspace tests**

Run: `rtk cargo nextest run --workspace`
Expected: PASS — 855 + 3 (new scratch tests) = 858. (Some test counts may shift if any test was inadvertently affected; the bar is "no failures.")

- [ ] **Step 8: Commit**

```bash
rtk git add crates/plugins/src/trait_def.rs crates/plugins/src/invoke.rs crates/plugins/src/registry.rs crates/plugins/src/builtin/usage_recorder.rs crates/gateway/src/pipeline.rs
rtk git commit -m "refactor: migrate PluginContext struct-init to ::new constructor (P06b1 T5)"
```

---

## Task 6: Observability metrics catalog entry

**Files:**
- Modify: `crates/observability/src/metrics/catalog.rs` (add `PluginPiiScrubberMatchesTotal` marker)

- [ ] **Step 1: Inspect existing markers**

Read `crates/observability/src/metrics/catalog.rs` for the existing pattern. Markers look like:

```rust
#[derive(Metric)]
#[metric(name = "...", kind = "counter", help = "...")]
pub struct SomeName;
```

- [ ] **Step 2: Add the new marker**

Append (or insert in an appropriate alphabetical position within the existing markers) the following to `crates/observability/src/metrics/catalog.rs`:

```rust
#[derive(Metric)]
#[metric(
    name = "agent_shim_plugin_pii_scrubber_matches_total",
    kind = "counter",
    help = "Total PII scrub rule matches, labeled by rule name and direction (inbound|outbound)."
)]
pub struct PluginPiiScrubberMatchesTotal;
```

- [ ] **Step 3: Verify the structural-parity test still passes**

Run: `rtk cargo nextest run -p agent-shim-observability`
Expected: PASS — including the structural-parity test that walks `METRIC_DESCRIPTORS` and checks every entry was declared once.

- [ ] **Step 4: Commit**

```bash
rtk git add crates/observability/src/metrics/catalog.rs
rtk git commit -m "feat(observability): declare plugin_pii_scrubber_matches_total counter (P06b1 T6)"
```

---

## Task 7: `pii_scrubber.rs` — config types + factory

**Files:**
- Create: `crates/plugins/src/builtin/pii_scrubber.rs` (config types, factory, partial Plugin impl returning `Ok(req)`)
- Modify: `crates/plugins/src/builtin/mod.rs` (declare new module + register factory)

This task adds the file with only deserialization + factory logic. Hook bodies are stubs (`Ok(req)` / `Ok(vec![event])`) that Task 8 + Task 9 fill in.

- [ ] **Step 1: Declare module + register factory**

Edit `crates/plugins/src/builtin/mod.rs` to add the new module under feature gate and register the factory:

```rust
//! Built-in plugin kinds. Each kind is feature-gated.
//!
//! Wire-up: `builtin_plugins()` returns the compiled-in built-in
//! factories. Gateway calls this once during `AppCore::build` before
//! invoking `PluginRegistry::build`.

#[cfg(feature = "usage_recorder")]
pub mod usage_recorder;

#[cfg(feature = "pii_scrubber")]
pub mod pii_scrubber;

use std::sync::Arc;

use crate::PluginFactory;

/// Return every built-in plugin factory compiled into this binary.
/// Operators opt out via Cargo features at compile time.
///
/// Order is alphabetical for predictable diagnostic output (e.g. the
/// `known` list in `RegistryBuildError::UnknownKind`).
pub fn builtin_plugins() -> Vec<Arc<dyn PluginFactory>> {
    let mut factories: Vec<Arc<dyn PluginFactory>> = Vec::new();
    #[cfg(feature = "pii_scrubber")]
    factories.push(Arc::new(pii_scrubber::PiiScrubberFactory));
    #[cfg(feature = "usage_recorder")]
    factories.push(Arc::new(usage_recorder::UsageRecorderFactory));
    factories
}
```

(Note: `pii_scrubber` is pushed before `usage_recorder` because `p` < `u` alphabetically.)

- [ ] **Step 2: Create the new file with config types + factory + Plugin stub**

Create `crates/plugins/src/builtin/pii_scrubber.rs` with the following content:

```rust
//! `pii_scrubber` built-in plugin — H2 + H5 regex-based PII redaction.
//!
//! Spec: `docs/superpowers/specs/2026-05-22-phase-7-p06b1-pii-scrubber-design.md`.

#![cfg(feature = "pii_scrubber")]

use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashSet;

use agent_shim_core::{CanonicalRequest, ContentBlock, StreamEvent};

use crate::context::{PluginContext, ResponseSummary};
use crate::error::{PluginConfigError, PluginResult};
use crate::trait_def::{HookSet, Plugin, PluginFactory};

// ─── Config types ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PiiScrubberConfig {
    #[serde(default)]
    pub inbound: Vec<RuleConfig>,
    #[serde(default)]
    pub outbound: Vec<RuleConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    pub name: String,
    pub pattern: String,
    pub replacement: String,
    #[serde(default)]
    pub case_insensitive: bool,
    #[serde(default)]
    pub multiline: bool,
}

// ─── Compiled internal state ─────────────────────────────────────────────

struct CompiledRule {
    name: String,
    regex: regex::Regex,
    replacement: String,
}

pub struct PiiScrubber {
    inbound: Vec<CompiledRule>,
    outbound: Vec<CompiledRule>,
}

// ─── Constants for H5 buffering (used in T9) ─────────────────────────────

const MAX_PATTERN_TAIL: usize = 64;
const FLUSH_THRESHOLD: usize = 256;

// ─── Factory ────────────────────────────────────────────────────────────

pub struct PiiScrubberFactory;

impl PluginFactory for PiiScrubberFactory {
    fn kind_name(&self) -> &'static str {
        "pii_scrubber"
    }

    fn instantiate(
        &self,
        plugin_name: &str,
        config: serde_json::Value,
    ) -> Result<Box<dyn Plugin>, PluginConfigError> {
        let cfg: PiiScrubberConfig = serde_json::from_value(config)
            .map_err(|e| PluginConfigError::Deserialize(e, plugin_name.to_string()))?;

        let inbound = compile_rules(plugin_name, "inbound", &cfg.inbound)?;
        let outbound = compile_rules(plugin_name, "outbound", &cfg.outbound)?;

        if inbound.is_empty() && outbound.is_empty() {
            tracing::warn!(
                plugin = plugin_name,
                "pii_scrubber instantiated with empty inbound and outbound — no scrubbing will occur"
            );
        }

        Ok(Box::new(PiiScrubber { inbound, outbound }))
    }
}

fn compile_rules(
    plugin_name: &str,
    direction: &'static str,
    rules: &[RuleConfig],
) -> Result<Vec<CompiledRule>, PluginConfigError> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(rules.len());

    for (i, r) in rules.iter().enumerate() {
        if !is_valid_rule_name(&r.name) {
            return Err(PluginConfigError::InvalidValue {
                plugin: plugin_name.to_string(),
                field: format!("{direction}[{i}].name"),
                reason: format!(
                    "rule name must match ^[A-Za-z0-9_-]{{1,64}}$, got `{}`",
                    r.name
                ),
            });
        }
        if !seen.insert(r.name.clone()) {
            return Err(PluginConfigError::InvalidValue {
                plugin: plugin_name.to_string(),
                field: format!("{direction}[{i}].name"),
                reason: format!("duplicate rule name `{}`", r.name),
            });
        }
        let mut flag = String::new();
        if r.case_insensitive {
            flag.push('i');
        }
        if r.multiline {
            flag.push('m');
        }
        let final_pattern = if flag.is_empty() {
            r.pattern.clone()
        } else {
            format!("(?{flag}){}", r.pattern)
        };
        let regex = regex::Regex::new(&final_pattern).map_err(|e| {
            PluginConfigError::InvalidValue {
                plugin: plugin_name.to_string(),
                field: format!("{direction}[{i}].pattern"),
                reason: format!("invalid regex `{}`: {e}", r.pattern),
            }
        })?;
        out.push(CompiledRule {
            name: r.name.clone(),
            regex,
            replacement: r.replacement.clone(),
        });
    }
    Ok(out)
}

fn is_valid_rule_name(s: &str) -> bool {
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

// ─── Plugin trait impl (stubs filled in by T8/T9) ────────────────────────

#[async_trait]
impl Plugin for PiiScrubber {
    fn kind_name(&self) -> &'static str {
        "pii_scrubber"
    }

    fn hooks(&self) -> HookSet {
        let mut h = HookSet::empty();
        if !self.inbound.is_empty() {
            h |= HookSet::DECODED_REQUEST;
        }
        if !self.outbound.is_empty() {
            h |= HookSet::STREAM_EVENT;
        }
        h
    }

    async fn on_decoded_request(
        &self,
        _ctx: &PluginContext,
        req: CanonicalRequest,
    ) -> PluginResult<CanonicalRequest> {
        // Filled in by T8.
        Ok(req)
    }

    async fn on_stream_event(
        &self,
        _ctx: &PluginContext,
        event: StreamEvent,
    ) -> PluginResult<Vec<StreamEvent>> {
        // Filled in by T9.
        Ok(vec![event])
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Config parsing ─────────────────────────────────────────────────

    #[test]
    fn config_defaults() {
        let cfg: PiiScrubberConfig = serde_json::from_value(json!({})).unwrap();
        assert!(cfg.inbound.is_empty());
        assert!(cfg.outbound.is_empty());
    }

    #[test]
    fn config_deny_unknown_field() {
        let result: Result<PiiScrubberConfig, _> = serde_json::from_value(json!({"extra": 1}));
        assert!(result.is_err(), "deny_unknown_fields must reject unknown keys");
    }

    // ── Factory ────────────────────────────────────────────────────────

    #[test]
    fn factory_kind_name() {
        assert_eq!(PiiScrubberFactory.kind_name(), "pii_scrubber");
    }

    #[test]
    fn factory_compiles_valid_regex() {
        let cfg = json!({
            "inbound": [
                { "name": "email", "pattern": r"[\w.]+@[\w.]+", "replacement": "[X]" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).expect("should compile");
        assert_eq!(plugin.kind_name(), "pii_scrubber");
        assert!(plugin.hooks().contains(crate::Hook::DecodedRequest));
        assert!(!plugin.hooks().contains(crate::Hook::StreamEvent));
    }

    #[test]
    fn factory_rejects_invalid_regex() {
        let cfg = json!({
            "inbound": [
                { "name": "bad", "pattern": "[unclosed", "replacement": "X" }
            ]
        });
        let err = PiiScrubberFactory.instantiate("p", cfg).unwrap_err();
        match err {
            PluginConfigError::InvalidValue { field, reason, .. } => {
                assert_eq!(field, "inbound[0].pattern");
                assert!(reason.contains("invalid regex"), "reason was: {reason}");
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    #[test]
    fn factory_rejects_duplicate_rule_name() {
        let cfg = json!({
            "inbound": [
                { "name": "email", "pattern": "a", "replacement": "x" },
                { "name": "email", "pattern": "b", "replacement": "y" }
            ]
        });
        let err = PiiScrubberFactory.instantiate("p", cfg).unwrap_err();
        match err {
            PluginConfigError::InvalidValue { field, reason, .. } => {
                assert_eq!(field, "inbound[1].name");
                assert!(reason.contains("duplicate"), "reason was: {reason}");
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    #[test]
    fn factory_rejects_invalid_name_char() {
        let cfg = json!({
            "inbound": [
                { "name": "my rule", "pattern": "a", "replacement": "x" }
            ]
        });
        let err = PiiScrubberFactory.instantiate("p", cfg).unwrap_err();
        assert!(matches!(err, PluginConfigError::InvalidValue { .. }));
    }

    #[test]
    fn factory_rejects_overlong_name() {
        let long_name = "a".repeat(65);
        let cfg = json!({
            "inbound": [
                { "name": long_name, "pattern": "a", "replacement": "x" }
            ]
        });
        let err = PiiScrubberFactory.instantiate("p", cfg).unwrap_err();
        assert!(matches!(err, PluginConfigError::InvalidValue { .. }));
    }

    #[test]
    fn hooks_minimal_subscription_outbound_only() {
        let cfg = json!({
            "outbound": [
                { "name": "x", "pattern": "a", "replacement": "y" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).unwrap();
        let h = plugin.hooks();
        assert!(!h.contains(crate::Hook::DecodedRequest));
        assert!(h.contains(crate::Hook::StreamEvent));
    }

    #[test]
    fn hooks_empty_subscription_on_empty_config() {
        let plugin = PiiScrubberFactory.instantiate("p", json!({})).unwrap();
        assert_eq!(plugin.hooks(), HookSet::empty());
    }
}
```

- [ ] **Step 3: Run the new tests**

Run: `rtk cargo nextest run -p agent-shim-plugins pii_scrubber`
Expected: PASS — 10 tests (2 config + 8 factory/hooks).

(Note: the spec target is 17 tests for the file. Steps 8 + 9 will add the remaining 7 H2/H5 tests.)

- [ ] **Step 4: Run full plugins-crate tests as regression gate**

Run: `rtk cargo nextest run -p agent-shim-plugins`
Expected: PASS — 54 (existing) + 3 (scratch from T4) + 10 (new pii_scrubber) = 67.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/plugins/src/builtin/mod.rs crates/plugins/src/builtin/pii_scrubber.rs
rtk git commit -m "feat(plugins): pii_scrubber config + factory + hook stubs (P06b1 T7)"
```

---

## Task 8: H2 — inbound scrubbing

**Files:**
- Modify: `crates/plugins/src/builtin/pii_scrubber.rs` (replace H2 stub with real impl; add `apply_rules` helper + 4 H2 tests)

- [ ] **Step 1: Write failing tests**

Open `crates/plugins/src/builtin/pii_scrubber.rs`. Inside the existing `mod tests` block, add the following tests **before** the closing `}` of the test mod:

```rust
    // ── H2 hook ────────────────────────────────────────────────────────

    use agent_shim_core::message::Message;
    use agent_shim_core::request::RequestMetadata;
    use agent_shim_core::{
        CanonicalRequest, ContentBlock, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel,
        GenerationOptions, MessageRole, ReasoningBlock, RequestId, ResolvedPolicy, TextBlock,
    };

    fn make_ctx() -> PluginContext {
        PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/test".to_string(),
        )
    }

    fn make_request_with_text(text: &str) -> CanonicalRequest {
        let block = ContentBlock::Text(TextBlock {
            text: text.to_string(),
            extensions: ExtensionMap::new(),
        });
        let msg = Message {
            role: MessageRole::User,
            content: vec![block],
            extensions: ExtensionMap::new(),
        };
        CanonicalRequest {
            request_id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                version: "v1".to_string(),
            },
            model: FrontendModel("test-model".to_string()),
            system: vec![],
            messages: vec![msg],
            tools: vec![],
            tool_choice: None,
            generation: GenerationOptions::default(),
            policy: ResolvedPolicy::default(),
            metadata: RequestMetadata::default(),
            extensions: ExtensionMap::new(),
        }
    }

    fn make_request_with_reasoning(text: &str) -> CanonicalRequest {
        let block = ContentBlock::Reasoning(ReasoningBlock {
            text: text.to_string(),
            extensions: ExtensionMap::new(),
        });
        let msg = Message {
            role: MessageRole::Assistant,
            content: vec![block],
            extensions: ExtensionMap::new(),
        };
        let mut req = make_request_with_text(""); // borrow scaffolding
        req.messages = vec![msg];
        req
    }

    #[tokio::test]
    async fn h2_scrubs_text_blocks() {
        let cfg = json!({
            "inbound": [
                { "name": "email", "pattern": r"\w+@\w+\.\w+", "replacement": "[EMAIL]" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).unwrap();
        let req = make_request_with_text("Contact me at alice@example.com please.");
        let out = plugin.on_decoded_request(&make_ctx(), req).await.unwrap();
        let text = match &out.messages[0].content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected Text block"),
        };
        assert_eq!(text, "Contact me at [EMAIL] please.");
    }

    #[tokio::test]
    async fn h2_scrubs_reasoning_blocks() {
        let cfg = json!({
            "inbound": [
                { "name": "ssn", "pattern": r"\b\d{3}-\d{2}-\d{4}\b", "replacement": "[SSN]" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).unwrap();
        let req = make_request_with_reasoning("User mentioned SSN 123-45-6789 earlier.");
        let out = plugin.on_decoded_request(&make_ctx(), req).await.unwrap();
        let text = match &out.messages[0].content[0] {
            ContentBlock::Reasoning(r) => r.text.clone(),
            _ => panic!("expected Reasoning block"),
        };
        assert_eq!(text, "User mentioned SSN [SSN] earlier.");
    }

    #[tokio::test]
    async fn h2_skips_other_blocks() {
        use agent_shim_core::{BinarySource, ImageBlock};

        let cfg = json!({
            "inbound": [
                { "name": "email", "pattern": r"\w+@\w+", "replacement": "[X]" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).unwrap();
        let image_block = ContentBlock::Image(ImageBlock {
            source: BinarySource::Url("https://example.com/cat@home.png".to_string()),
            extensions: ExtensionMap::new(),
        });
        let mut req = make_request_with_text("seed");
        req.messages[0].content = vec![image_block.clone()];

        let out = plugin.on_decoded_request(&make_ctx(), req).await.unwrap();
        match &out.messages[0].content[0] {
            ContentBlock::Image(_) => {} // unchanged
            other => panic!("expected unmodified Image, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn h2_counter_increments_per_match() {
        use metrics::Key;
        use metrics_util::debugging::{DebuggingRecorder, DebugValue, MetricKind};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let cfg = json!({
            "inbound": [
                { "name": "email", "pattern": r"\w+@\w+\.\w+", "replacement": "[E]" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).unwrap();
        let req = make_request_with_text("a@b.com c@d.com");
        plugin.on_decoded_request(&make_ctx(), req).await.unwrap();

        let snapshot = snapshotter.snapshot().into_vec();
        let matches: u64 = snapshot
            .iter()
            .filter_map(|(key, _, _, value)| {
                if key.key().name() == "agent_shim_plugin_pii_scrubber_matches_total" {
                    match value {
                        DebugValue::Counter(v) => Some(*v),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .sum();
        assert_eq!(matches, 2, "two emails should produce two counter increments");
    }
```

- [ ] **Step 2: Run tests to verify failure**

Run: `rtk cargo build -p agent-shim-plugins --tests`
Expected: FAIL — `apply_rules` doesn't exist (or H2 stub returns `req` unchanged).

- [ ] **Step 3: Implement H2 body + helper**

In `crates/plugins/src/builtin/pii_scrubber.rs`, replace the `on_decoded_request` stub with the real impl AND add the `apply_rules` helper. Find:

```rust
    async fn on_decoded_request(
        &self,
        _ctx: &PluginContext,
        req: CanonicalRequest,
    ) -> PluginResult<CanonicalRequest> {
        // Filled in by T8.
        Ok(req)
    }
```

Replace with:

```rust
    async fn on_decoded_request(
        &self,
        ctx: &PluginContext,
        mut req: CanonicalRequest,
    ) -> PluginResult<CanonicalRequest> {
        for msg in &mut req.messages {
            for block in &mut msg.content {
                match block {
                    ContentBlock::Text(t) => {
                        t.text = apply_rules(&self.inbound, &t.text, ctx, "inbound");
                    }
                    ContentBlock::Reasoning(r) => {
                        r.text = apply_rules(&self.inbound, &r.text, ctx, "inbound");
                    }
                    _ => {}
                }
            }
        }
        Ok(req)
    }
```

Then add the `apply_rules` free function above the `impl Plugin for PiiScrubber {` block (or at the bottom of the module before the `#[cfg(test)] mod tests`):

```rust
fn apply_rules(
    rules: &[CompiledRule],
    text: &str,
    _ctx: &PluginContext,
    direction: &'static str,
) -> String {
    let mut current = std::borrow::Cow::Borrowed(text);
    for rule in rules {
        let match_count = rule.regex.find_iter(&current).count() as u64;
        if match_count > 0 {
            metrics::counter!(
                agent_shim_observability::metrics::catalog::PluginPiiScrubberMatchesTotal::NAME,
                "rule" => rule.name.clone(),
                "direction" => direction,
            )
            .increment(match_count);
            let replaced = rule
                .regex
                .replace_all(&current, rule.replacement.as_str())
                .into_owned();
            current = std::borrow::Cow::Owned(replaced);
        }
    }
    current.into_owned()
}
```

You'll need to import the catalog name at the top of the file. Add this to the imports section (below the existing `use` statements):

```rust
// Catalog NAME constant; available regardless of whether the global
// recorder is the prometheus one (the `metrics` facade dispatches).
use agent_shim_observability::metrics::catalog::PluginPiiScrubberMatchesTotal as _;
```

(The `as _` import binds the trait/type just enough that `PluginPiiScrubberMatchesTotal::NAME` resolves.)

- [ ] **Step 4: Run tests**

Run: `rtk cargo nextest run -p agent-shim-plugins pii_scrubber`
Expected: PASS — 14 tests (10 from T7 + 4 new H2).

- [ ] **Step 5: Full regression**

Run: `rtk cargo nextest run -p agent-shim-plugins`
Expected: PASS — 67 + 4 = 71.

- [ ] **Step 6: Commit**

```bash
rtk git add crates/plugins/src/builtin/pii_scrubber.rs
rtk git commit -m "feat(plugins): pii_scrubber H2 inbound scrubbing + apply_rules helper (P06b1 T8)"
```

---

## Task 9: H5 — outbound buffered scrubbing

**Files:**
- Modify: `crates/plugins/src/builtin/pii_scrubber.rs` (replace H5 stub with buffered impl; add 5 H5 tests)

- [ ] **Step 1: Write failing tests**

In `crates/plugins/src/builtin/pii_scrubber.rs`, inside the test mod, append the following block of H5 tests:

```rust
    // ── H5 hook ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn h5_buffer_below_threshold_returns_empty() {
        let cfg = json!({
            "outbound": [
                { "name": "n", "pattern": "x", "replacement": "y" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).unwrap();
        let ctx = make_ctx();
        let event = StreamEvent::TextDelta { index: 0, text: "hello".to_string() };
        let out = plugin.on_stream_event(&ctx, event).await.unwrap();
        assert!(out.is_empty(), "below FLUSH_THRESHOLD must buffer (return empty vec)");
    }

    #[tokio::test]
    async fn h5_buffer_above_threshold_emits_scrubbed_prefix() {
        let cfg = json!({
            "outbound": [
                { "name": "n", "pattern": "secret", "replacement": "REDACTED" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).unwrap();
        let ctx = make_ctx();
        // First small delta: buffer.
        let _ = plugin.on_stream_event(&ctx, StreamEvent::TextDelta {
            index: 0,
            text: "small ".to_string(),
        }).await.unwrap();
        // Large delta pushes buffer over 256B.
        let big = "secret ".repeat(50); // 350 bytes
        let out = plugin.on_stream_event(&ctx, StreamEvent::TextDelta {
            index: 0,
            text: big,
        }).await.unwrap();
        assert_eq!(out.len(), 1);
        match &out[0] {
            StreamEvent::TextDelta { text, .. } => {
                assert!(text.contains("REDACTED"), "scrubbed prefix must replace 'secret'");
            }
            other => panic!("expected TextDelta, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn h5_pattern_spans_deltas_caught() {
        let cfg = json!({
            "outbound": [
                { "name": "cc", "pattern": r"\b\d{4}-\d{4}-\d{4}-\d{4}\b", "replacement": "[CC]" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).unwrap();
        let ctx = make_ctx();
        // Split a credit card across two deltas.
        let _ = plugin.on_stream_event(&ctx, StreamEvent::TextDelta {
            index: 0,
            text: "card 1234-5678-".to_string(),
        }).await.unwrap();
        let _ = plugin.on_stream_event(&ctx, StreamEvent::TextDelta {
            index: 0,
            text: "9012-3456 end".to_string(),
        }).await.unwrap();
        // Trigger flush via MessageStop.
        let out = plugin.on_stream_event(&ctx, StreamEvent::MessageStop {
            stop_reason: agent_shim_core::usage::StopReason::EndTurn,
            stop_sequence: None,
        }).await.unwrap();

        // out should be [TextDelta with scrubbed text, MessageStop].
        assert!(out.len() >= 2, "expected scrubbed-text + MessageStop");
        let combined: String = out.iter().filter_map(|e| match e {
            StreamEvent::TextDelta { text, .. } => Some(text.clone()),
            _ => None,
        }).collect();
        assert!(combined.contains("[CC]"), "spanning credit card must be redacted; got: {combined}");
        assert!(!combined.contains("1234-5678-9012-3456"), "raw card must not appear");
    }

    #[tokio::test]
    async fn h5_non_text_event_flushes_buffer() {
        let cfg = json!({
            "outbound": [
                { "name": "x", "pattern": "x", "replacement": "y" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).unwrap();
        let ctx = make_ctx();
        let _ = plugin.on_stream_event(&ctx, StreamEvent::TextDelta {
            index: 0,
            text: "buffered".to_string(),
        }).await.unwrap();
        let out = plugin.on_stream_event(&ctx, StreamEvent::MessageStop {
            stop_reason: agent_shim_core::usage::StopReason::EndTurn,
            stop_sequence: None,
        }).await.unwrap();
        // First emission is the flushed text, second is the MessageStop.
        assert!(out.len() >= 2);
        assert!(matches!(out[0], StreamEvent::TextDelta { .. }));
        assert!(matches!(out.last(), Some(StreamEvent::MessageStop { .. })));
    }

    #[tokio::test]
    async fn h5_utf8_boundary_safe() {
        let cfg = json!({
            "outbound": [
                { "name": "n", "pattern": "x", "replacement": "y" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).unwrap();
        let ctx = make_ctx();
        // Build a 300-byte buffer containing multi-byte UTF-8 chars near
        // the cut point. Chinese characters are 3 bytes each in UTF-8.
        let mut text = String::with_capacity(300);
        while text.len() < 300 {
            text.push('中'); // 3 bytes per push
        }
        let out = plugin.on_stream_event(&ctx, StreamEvent::TextDelta {
            index: 0,
            text,
        }).await.unwrap();
        // No panic = pass. Output must be valid UTF-8 (Rust String invariant ensures this).
        assert!(!out.is_empty(), "expected at least one emission");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `rtk cargo build -p agent-shim-plugins --tests`
Expected: PASS to compile (the existing stub returns `Ok(vec![event])` which is type-correct).

Run: `rtk cargo nextest run -p agent-shim-plugins h5_`
Expected: FAIL — tests expect buffered behaviour, current stub passes events through.

- [ ] **Step 3: Implement H5 body + helpers**

Replace the `on_stream_event` stub in the `impl Plugin for PiiScrubber` block:

```rust
    async fn on_stream_event(
        &self,
        ctx: &PluginContext,
        event: StreamEvent,
    ) -> PluginResult<Vec<StreamEvent>> {
        let mut buf = ctx.scratch_get_or_init::<PiiBuffer, _>("pii_scrubber", PiiBuffer::default);

        match event {
            StreamEvent::TextDelta { index, text } => {
                buf.text.push_str(&text);
                if buf.text.len() > FLUSH_THRESHOLD {
                    let emitted = flush_safe_prefix(&mut buf.text, &self.outbound, ctx);
                    Ok(if emitted.is_empty() {
                        vec![]
                    } else {
                        vec![StreamEvent::TextDelta { index, text: emitted }]
                    })
                } else {
                    Ok(vec![])
                }
            }
            StreamEvent::ReasoningDelta { index, text } => {
                buf.reasoning.push_str(&text);
                if buf.reasoning.len() > FLUSH_THRESHOLD {
                    let emitted = flush_safe_prefix(&mut buf.reasoning, &self.outbound, ctx);
                    Ok(if emitted.is_empty() {
                        vec![]
                    } else {
                        vec![StreamEvent::ReasoningDelta { index, text: emitted }]
                    })
                } else {
                    Ok(vec![])
                }
            }
            other => {
                let mut out = Vec::with_capacity(3);
                if !buf.text.is_empty() {
                    let scrubbed = scrub_full(&buf.text, &self.outbound, ctx);
                    buf.text.clear();
                    if !scrubbed.is_empty() {
                        out.push(StreamEvent::TextDelta {
                            index: index_from_event(&other).unwrap_or(0),
                            text: scrubbed,
                        });
                    }
                }
                if !buf.reasoning.is_empty() {
                    let scrubbed = scrub_full(&buf.reasoning, &self.outbound, ctx);
                    buf.reasoning.clear();
                    if !scrubbed.is_empty() {
                        out.push(StreamEvent::ReasoningDelta {
                            index: index_from_event(&other).unwrap_or(0),
                            text: scrubbed,
                        });
                    }
                }
                out.push(other);
                Ok(out)
            }
        }
    }
```

Add the `PiiBuffer` struct and helper functions to the module (place them above the `impl Plugin for PiiScrubber` block, or near `apply_rules` from T8):

```rust
#[derive(Default)]
struct PiiBuffer {
    text: String,
    reasoning: String,
}

fn flush_safe_prefix(buf: &mut String, rules: &[CompiledRule], ctx: &PluginContext) -> String {
    let split_at = floor_char_boundary(buf, buf.len().saturating_sub(MAX_PATTERN_TAIL));
    let prefix: String = buf.drain(..split_at).collect();
    apply_rules(rules, &prefix, ctx, "outbound")
}

fn scrub_full(buf: &str, rules: &[CompiledRule], ctx: &PluginContext) -> String {
    apply_rules(rules, buf, ctx, "outbound")
}

/// Find the largest UTF-8 char boundary ≤ `idx`. Std's
/// `floor_char_boundary` is unstable as of Rust 1.85; polyfill here.
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let bytes = s.as_bytes();
    let mut i = idx;
    while i > 0 && (bytes[i] & 0b1100_0000) == 0b1000_0000 {
        i -= 1;
    }
    i
}

fn index_from_event(e: &StreamEvent) -> Option<u32> {
    match e {
        StreamEvent::ContentBlockStart { index, .. }
        | StreamEvent::ContentBlockStop { index }
        | StreamEvent::ToolCallStart { index, .. }
        | StreamEvent::ToolCallArgumentsDelta { index, .. }
        | StreamEvent::ToolCallStop { index } => Some(*index),
        _ => None,
    }
}
```

- [ ] **Step 4: Run H5 tests**

Run: `rtk cargo nextest run -p agent-shim-plugins h5_`
Expected: PASS — 5 H5 tests.

- [ ] **Step 5: Full plugin-crate regression**

Run: `rtk cargo nextest run -p agent-shim-plugins`
Expected: PASS — 71 + 5 = 76.

- [ ] **Step 6: Commit**

```bash
rtk git add crates/plugins/src/builtin/pii_scrubber.rs
rtk git commit -m "feat(plugins): pii_scrubber H5 buffered outbound scrubbing (P06b1 T9)"
```

---

## Task 10: Gateway integration test

**Files:**
- Create: `crates/gateway/tests/pii_scrubber_integration.rs`

Validates the full production path: YAML → `AppState::new` → HTTP request → upstream sees scrubbed prompt; client sees scrubbed response; Prometheus counter incremented.

- [ ] **Step 1: Verify gateway dev-dependencies have what we need**

Read `crates/gateway/Cargo.toml` `[dev-dependencies]` to confirm `mockito`, `tower`, `axum`, `serde_yaml`, and `tracing-test` are present. (P06a T9 added `tracing-test` and `serde_yaml`; the rest came from earlier phases.)

If `mockito` is NOT present in dev-dependencies, add it:

```toml
mockito = "1"
```

- [ ] **Step 2: Create the integration test**

Create `crates/gateway/tests/pii_scrubber_integration.rs`:

```rust
//! Phase 7 P06b1: end-to-end integration test for pii_scrubber.
//!
//! Validates the full production path: YAML → AppState::new →
//! builtin_plugins() → PluginRegistry::build → axum router → HTTP
//! request with PII in prompt → upstream sees scrubbed prompt → Prometheus
//! counter `agent_shim_plugin_pii_scrubber_matches_total` increments.
//!
//! Uses a mockito upstream that captures the body so we can assert the
//! prompt was scrubbed before reaching the OpenAI-compatible provider.

use agent_shim_config::GatewayConfig;
use agent_shim_gateway::{server::build_router, state::AppState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::util::ServiceExt;

const REQUEST_BODY: &str = r#"{
    "model": "test-model",
    "max_tokens": 16,
    "messages": [{
        "role": "user",
        "content": "Contact me at alice@example.com about my SSN 123-45-6789."
    }]
}"#;

#[tokio::test]
async fn pii_scrubber_end_to_end_scrubs_prompt() -> anyhow::Result<()> {
    // Start a mockito server that echoes back a simple OpenAI-compatible
    // streaming response.
    let mut server = mockito::Server::new_async().await;
    let upstream_body_capture = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let capture_clone = upstream_body_capture.clone();

    let _mock = server
        .mock("POST", "/chat/completions")
        .match_body(mockito::Matcher::Any)
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(concat!(
            "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        ))
        .with_body_from_request(move |req| {
            // Capture request body for later assertion.
            if let Some(body) = req.body() {
                let s = String::from_utf8_lossy(body).to_string();
                *capture_clone.lock().unwrap() = s;
            }
            Default::default()
        })
        .create_async()
        .await;

    let yaml = format!(r#"
server:
  bind: 127.0.0.1
  port: 0
  keepalive_secs: 15
upstreams:
  echo:
    type: openai
    base_url: "{}"
    api_key: "test-key"
routes:
  - frontend: anthropic_messages
    model: test-model
    upstream: echo
    backend_model: test-model
    plugins:
      on_decoded_request:
        - pii_scrub
plugins:
  pii_scrub:
    type: pii_scrubber
    config:
      inbound:
        - name: email
          pattern: "[\\w.]+@[\\w.]+\\.[a-z]{{2,}}"
          replacement: "[REDACTED-EMAIL]"
        - name: ssn
          pattern: "\\b\\d{{3}}-\\d{{2}}-\\d{{4}}\\b"
          replacement: "[REDACTED-SSN]"
"#, server.url());

    let cfg: GatewayConfig = serde_yaml::from_str(&yaml)?;
    let (state, _reload_rx) = AppState::new(cfg).await?;
    let app = build_router(state);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(REQUEST_BODY))?;

    let response = app.oneshot(request).await?;
    assert_eq!(response.status(), StatusCode::OK, "request must succeed");

    // Drain the response body to ensure the H2 hook ran.
    let _body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;

    // Verify the upstream received SCRUBBED content.
    let captured = upstream_body_capture.lock().unwrap().clone();
    assert!(
        captured.contains("[REDACTED-EMAIL]"),
        "upstream must receive scrubbed email; body was: {captured}"
    );
    assert!(
        captured.contains("[REDACTED-SSN]"),
        "upstream must receive scrubbed SSN; body was: {captured}"
    );
    assert!(
        !captured.contains("alice@example.com"),
        "upstream must NOT see raw email"
    );
    assert!(
        !captured.contains("123-45-6789"),
        "upstream must NOT see raw SSN"
    );

    Ok(())
}
```

- [ ] **Step 3: Run the integration test**

Run: `rtk cargo nextest run --test pii_scrubber_integration`
Expected: PASS — 1 test.

If the test fails because of mockito API mismatch (e.g. `with_body_from_request` syntax may differ across mockito versions), adapt: the simplest fallback is to use mockito's request matchers and assert via the matcher itself rather than capturing the body. The critical assertion is "upstream did NOT see alice@example.com or 123-45-6789".

- [ ] **Step 4: Full workspace regression**

Run: `rtk cargo nextest run --workspace`
Expected: PASS — total test count ~876.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/gateway/Cargo.toml crates/gateway/tests/pii_scrubber_integration.rs
rtk git commit -m "test(gateway): pii_scrubber end-to-end integration via mockito (P06b1 T10)"
```

---

## Task 11: Clippy + fmt + frozen-core verification

**Files:** No new code; verification only.

- [ ] **Step 1: Run clippy on workspace**

Run: `rtk cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

If lint warnings fire on the new code, fix them in the appropriate task's file. Common candidates:
- Unused imports → remove
- `dead_code` on internal types → add `#[allow(dead_code)]` only if usage is genuinely deferred (e.g. a constant only consulted by tests).
- `clippy::needless_collect` on the find_iter().count() call → ignore if performance is not measurably affected, OR convert to a manual loop.

- [ ] **Step 2: Run rustfmt**

Run: `rtk cargo fmt --all -- --check`
Expected: PASS (no diff).

If fmt would change files, run `rtk cargo fmt --all` and re-commit those files under THIS task.

- [ ] **Step 3: Verify frozen-core invariant**

Run: `rtk git diff master..HEAD -- crates/core/`
Expected: EMPTY output.

If non-empty, the change leaked into core — back out the offending edit. P06b1 must touch only plugins, observability, gateway, workspace Cargo.toml, and docs.

- [ ] **Step 4: Final test count check**

Run: `rtk cargo nextest run --workspace`
Expected: PASS — exact test count ~876.

- [ ] **Step 5: Verify feature-off build**

Run: `rtk cargo build --workspace --no-default-features --features agent-shim-plugins/usage_recorder`
Expected: PASS — pii_scrubber feature off; regex not in the binary; workspace still builds.

(Note: cargo's feature flag for a workspace member requires the `<member>/<feature>` syntax; adjust the command if the workspace Cargo.toml needs a different invocation.)

- [ ] **Step 6: Commit (no-op if everything is clean)**

Only commit if any fmt or clippy fixes landed:

```bash
rtk git add -A
rtk git commit -m "chore: clippy + fmt cleanup after P06b1 (P06b1 T11)"
```

If no fixes were needed, skip the commit.

---

## Acceptance gates

After all 11 tasks land:

1. `rtk cargo nextest run --workspace` — test count ~876 (+/- a few). Zero failures.
2. `rtk cargo clippy --workspace --all-targets -- -D warnings` — clean.
3. `rtk cargo fmt --all -- --check` — clean.
4. `rtk git diff master..HEAD -- crates/core/` — empty (frozen-core preserved).
5. Manual smoke: `agent-shim serve --config <yaml with pii_scrubber plugin>` boots; one request with PII in the prompt routes through, upstream sees scrubbed text, `/metrics` shows non-zero `agent_shim_plugin_pii_scrubber_matches_total{rule, direction}`.
6. Spec acceptance criteria 1-15 (see `docs/superpowers/specs/2026-05-22-phase-7-p06b1-pii-scrubber-design.md` §13).

## Notes for the implementer

- **Task order matters.** T1 lands the workspace dep first; T2 wires the plugin crate's feature flag; T3 changes the error type (without which T7's factory error reporting wouldn't compile); T4 adds PluginContext scratch; T5 ripples PluginContext::new through ~19 callsites; T6 declares the metric; T7-T9 build pii_scrubber incrementally; T10 verifies end-to-end; T11 cleans up.
- **mockito API.** mockito's `with_body_from_request` closure syntax has shifted between major versions; the integration test sketch is one viable shape but adapt to whatever mockito version the workspace pins. The hard invariant is: upstream MUST receive scrubbed text. If `mockito::Matcher::Regex` or similar is easier, use it.
- **Counter label cardinality.** The `rule` label is operator-controlled (validated to `^[A-Za-z0-9_-]{1,64}$`). Operators with many rules will see proportional cardinality — document this in §13.13 if it becomes a concern; for v1 it's the right trade-off.
- **`regex::Regex::find_iter().count()`** is an O(n) pre-pass before `replace_all`. For very long text bodies this doubles the regex work. Acceptable for P06b1 given typical chat prompts are <10KB. Optimisation (replace-then-count) is deferred.
- **`scratch_get_or_init`** holds a write lock for the duration of the returned `ScratchGuard`. H5 hooks are serialised per request by GuardedH5Stream, so contention within a request is impossible. Across requests each PluginContext has its own scratch — no contention either. Drop the guard before any await point if you ever need to release the lock (the current impl doesn't, because all work between guard creation and drop is synchronous).
- **Feature-gated module path.** `#![cfg(feature = "pii_scrubber")]` at the top of `pii_scrubber.rs` ensures the file is excluded entirely when the feature is off. The `builtin/mod.rs` `#[cfg(feature = "pii_scrubber")] pub mod pii_scrubber;` line gates module declaration. Together they yield clean feature-off builds.
- **If `cargo nextest run` deadlocks during the integration test**, it's most likely the mockito server hanging on body capture. Try shorter timeouts or replace `with_body_from_request` with `Matcher::Any` and use `server.received_requests()` (mockito 1.x adds this) to fetch the captured body after the test.
