# Phase 7 P05 — Plugin Observability + H7 JoinSet Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire plugin invocations into AgentShim's logging/metrics/tracing layers (spec §7) and add a `PluginSupervisor` that lets gateway shutdown gracefully flush H7 (`on_response_complete`) tasks with bounded deadline.

**Architecture:** All instrumentation lives in the `invoke()` template and `wrap_stream()` in the plugins crate. Per-call `plugin.invoke` span for H2/H3/H7; per-stream aggregated `plugin.stream` span for H5. New `PluginSupervisor` owns the JoinSet for H7 tasks; gateway `run_core` flushes it between axum drain and otel shutdown.

**Tech Stack:** Rust, `tracing` 0.1, `metrics` 0.23, `metrics-util` 0.17 (test only), `tokio::task::JoinSet`, `std::sync::Mutex`, `Arc<AtomicU64>`.

**Source spec:** `docs/superpowers/specs/2026-05-15-phase-7-p05-observability-design.md`

---

## Pre-flight

Baseline: **789 tests passing** at master tip (`b5479bc` after P05 spec patched). After P05: expect ~811 tests (789 + ~22 new).

Frozen-core invariant: `crates/core/` MUST be untouched. Acceptance check at end: `git diff master..HEAD -- crates/core/` empty.

---

## Task 1: Plugins crate dependencies

**Files:**
- Modify: `crates/plugins/Cargo.toml`

This task only edits Cargo.toml. No tests; verified by next task's compilation.

- [ ] **Step 1: Add `metrics` direct dep + `agent-shim-observability` dep + `metrics-util` dev-dep**

Edit `crates/plugins/Cargo.toml`:

```toml
[package]
name = "agent-shim-plugins"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[lib]
name = "agent_shim_plugins"
path = "src/lib.rs"

[dependencies]
agent-shim-core = { path = "../core" }
agent-shim-observability = { path = "../observability" }
agent-shim-tokens = { path = "../tokens" }
async-trait.workspace = true
futures.workspace = true
metrics.workspace = true
parking_lot.workspace = true
serde = { workspace = true, features = ["derive"] }
serde_json.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "sync", "time"] }
tracing.workspace = true

[dev-dependencies]
metrics-util = "0.17"
pretty_assertions.workspace = true
serde_yaml = "0.9"
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "sync", "time", "test-util"] }
```

- [ ] **Step 2: Verify workspace builds**

Run: `cargo build -p agent-shim-plugins`
Expected: PASS (no behavioral change yet, just dep wiring)

- [ ] **Step 3: Commit**

```bash
git add crates/plugins/Cargo.toml Cargo.lock
git commit -m "build(plugins): add metrics + observability + metrics-util deps (P05 T1)"
```

---

## Task 2: Observability — three plugin metric descriptors + names + helpers + default buckets

**Files:**
- Modify: `crates/observability/src/metrics/catalog.rs`
- Modify: `crates/observability/src/metrics/names.rs`
- Modify: `crates/observability/src/metrics/recorders.rs`
- Modify: `crates/observability/src/metrics/mod.rs`

- [ ] **Step 1: Add three `#[derive(Metric)]` structs to catalog.rs**

In `crates/observability/src/metrics/catalog.rs`, after `ConfigReloadsTotal` (around line 201), insert:

```rust
// --- Plugin observability (Phase 7 P05) ---

#[derive(Metric)]
#[metric(
    name = "agent_shim_plugin_invocations_total",
    kind = "counter",
    help = "Plugin hook invocations by kind, name, hook, outcome (Plan 07 P05)"
)]
pub struct PluginInvocationsTotal;

#[derive(Metric)]
#[metric(
    name = "agent_shim_plugin_duration_seconds",
    kind = "histogram",
    help = "Plugin hook duration by kind, name, hook (Plan 07 P05)"
)]
pub struct PluginDurationSeconds;

#[derive(Metric)]
#[metric(
    name = "agent_shim_plugin_h7_dropped_at_shutdown_total",
    kind = "counter",
    help = "H7 plugin tasks dropped at shutdown by plugin_name (Plan 07 P05)"
)]
pub struct PluginH7DroppedTotal;
```

- [ ] **Step 2: Add `pub const` re-exports to names.rs**

In `crates/observability/src/metrics/names.rs`, after `COST_FILTERED_TOTAL` (around line 40), insert:

```rust
// --- Plugin observability (Phase 7 P05) ---
pub const PLUGIN_INVOCATIONS_TOTAL: &str = PluginInvocationsTotal::NAME;
pub const PLUGIN_DURATION_SECONDS: &str = PluginDurationSeconds::NAME;
pub const PLUGIN_H7_DROPPED_TOTAL: &str = PluginH7DroppedTotal::NAME;
```

- [ ] **Step 3: Add typed helpers to recorders.rs**

At end of `crates/observability/src/metrics/recorders.rs`, append:

```rust
/// Record one plugin hook invocation. Counter increments by 1; histogram
/// records `duration_secs`. §7.4 of the plugin design spec.
///
/// `plugin_kind` and `hook` are `&'static str` (zero-alloc). `plugin_name`
/// is `&str`, allocated via `.to_string()` per label per call (consistent
/// with `record_request` / `record_retry_attempt` pattern). For the H5
/// hook this is on the hot path (~2000 alloc / streaming response in the
/// worst case), but small-string allocator overhead is < 10 μs total on
/// a multi-100ms LLM stream — acceptable. Profile and refactor to
/// `Cow<'a, str>` if benchmarks ever justify.
pub fn record_plugin_invocation(
    plugin_kind: &'static str,
    plugin_name: &str,
    hook: &'static str,
    outcome: &'static str,
    duration_secs: f64,
) {
    metrics::counter!(
        names::PLUGIN_INVOCATIONS_TOTAL,
        "plugin_kind" => plugin_kind,
        "plugin_name" => plugin_name.to_string(),
        "hook" => hook,
        "outcome" => outcome,
    )
    .increment(1);
    metrics::histogram!(
        names::PLUGIN_DURATION_SECONDS,
        "plugin_kind" => plugin_kind,
        "plugin_name" => plugin_name.to_string(),
        "hook" => hook,
    )
    .record(duration_secs);
}

/// Record N H7 tasks dropped at shutdown for a given plugin. Called once
/// per plugin_name by the gateway shutdown hook with the aggregated count
/// from `PluginSupervisor::flush_pending_h7`.
pub fn record_h7_dropped(plugin_name: &str, count: u64) {
    metrics::counter!(
        names::PLUGIN_H7_DROPPED_TOTAL,
        "plugin_name" => plugin_name.to_string(),
    )
    .increment(count);
}
```

- [ ] **Step 4: Add hardcoded default histogram buckets in `install()`**

In `crates/observability/src/metrics/mod.rs`, inside the `INSTALLED.get_or_init` closure, BEFORE the existing `for (name, buckets) in &cfg.histogram_buckets` loop, insert:

```rust
        // Plan 07 P05 (Q10/B+): plugin H5 hook fires per SSE event at
        // sub-millisecond latency. The default Prometheus exporter buckets
        // (5ms-10s) put 95% of H5 samples into the first bucket, losing
        // resolution. Apply a sub-ms-to-5s default for plugin_duration_seconds
        // BEFORE the operator override loop so explicit YAML overrides still
        // win. Other metrics keep the exporter default.
        const PLUGIN_DURATION_DEFAULT_BUCKETS: &[f64] = &[
            0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.5, 1.0, 5.0,
        ];
        builder = builder
            .set_buckets_for_metric(
                Matcher::Full(names::PLUGIN_DURATION_SECONDS.to_string()),
                PLUGIN_DURATION_DEFAULT_BUCKETS,
            )
            .expect("plugin duration default buckets must be valid");
```

- [ ] **Step 5: Run observability crate tests**

Run: `cargo nextest run -p agent-shim-observability`
Expected: PASS (existing `all_unique` + `all_prefixed` catalog tests automatically cover the 3 new metrics; nothing new to write here)

- [ ] **Step 6: Commit**

```bash
git add crates/observability/src/metrics/
git commit -m "feat(observability): plugin metrics + helpers + default H5-friendly buckets (P05 T2)"
```

---

## Task 3: `PluginLogFields` + `PluginOutcome` + emit macro

**Files:**
- Create: `crates/plugins/src/log_fields.rs`
- Modify: `crates/plugins/src/lib.rs`

- [ ] **Step 1: Add module declaration to lib.rs**

Edit `crates/plugins/src/lib.rs` — in the `mod` declarations block (around line 16-19), add:

```rust
mod log_fields;
```

- [ ] **Step 2: Write the failing test file first (TDD red)**

Create `crates/plugins/src/log_fields.rs` with just the test module to verify it compiles and tests fail:

```rust
//! Whitelist of fields emitted by plugin observability. §7.1 of the
//! plugin design spec. The whitelist enforces §7.6 PII red lines —
//! adding a field MUST update §7.1 and the PII red-line doc.

use agent_shim_core::RequestId;

use crate::error::OnError;

/// Plugin hook outcome. Mapped to `outcome` metric label and
/// determines the tracing level per §7.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginOutcome {
    Success,
    Skipped,
    Failed,
    TimedOut,
    Aborted,
    ProtectedFieldMutated,
}

impl PluginOutcome {
    /// Stable label string for metric + log emission. NEVER rename — it
    /// is the public Prometheus label value.
    pub(crate) fn as_label(self) -> &'static str {
        match self {
            PluginOutcome::Success => "success",
            PluginOutcome::Skipped => "skipped",
            PluginOutcome::Failed => "failed",
            PluginOutcome::TimedOut => "timed_out",
            PluginOutcome::Aborted => "aborted",
            PluginOutcome::ProtectedFieldMutated => "protected_field_mutated",
        }
    }

    /// Tracing level per §7.2. The on_error policy upgrades TimedOut
    /// from WARN to ERROR when the timeout will abort the request.
    pub(crate) fn level(self, on_error: OnError) -> tracing::Level {
        match (self, on_error) {
            (PluginOutcome::Success, _) => tracing::Level::DEBUG,
            (PluginOutcome::Skipped, _) => tracing::Level::WARN,
            (PluginOutcome::Failed, _) => tracing::Level::ERROR,
            (PluginOutcome::TimedOut, OnError::Skip) => tracing::Level::WARN,
            (PluginOutcome::TimedOut, OnError::Fail) => tracing::Level::ERROR,
            (PluginOutcome::Aborted, _) => tracing::Level::INFO,
            (PluginOutcome::ProtectedFieldMutated, _) => tracing::Level::ERROR,
        }
    }
}

/// Whitelist of fields safe to emit to logs/metrics. Matches spec §7.1
/// verbatim. Adding a field MUST update §7.1 + the PII red-line doc.
///
/// NEVER add request content, user input, plugin output, or any
/// derived-from-user-content fields here. §7.6 PII red lines.
pub(crate) struct PluginLogFields<'a> {
    pub plugin_name: &'a str,
    pub plugin_kind: &'static str,
    pub plugin_hook: &'static str,
    pub request_id: &'a RequestId,
    pub route: &'a str,
    pub outcome: PluginOutcome,
    pub elapsed_ms: u64,
    pub on_error_policy: OnError,
    pub error: Option<&'a str>,
}

/// File-local: the 4-level dispatch macro. `tracing::event!`'s Level arg
/// must be a compile-time path, so the match is unavoidable. The macro
/// keeps the field list defined exactly once.
macro_rules! emit_at_level {
    ($level:expr, $f:expr) => {
        match $level {
            tracing::Level::ERROR => tracing::error!(
                "plugin.name" = $f.plugin_name,
                "plugin.kind" = $f.plugin_kind,
                "plugin.hook" = $f.plugin_hook,
                "agent_shim.request_id" = %$f.request_id,
                "agent_shim.route" = $f.route,
                "plugin.outcome" = $f.outcome.as_label(),
                "plugin.elapsed_ms" = $f.elapsed_ms,
                "plugin.on_error_policy" = ?$f.on_error_policy,
                "plugin.error" = $f.error,
            ),
            tracing::Level::WARN => tracing::warn!(
                "plugin.name" = $f.plugin_name,
                "plugin.kind" = $f.plugin_kind,
                "plugin.hook" = $f.plugin_hook,
                "agent_shim.request_id" = %$f.request_id,
                "agent_shim.route" = $f.route,
                "plugin.outcome" = $f.outcome.as_label(),
                "plugin.elapsed_ms" = $f.elapsed_ms,
                "plugin.on_error_policy" = ?$f.on_error_policy,
                "plugin.error" = $f.error,
            ),
            tracing::Level::INFO => tracing::info!(
                "plugin.name" = $f.plugin_name,
                "plugin.kind" = $f.plugin_kind,
                "plugin.hook" = $f.plugin_hook,
                "agent_shim.request_id" = %$f.request_id,
                "agent_shim.route" = $f.route,
                "plugin.outcome" = $f.outcome.as_label(),
                "plugin.elapsed_ms" = $f.elapsed_ms,
                "plugin.on_error_policy" = ?$f.on_error_policy,
                "plugin.error" = $f.error,
            ),
            tracing::Level::DEBUG => tracing::debug!(
                "plugin.name" = $f.plugin_name,
                "plugin.kind" = $f.plugin_kind,
                "plugin.hook" = $f.plugin_hook,
                "agent_shim.request_id" = %$f.request_id,
                "agent_shim.route" = $f.route,
                "plugin.outcome" = $f.outcome.as_label(),
                "plugin.elapsed_ms" = $f.elapsed_ms,
                "plugin.on_error_policy" = ?$f.on_error_policy,
                "plugin.error" = $f.error,
            ),
            _ => {}
        }
    };
}

impl PluginLogFields<'_> {
    /// Single emission point from `invoke()`. §7.5 noise control: H5
    /// success skips log emission (metric still updates upstream).
    pub(crate) fn emit(&self) {
        if self.outcome == PluginOutcome::Success && self.plugin_hook == "on_stream_event" {
            return;
        }
        emit_at_level!(self.outcome.level(self.on_error_policy), self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_label_strings_are_stable() {
        assert_eq!(PluginOutcome::Success.as_label(), "success");
        assert_eq!(PluginOutcome::Skipped.as_label(), "skipped");
        assert_eq!(PluginOutcome::Failed.as_label(), "failed");
        assert_eq!(PluginOutcome::TimedOut.as_label(), "timed_out");
        assert_eq!(PluginOutcome::Aborted.as_label(), "aborted");
        assert_eq!(
            PluginOutcome::ProtectedFieldMutated.as_label(),
            "protected_field_mutated"
        );
    }

    #[test]
    fn level_success_is_debug_regardless_of_on_error() {
        assert_eq!(
            PluginOutcome::Success.level(OnError::Skip),
            tracing::Level::DEBUG
        );
        assert_eq!(
            PluginOutcome::Success.level(OnError::Fail),
            tracing::Level::DEBUG
        );
    }

    #[test]
    fn level_skipped_is_warn() {
        assert_eq!(
            PluginOutcome::Skipped.level(OnError::Skip),
            tracing::Level::WARN
        );
        // OnError::Fail with Skipped is structurally unreachable in
        // invoke() (Fail policy propagates as Failed instead), but the
        // level mapping is defined for safety.
        assert_eq!(
            PluginOutcome::Skipped.level(OnError::Fail),
            tracing::Level::WARN
        );
    }

    #[test]
    fn level_failed_is_error() {
        assert_eq!(
            PluginOutcome::Failed.level(OnError::Skip),
            tracing::Level::ERROR
        );
        assert_eq!(
            PluginOutcome::Failed.level(OnError::Fail),
            tracing::Level::ERROR
        );
    }

    #[test]
    fn level_timeout_is_warn_on_skip_error_on_fail() {
        assert_eq!(
            PluginOutcome::TimedOut.level(OnError::Skip),
            tracing::Level::WARN
        );
        assert_eq!(
            PluginOutcome::TimedOut.level(OnError::Fail),
            tracing::Level::ERROR
        );
    }

    #[test]
    fn level_aborted_is_info() {
        assert_eq!(
            PluginOutcome::Aborted.level(OnError::Skip),
            tracing::Level::INFO
        );
        assert_eq!(
            PluginOutcome::Aborted.level(OnError::Fail),
            tracing::Level::INFO
        );
    }

    #[test]
    fn level_protected_field_mutated_is_error() {
        assert_eq!(
            PluginOutcome::ProtectedFieldMutated.level(OnError::Skip),
            tracing::Level::ERROR
        );
        assert_eq!(
            PluginOutcome::ProtectedFieldMutated.level(OnError::Fail),
            tracing::Level::ERROR
        );
    }

    #[test]
    fn h5_success_skips_emit_no_panic() {
        // We can't easily assert "didn't emit" without a tracing subscriber
        // capture, but we can verify the noise-control path is reachable.
        let fields = PluginLogFields {
            plugin_name: "n",
            plugin_kind: "k",
            plugin_hook: "on_stream_event",
            request_id: &RequestId::new(),
            route: "r",
            outcome: PluginOutcome::Success,
            elapsed_ms: 0,
            on_error_policy: OnError::Skip,
            error: None,
        };
        fields.emit(); // must return without panic
    }

    #[test]
    fn h2_success_emit_no_panic() {
        let fields = PluginLogFields {
            plugin_name: "n",
            plugin_kind: "k",
            plugin_hook: "on_decoded_request",
            request_id: &RequestId::new(),
            route: "r",
            outcome: PluginOutcome::Success,
            elapsed_ms: 1,
            on_error_policy: OnError::Skip,
            error: None,
        };
        fields.emit();
    }

    #[test]
    fn failed_emit_with_error_message_no_panic() {
        let fields = PluginLogFields {
            plugin_name: "n",
            plugin_kind: "k",
            plugin_hook: "on_decoded_request",
            request_id: &RequestId::new(),
            route: "r",
            outcome: PluginOutcome::Failed,
            elapsed_ms: 1,
            on_error_policy: OnError::Fail,
            error: Some("boom"),
        };
        fields.emit();
    }
}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo nextest run -p agent-shim-plugins log_fields::`
Expected: PASS — 10 tests (7 level mapping + 3 emit smoke). All pass on first run because the implementation is included in the file.

- [ ] **Step 4: Commit**

```bash
git add crates/plugins/src/log_fields.rs crates/plugins/src/lib.rs
git commit -m "feat(plugins): PluginLogFields + PluginOutcome + emit macro (P05 T3)"
```

---

## Task 4: `SpanMode` enum + `InvokeArgs` struct + invoke() refactor

**Files:**
- Modify: `crates/plugins/src/invoke.rs`

This task refactors the existing P04 `invoke()` from 6 positional args to `(InvokeArgs, &ctx, work)` while adding span instrumentation, LogFields emission, and metric recording.

- [ ] **Step 1: Write the failing tests for new shape**

Open `crates/plugins/src/invoke.rs`. Replace the existing test module's `invoke_*` tests (`invoke_success_returns_value`, `invoke_failed_with_skip_returns_skipped`, etc.) with tests using the new shape. The existing six tests are at the end of the file:

Add to the test module (or replace the existing four invoke tests):

```rust
    use agent_shim_core::RequestId;
    use crate::trait_def::Hook;

    fn ctx() -> PluginContext {
        PluginContext {
            request_id: RequestId::new(),
            frontend: agent_shim_core::FrontendKind::AnthropicMessages,
            route_label: "anthropic_messages/m".to_string(),
        }
    }

    fn args_for(name: &str, hook: &'static str) -> InvokeArgs<'_> {
        InvokeArgs {
            plugin_name: name,
            plugin_kind: "test_kind",
            hook,
            timeout_ms: 50,
            on_error: OnError::Skip,
            span_mode: SpanMode::PerInvocation,
        }
    }

    #[tokio::test]
    async fn invoke_with_args_success_returns_value() {
        let c = ctx();
        let a = args_for("p", "on_decoded_request");
        let out: InvokeOutcome<i32> = invoke(a, &c, async { Ok(42) }).await;
        match out {
            InvokeOutcome::Success(v) => assert_eq!(v, 42),
            _ => panic!("expected Success"),
        }
    }

    #[tokio::test]
    async fn invoke_with_args_failed_skip_returns_skipped() {
        let c = ctx();
        let a = args_for("p", "on_decoded_request");
        let out: InvokeOutcome<i32> = invoke(
            a,
            &c,
            async {
                Err(PluginError::Failed {
                    plugin: "p".to_string(),
                    hook: "on_decoded_request",
                    message: "boom".to_string(),
                })
            },
        )
        .await;
        assert!(matches!(out, InvokeOutcome::Skipped));
    }

    #[tokio::test]
    async fn invoke_with_args_failed_fail_propagates() {
        let c = ctx();
        let mut a = args_for("p", "on_decoded_request");
        a.on_error = OnError::Fail;
        let out: InvokeOutcome<i32> = invoke(
            a,
            &c,
            async {
                Err(PluginError::Failed {
                    plugin: "p".to_string(),
                    hook: "on_decoded_request",
                    message: "boom".to_string(),
                })
            },
        )
        .await;
        assert!(matches!(out, InvokeOutcome::Propagate(_)));
    }

    #[tokio::test]
    async fn invoke_with_args_aborted_always_propagates() {
        let c = ctx();
        let a = args_for("p", "on_decoded_request"); // Skip
        let out: InvokeOutcome<i32> = invoke(
            a,
            &c,
            async {
                Err(PluginError::Aborted {
                    plugin: "p".to_string(),
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
    async fn invoke_with_args_timeout_skip_returns_skipped() {
        let c = ctx();
        let mut a = args_for("p", "on_decoded_request");
        a.timeout_ms = 10;
        let out: InvokeOutcome<i32> = invoke(
            a,
            &c,
            async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(42)
            },
        )
        .await;
        assert!(matches!(out, InvokeOutcome::Skipped));
    }

    #[tokio::test]
    async fn invoke_aggregated_span_mode_does_not_open_new_span() {
        // Smoke test: Aggregated mode runs without panic and returns
        // the work's result. Span correctness is covered indirectly by
        // the H5 integration test in T12.
        let c = ctx();
        let mut a = args_for("p", "on_stream_event");
        a.span_mode = SpanMode::Aggregated;
        let out: InvokeOutcome<i32> = invoke(a, &c, async { Ok(99) }).await;
        assert!(matches!(out, InvokeOutcome::Success(99)));
    }
```

- [ ] **Step 2: Replace `invoke.rs` body with new shape**

Overwrite `crates/plugins/src/invoke.rs` body (keeping the test module from Step 1):

```rust
//! The `invoke()` template runs around every plugin call. It owns the
//! timeout, the on_error policy, the protected-field diff, the
//! per-call OTel span, structured logging, and metric recording.
//! Plugin authors never see this code.
//!
//! Spec §6.3 / §6.4 / §4.5 + §7 (P05).

use std::future::Future;
use std::time::{Duration, Instant};

use agent_shim_core::CanonicalRequest;
use tracing::Instrument;

use crate::context::PluginContext;
use crate::error::{OnError, PluginError, PluginResult};
use crate::log_fields::{PluginLogFields, PluginOutcome};
use crate::registry::PluginEntry;
use crate::trait_def::Hook;

/// Outcome of a single invocation. Distinct from `PluginResult` because
/// the registry needs to know "swallowed by on_error: skip" (Skipped)
/// vs "success" (Success(v)) vs "propagate" (Propagate(err)).
#[allow(dead_code)] // used by P04 run_* methods
pub(crate) enum InvokeOutcome<T> {
    Success(T),
    Skipped,
    Propagate(PluginError),
}

/// Span instrumentation mode for a single invoke call. §7.3 + §7.5.
///
/// - `PerInvocation`: open a fresh `plugin.invoke` span. Used by H2/H3/H7.
/// - `Aggregated`: do NOT open a new span; events attach to whatever
///   span is current (typically `plugin.stream` from `wrap_stream`).
///   Used by H5 to avoid 500-event-per-request span explosion.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // used by P04 run_* methods after T6
pub(crate) enum SpanMode {
    PerInvocation,
    Aggregated,
}

/// Bundle of static/per-entry args for a single invoke. Replaces the
/// 6-positional-args form to keep call sites readable (P05 Q7).
#[allow(dead_code)] // used by P04 run_* methods after T6
pub(crate) struct InvokeArgs<'a> {
    pub plugin_name: &'a str,
    pub plugin_kind: &'static str,
    pub hook: &'static str,
    pub timeout_ms: u64,
    pub on_error: OnError,
    pub span_mode: SpanMode,
}

impl InvokeArgs<'_> {
    /// Build args from a `PluginEntry` + the hook being invoked + the
    /// span mode for this hook class. The cached `entry.kind`
    /// `&'static str` flows through; timeouts pick the per-hook value.
    #[allow(dead_code)] // used by P04 run_* methods after T6
    pub(crate) fn from_entry<'a>(
        entry: &'a PluginEntry,
        hook: Hook,
        span_mode: SpanMode,
    ) -> InvokeArgs<'a> {
        InvokeArgs {
            plugin_name: &entry.name,
            plugin_kind: entry.kind,
            hook: hook.as_str(),
            timeout_ms: entry.timeouts.for_hook(hook),
            on_error: entry.on_error,
            span_mode,
        }
    }
}

/// Run a single plugin hook with the standard policy envelope.
#[allow(dead_code)] // used by P04 run_* methods after T6
pub(crate) async fn invoke<T, Fut>(
    args: InvokeArgs<'_>,
    ctx: &PluginContext,
    work: Fut,
) -> InvokeOutcome<T>
where
    Fut: Future<Output = PluginResult<T>>,
{
    // §7.3 + Q16: PerInvocation opens a fresh plugin.invoke span;
    // Aggregated uses Span::none() (zero-overhead disabled span). The
    // work future is always `.instrument(span.clone())`, which means a
    // single timeout code path regardless of mode.
    let span = match args.span_mode {
        SpanMode::PerInvocation => tracing::info_span!(
            "plugin.invoke",
            "plugin.name" = args.plugin_name,
            "plugin.kind" = args.plugin_kind,
            "plugin.hook" = args.hook,
            "plugin.outcome" = tracing::field::Empty,
            "plugin.elapsed_ms" = tracing::field::Empty,
        ),
        SpanMode::Aggregated => tracing::Span::none(),
    };

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_millis(args.timeout_ms),
        work.instrument(span.clone()),
    )
    .await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let (outcome, err_for_log, returned): (PluginOutcome, Option<String>, InvokeOutcome<T>) =
        match result {
            Ok(Ok(value)) => (PluginOutcome::Success, None, InvokeOutcome::Success(value)),
            Ok(Err(PluginError::Aborted { plugin, reason })) => {
                let err_string = format!("plugin {plugin} aborted request: {reason}");
                (
                    PluginOutcome::Aborted,
                    Some(err_string),
                    InvokeOutcome::Propagate(PluginError::Aborted { plugin, reason }),
                )
            }
            Ok(Err(PluginError::ProtectedFieldMutated { plugin, hook: h, field })) => {
                let err_string = format!(
                    "plugin {plugin} modified protected field `{field}` in {h}"
                );
                let outcome = PluginOutcome::ProtectedFieldMutated;
                let propagate = PluginError::ProtectedFieldMutated {
                    plugin,
                    hook: h,
                    field,
                };
                let returned = match args.on_error {
                    OnError::Skip => InvokeOutcome::Skipped,
                    OnError::Fail => InvokeOutcome::Propagate(propagate),
                };
                (outcome, Some(err_string), returned)
            }
            Ok(Err(PluginError::Failed { plugin, hook: h, message })) => {
                let err_string = format!("plugin {plugin} failed in {h}: {message}");
                let returned = match args.on_error {
                    OnError::Skip => InvokeOutcome::Skipped,
                    OnError::Fail => InvokeOutcome::Propagate(PluginError::Failed {
                        plugin,
                        hook: h,
                        message,
                    }),
                };
                let outcome = match args.on_error {
                    OnError::Skip => PluginOutcome::Skipped,
                    OnError::Fail => PluginOutcome::Failed,
                };
                (outcome, Some(err_string), returned)
            }
            Ok(Err(PluginError::Timeout { plugin, hook: h, elapsed_ms })) => {
                // Re-emitting Timeout for an inner future that returned it
                // (rare; the outer tokio::time::timeout typically races first).
                let err_string =
                    format!("plugin {plugin} timed out after {elapsed_ms}ms in {h}");
                let returned = match args.on_error {
                    OnError::Skip => InvokeOutcome::Skipped,
                    OnError::Fail => InvokeOutcome::Propagate(PluginError::Timeout {
                        plugin,
                        hook: h,
                        elapsed_ms,
                    }),
                };
                (PluginOutcome::TimedOut, Some(err_string), returned)
            }
            Err(_elapsed) => {
                let err = PluginError::Timeout {
                    plugin: args.plugin_name.to_string(),
                    hook: args.hook,
                    elapsed_ms,
                };
                let err_string = err.to_string();
                let returned = match args.on_error {
                    OnError::Skip => InvokeOutcome::Skipped,
                    OnError::Fail => InvokeOutcome::Propagate(err),
                };
                (PluginOutcome::TimedOut, Some(err_string), returned)
            }
        };

    // Record fields on the span. No-op on Span::none() (Aggregated mode).
    span.record("plugin.outcome", outcome.as_label());
    span.record("plugin.elapsed_ms", elapsed_ms);

    // Structured log emission (Q3). H5 success skips inside emit().
    let fields = PluginLogFields {
        plugin_name: args.plugin_name,
        plugin_kind: args.plugin_kind,
        plugin_hook: args.hook,
        request_id: &ctx.request_id,
        route: &ctx.route_label,
        outcome,
        elapsed_ms,
        on_error_policy: args.on_error,
        error: err_for_log.as_deref(),
    };
    fields.emit();

    // Prometheus metric (always update, including H5 success).
    agent_shim_observability::metrics::recorders::record_plugin_invocation(
        args.plugin_kind,
        args.plugin_name,
        args.hook,
        outcome.as_label(),
        elapsed_ms as f64 / 1000.0,
    );

    returned
}

/// Check that a plugin did not mutate any of the four protected
/// fields. Returns `Err(ProtectedFieldMutated)` if it did, naming the
/// first field that changed.
///
/// Protected fields: `id`, `frontend`, `model`, `stream`. The four
/// fields that drive routing, identity, and pipeline branching.
/// (Spec §4.5 / Q3.)
#[allow(dead_code)] // used by P04 run_* methods
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

    // ── invoke() — InvokeArgs shape (P05) ──────────────────────────────

    use agent_shim_core::RequestId as RID;
    use crate::trait_def::Hook;

    fn ctx() -> PluginContext {
        PluginContext {
            request_id: RID::new(),
            frontend: agent_shim_core::FrontendKind::AnthropicMessages,
            route_label: "anthropic_messages/m".to_string(),
        }
    }

    fn args_for(name: &str, hook: &'static str) -> InvokeArgs<'_> {
        InvokeArgs {
            plugin_name: name,
            plugin_kind: "test_kind",
            hook,
            timeout_ms: 50,
            on_error: OnError::Skip,
            span_mode: SpanMode::PerInvocation,
        }
    }

    #[tokio::test]
    async fn invoke_with_args_success_returns_value() {
        let c = ctx();
        let a = args_for("p", "on_decoded_request");
        let out: InvokeOutcome<i32> = invoke(a, &c, async { Ok(42) }).await;
        match out {
            InvokeOutcome::Success(v) => assert_eq!(v, 42),
            _ => panic!("expected Success"),
        }
    }

    #[tokio::test]
    async fn invoke_with_args_failed_skip_returns_skipped() {
        let c = ctx();
        let a = args_for("p", "on_decoded_request");
        let out: InvokeOutcome<i32> = invoke(
            a,
            &c,
            async {
                Err(PluginError::Failed {
                    plugin: "p".to_string(),
                    hook: "on_decoded_request",
                    message: "boom".to_string(),
                })
            },
        )
        .await;
        assert!(matches!(out, InvokeOutcome::Skipped));
    }

    #[tokio::test]
    async fn invoke_with_args_failed_fail_propagates() {
        let c = ctx();
        let mut a = args_for("p", "on_decoded_request");
        a.on_error = OnError::Fail;
        let out: InvokeOutcome<i32> = invoke(
            a,
            &c,
            async {
                Err(PluginError::Failed {
                    plugin: "p".to_string(),
                    hook: "on_decoded_request",
                    message: "boom".to_string(),
                })
            },
        )
        .await;
        assert!(matches!(out, InvokeOutcome::Propagate(_)));
    }

    #[tokio::test]
    async fn invoke_with_args_aborted_always_propagates() {
        let c = ctx();
        let a = args_for("p", "on_decoded_request"); // Skip
        let out: InvokeOutcome<i32> = invoke(
            a,
            &c,
            async {
                Err(PluginError::Aborted {
                    plugin: "p".to_string(),
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
    async fn invoke_with_args_timeout_skip_returns_skipped() {
        let c = ctx();
        let mut a = args_for("p", "on_decoded_request");
        a.timeout_ms = 10;
        let out: InvokeOutcome<i32> = invoke(
            a,
            &c,
            async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(42)
            },
        )
        .await;
        assert!(matches!(out, InvokeOutcome::Skipped));
    }

    #[tokio::test]
    async fn invoke_aggregated_span_mode_does_not_panic() {
        let c = ctx();
        let mut a = args_for("p", "on_stream_event");
        a.span_mode = SpanMode::Aggregated;
        let out: InvokeOutcome<i32> = invoke(a, &c, async { Ok(99) }).await;
        assert!(matches!(out, InvokeOutcome::Success(99)));
    }

    // ── check_protected_fields (unchanged from P04) ────────────────────

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
        b.messages = vec![];
        b.system = vec![];
        assert!(check_protected_fields("p", "h", &a, &b).is_ok());
    }
}
```

- [ ] **Step 3: Run plugin crate tests (will FAIL on registry.rs callsites)**

Run: `cargo build -p agent-shim-plugins`
Expected: FAIL — `registry.rs` still calls the old 6-arg `invoke()` form. Compile errors point to those 4 callsites. This is the TDD red state for Task 6.

Do not commit yet — Tasks 5 + 6 fix the callsites.

---

## Task 5: `PluginEntry.kind` field + rustdoc constraint + `for_testing_single_plugin` update

**Files:**
- Modify: `crates/plugins/src/registry.rs`
- Modify: `crates/plugins/src/trait_def.rs`

- [ ] **Step 1: Add rustdoc constraint on `Plugin::kind_name()`**

In `crates/plugins/src/trait_def.rs`, replace the rustdoc on `kind_name()` (around line 96-99):

```rust
    /// Globally-unique kind name. Matches the YAML `type:` field.
    ///
    /// The returned value MUST be a string literal (`&'static str`
    /// referring to constant data). DO NOT return `Box::leak`-derived
    /// references — `PluginEntry.kind` caches this value and metric
    /// labels rely on its stable identity for cardinality bounds
    /// (P05 §7.4). Runtime-generated kind names are not supported.
    fn kind_name(&self) -> &'static str;
```

- [ ] **Step 2: Add `kind` field to `PluginEntry` struct**

In `crates/plugins/src/registry.rs`, find the `PluginEntry` struct (around line 63) and add `kind`:

```rust
/// One entry in the registry, plus the policy knobs bound to it.
pub struct PluginEntry {
    pub name: String,
    /// Cached `Plugin::kind_name()` value — `&'static str` literal.
    /// Populated at construction. P05 §7.1 / Q1.
    pub kind: &'static str,
    pub plugin: Arc<dyn Plugin>,
    pub on_error: OnError,
    pub timeouts: HookTimeouts,
    pub enabled: bool,
}
```

- [ ] **Step 3: Update `for_testing_single_plugin` constructor**

Still in `registry.rs`, find `for_testing_single_plugin` (introduced in P04 T12, around line 130). Update the `PluginEntry` literal to populate `kind`:

```rust
    #[doc(hidden)]
    pub fn for_testing_single_plugin(
        name: &str,
        plugin: Arc<dyn crate::Plugin>,
        on_error: OnError,
        hook: Hook,
        frontend: FrontendKind,
        model: &str,
    ) -> Self {
        let kind = plugin.kind_name();
        let entry = Arc::new(PluginEntry {
            name: name.to_string(),
            kind,
            plugin,
            on_error,
            timeouts: HookTimeouts::default(),
            enabled: true,
        });
        // ... rest unchanged ...
```

- [ ] **Step 4: Update existing in-file tests that build `PluginEntry` literals**

Search the `tests` mod inside `registry.rs` for `PluginEntry {` literal construction. Each such site (there are roughly 4-5 in P04's tests for `run_on_decoded_request`, `run_on_resolved`, `wrap_stream`, `run_on_response_complete`) needs `kind: "test_kind",` (or whatever the test plugin's `kind_name` returns) added.

For each test plugin struct that impls `Plugin`, ensure its `kind_name()` returns a literal like `"counter"` / `"resolved_counter"` etc. (these already exist from P04 — leave the strings as-is). Then the `PluginEntry` construction sites add:

```rust
        let a = Arc::new(PluginEntry {
            name: "a".to_string(),
            kind: "counter",   // ← NEW: matches CounterPlugin::kind_name()
            plugin: Arc::new(CounterPlugin { n: counter_a }),
            on_error: OnError::Skip,
            timeouts: HookTimeouts::default(),
            enabled: true,
        });
```

Repeat for `b`, `rc` (resolved_counter), `d` (drop_every_second), `f` (always_fail), `rs` (record_summary). The kind values match the in-test `kind_name()` impls already present.

- [ ] **Step 5: Build to verify struct change ripples cleanly**

Run: `cargo build -p agent-shim-plugins`
Expected: STILL FAIL on registry.rs invoke() callsites (Task 6 fixes those), but no new errors related to `PluginEntry` construction.

- [ ] **Step 6: Commit T3+T4+T5 together (logical unit)**

```bash
git add crates/plugins/src/log_fields.rs \
        crates/plugins/src/lib.rs \
        crates/plugins/src/invoke.rs \
        crates/plugins/src/registry.rs \
        crates/plugins/src/trait_def.rs
git commit -m "feat(plugins): LogFields + InvokeArgs + SpanMode + PluginEntry.kind (P05 T3-T5)"
```

(Note: this commits T3+T4+T5 jointly because the workspace doesn't compile until all three land — TDD red persists until T6's callsite fix.)

---

## Task 6: Refactor 4 P04 callsites in registry.rs to `InvokeArgs::from_entry`

**Files:**
- Modify: `crates/plugins/src/registry.rs`

P04 has 4 `crate::invoke::invoke(...)` callsites in `registry.rs`:
1. `run_on_decoded_request` body
2. `run_on_resolved` body
3. `wrap_stream` then-closure (H5)
4. `run_on_response_complete` tokio::spawn body (will be rewired in T8 — leave alone here)

- [ ] **Step 1: Update `run_on_decoded_request` callsite**

In `crates/plugins/src/registry.rs`, find `run_on_decoded_request` (around line 200). Replace the `invoke()` call with the new form:

```rust
            let outcome = crate::invoke::invoke(
                crate::invoke::InvokeArgs::from_entry(
                    entry,
                    Hook::DecodedRequest,
                    crate::invoke::SpanMode::PerInvocation,
                ),
                ctx,
                plugin.on_decoded_request(ctx, candidate),
            )
            .await;
```

Delete the now-unused local bindings `plugin_name`, `plugin`, `hook_str`:

```rust
        for entry in &plan.on_decoded_request {
            if !entry.enabled {
                continue;
            }
            let candidate = req.clone();
            let plugin = entry.plugin.clone();
            // hook_str still needed downstream for check_protected_fields:
            let hook_str = Hook::DecodedRequest.as_str();
            let plugin_name = entry.name.clone();
            let outcome = crate::invoke::invoke(
                crate::invoke::InvokeArgs::from_entry(
                    entry,
                    Hook::DecodedRequest,
                    crate::invoke::SpanMode::PerInvocation,
                ),
                ctx,
                plugin.on_decoded_request(ctx, candidate),
            )
            .await;
            // ... rest of the match unchanged (uses `plugin_name` / `hook_str` in check_protected_fields call)
```

(The `plugin_name` + `hook_str` locals stay because `check_protected_fields` after Success uses them.)

- [ ] **Step 2: Update `run_on_resolved` callsite**

In the same file, find `run_on_resolved` (around line 270). Same pattern:

```rust
            let outcome = crate::invoke::invoke(
                crate::invoke::InvokeArgs::from_entry(
                    entry,
                    Hook::Resolved,
                    crate::invoke::SpanMode::PerInvocation,
                ),
                ctx,
                plugin.on_resolved(ctx, candidate, target),
            )
            .await;
```

- [ ] **Step 3: Update `wrap_stream` H5 callsite**

In `wrap_stream` (around line 320), the inner `then` closure. **H5 uses `SpanMode::Aggregated`**:

```rust
                        let outcome =
                            crate::invoke::invoke::<Vec<agent_shim_core::StreamEvent>, _>(
                                crate::invoke::InvokeArgs::from_entry(
                                    entry,
                                    Hook::StreamEvent,
                                    crate::invoke::SpanMode::Aggregated,
                                ),
                                &ctx,
                                plugin.on_stream_event(&ctx, ev_for_invoke),
                            )
                            .await;
```

- [ ] **Step 4: Update `run_on_response_complete` callsite**

In `run_on_response_complete` (around line 400). H7 = `PerInvocation`. (Note: this site will be REWIRED in T8 to route through `supervisor.spawn_h7` — for now just update the args form.)

```rust
            tokio::spawn(async move {
                let _ = crate::invoke::invoke::<(), _>(
                    crate::invoke::InvokeArgs {
                        plugin_name: &plugin_name,
                        plugin_kind: plugin_kind,
                        hook: Hook::ResponseComplete.as_str(),
                        timeout_ms,
                        on_error,
                        span_mode: crate::invoke::SpanMode::PerInvocation,
                    },
                    &ctx,
                    plugin.on_response_complete(&ctx, &summary),
                )
                .await;
            });
```

The `from_entry` builder can't be used here because the function moves the entry's fields into local clones before spawn. Add a `plugin_kind` local before the spawn:

```rust
        for entry in &plan.on_response_complete {
            if !entry.enabled {
                continue;
            }
            let plugin = entry.plugin.clone();
            let plugin_name = entry.name.clone();
            let plugin_kind: &'static str = entry.kind;  // ← NEW
            let timeout_ms = entry.timeouts.for_hook(Hook::ResponseComplete);
            let on_error = entry.on_error;
            let summary = summary.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move {
                let _ = crate::invoke::invoke::<(), _>(
                    crate::invoke::InvokeArgs {
                        plugin_name: &plugin_name,
                        plugin_kind,
                        hook: Hook::ResponseComplete.as_str(),
                        timeout_ms,
                        on_error,
                        span_mode: crate::invoke::SpanMode::PerInvocation,
                    },
                    &ctx,
                    plugin.on_response_complete(&ctx, &summary),
                )
                .await;
            });
        }
```

- [ ] **Step 5: Build + run plugins tests (TDD green)**

Run: `cargo build -p agent-shim-plugins && cargo nextest run -p agent-shim-plugins`
Expected: PASS — workspace compiles, all P04 + new P05 unit tests pass (around 30-32 tests in this crate).

- [ ] **Step 6: Run full workspace build to catch downstream breakage**

Run: `cargo build --workspace --tests`
Expected: PASS — gateway crate may need callsite updates if its tests build `PluginEntry` literals; check `crates/gateway/tests/plugins_pipeline.rs` and `crates/gateway/tests/vision_capability_mismatch.rs` etc.

If gateway tests have `PluginEntry { ... }` literals missing `kind:`, those tests need `kind: "test_kind"` (or matching `kind_name()` of their stub plugin) added. Locate via grep:

```bash
rtk grep -n "PluginEntry {" crates/gateway/tests/
```

Each match needs the `kind` field added.

- [ ] **Step 7: Commit**

```bash
git add crates/plugins/src/registry.rs crates/gateway/tests/
git commit -m "refactor(plugins): callsites use InvokeArgs::from_entry (P05 T6)"
```

---

## Task 7: `PluginSupervisor` (new file) + unit tests

**Files:**
- Create: `crates/plugins/src/supervisor.rs`
- Modify: `crates/plugins/src/lib.rs`

- [ ] **Step 1: Add module declaration**

Edit `crates/plugins/src/lib.rs` — add `mod supervisor;` to the module list.

- [ ] **Step 2: Write the failing tests first**

Create `crates/plugins/src/supervisor.rs` with type stubs + failing tests:

```rust
//! `PluginSupervisor` — owns the JoinSet of H7 (on_response_complete)
//! futures and provides bounded-deadline shutdown flush.
//!
//! Spec §6.8 + §7.4 P05.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinSet;

/// Owns the H7 task lifecycle: per-spawn JoinSet membership +
/// pending-attribution counter for shutdown drop reporting.
pub struct PluginSupervisor {
    /// std::sync::Mutex (NOT tokio::sync): critical sections are
    /// nanosecond-scale (JoinSet::spawn or std::mem::take), never held
    /// across .await. Clippy await_holding_lock confirms.
    tasks: std::sync::Mutex<JoinSet<()>>,
    /// plugin_name → pending count. spawn_h7 increments; the task body
    /// (clone of this Arc) decrements on completion.
    pending: Arc<std::sync::Mutex<HashMap<String, u64>>>,
}

impl PluginSupervisor {
    pub fn new() -> Self {
        Self {
            tasks: std::sync::Mutex::new(JoinSet::new()),
            pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Spawn an H7 future. Sync function. Increments the pending counter
    /// for `plugin_name`; the spawned task decrements on completion.
    pub fn spawn_h7(
        &self,
        plugin_name: String,
        fut: impl Future<Output = ()> + Send + 'static,
    ) {
        *self
            .pending
            .lock()
            .unwrap()
            .entry(plugin_name.clone())
            .or_insert(0) += 1;
        let pending = Arc::clone(&self.pending);
        self.tasks.lock().unwrap().spawn(async move {
            fut.await;
            let mut p = pending.lock().unwrap();
            if let Some(cnt) = p.get_mut(&plugin_name) {
                *cnt = cnt.saturating_sub(1);
                if *cnt == 0 {
                    p.remove(&plugin_name);
                }
            }
        });
    }

    /// Wait for pending H7 tasks until `deadline` elapses. Returns
    /// `Vec<(plugin_name, dropped_count)>` for tasks that did not
    /// complete in time. Tasks remaining in the JoinSet on drop are
    /// aborted by Tokio.
    pub async fn flush_pending_h7(&self, deadline: Duration) -> Vec<(String, u64)> {
        // Take the JoinSet out of the Mutex for the flush duration.
        // After axum drain, no new H7 spawns happen — this is safe.
        let mut tasks = std::mem::take(&mut *self.tasks.lock().unwrap());

        let _ = tokio::time::timeout(deadline, async {
            while tasks.join_next().await.is_some() {}
        })
        .await;

        // Any remaining `pending` entries are tasks that did not finish
        // in time. Snapshot + return + clear.
        let mut p = self.pending.lock().unwrap();
        let dropped: Vec<(String, u64)> =
            p.iter().map(|(name, count)| (name.clone(), *count)).collect();
        p.clear();
        // tasks drops on function exit -> aborts in-flight survivors.
        drop(tasks);
        dropped
    }
}

impl Default for PluginSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_then_flush_completes_within_deadline() {
        let sup = PluginSupervisor::new();
        for i in 0..3 {
            sup.spawn_h7(format!("p{i}"), async {});
        }
        let dropped = sup.flush_pending_h7(Duration::from_secs(1)).await;
        assert!(
            dropped.is_empty(),
            "fast tasks completed; dropped must be empty, got {dropped:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn spawn_then_flush_drops_slow_tasks_returns_attribution() {
        let sup = PluginSupervisor::new();
        sup.spawn_h7("slow_plugin".to_string(), async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let dropped = sup.flush_pending_h7(Duration::from_millis(10)).await;
        assert_eq!(
            dropped,
            vec![("slow_plugin".to_string(), 1)],
            "single slow task dropped, attributed to plugin"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn spawn_concurrent_same_plugin_returns_aggregated_count() {
        let sup = PluginSupervisor::new();
        for _ in 0..5 {
            sup.spawn_h7("slow_plugin".to_string(), async {
                tokio::time::sleep(Duration::from_secs(60)).await;
            });
        }
        let dropped = sup.flush_pending_h7(Duration::from_millis(10)).await;
        assert_eq!(
            dropped,
            vec![("slow_plugin".to_string(), 5)],
            "5 same-plugin spawns aggregate to count=5"
        );
    }

    #[tokio::test]
    async fn spawn_many_fast_then_flush_handles_all() {
        let sup = PluginSupervisor::new();
        for i in 0..100 {
            sup.spawn_h7(format!("plug{i}"), async {});
        }
        let dropped = sup.flush_pending_h7(Duration::from_secs(2)).await;
        assert!(
            dropped.is_empty(),
            "100 fast tasks all completed within deadline"
        );
    }
}
```

- [ ] **Step 3: Run supervisor tests**

Run: `cargo nextest run -p agent-shim-plugins supervisor::`
Expected: PASS — 4 tests.

- [ ] **Step 4: Commit**

```bash
git add crates/plugins/src/supervisor.rs crates/plugins/src/lib.rs
git commit -m "feat(plugins): PluginSupervisor with bounded H7 flush (P05 T7)"
```

---

## Task 8: `PluginRegistry.supervisor` + `run_on_response_complete` uses supervisor

**Files:**
- Modify: `crates/plugins/src/registry.rs`
- Modify: `crates/plugins/src/lib.rs`

- [ ] **Step 1: Expose `PluginSupervisor` from the lib**

Edit `crates/plugins/src/lib.rs` — add `pub use supervisor::PluginSupervisor;` next to the other `pub use` lines.

- [ ] **Step 2: Add `supervisor` field + accessor on `PluginRegistry`**

In `crates/plugins/src/registry.rs`, modify the `PluginRegistry` struct (around line 103):

```rust
pub struct PluginRegistry {
    #[allow(dead_code)]
    pub(crate) plugins: HashMap<String, Arc<PluginEntry>>,
    pub(crate) plans: HashMap<FrontendKind, FrontendRoutePlans>,
    /// Owns H7 task lifecycle. Lives as long as the registry; gateway
    /// shutdown calls `flush_pending_h7` to drain. P05 §6.8.
    pub(crate) supervisor: Arc<crate::supervisor::PluginSupervisor>,
}
```

Update `empty()` (around line 113):

```rust
    pub fn empty() -> Self {
        Self {
            plugins: HashMap::new(),
            plans: HashMap::new(),
            supervisor: Arc::new(crate::supervisor::PluginSupervisor::new()),
        }
    }
```

Update `for_testing_single_plugin` (around line 130) to populate the new field — same value:

```rust
        Self {
            plugins,
            plans,
            supervisor: Arc::new(crate::supervisor::PluginSupervisor::new()),
        }
```

Add the public flush accessor (after `empty()`):

```rust
    /// Flush pending H7 tasks during shutdown. See `PluginSupervisor::flush_pending_h7`.
    /// Returns `Vec<(plugin_name, dropped_count)>`. P05 §6.8.
    pub async fn flush_pending_h7(&self, deadline: Duration) -> Vec<(String, u64)> {
        self.supervisor.flush_pending_h7(deadline).await
    }
```

Add `use std::time::Duration;` at the top of the file if not already present.

- [ ] **Step 3: Reroute `run_on_response_complete` through supervisor**

Find `run_on_response_complete` in `registry.rs` (around line 400). Replace the bare `tokio::spawn` with `self.supervisor.spawn_h7`:

```rust
    pub fn run_on_response_complete(
        &self,
        route: (FrontendKind, &str),
        ctx: crate::PluginContext,
        summary: crate::ResponseSummary,
    ) {
        let Some(plan) = self.lookup(route.0, route.1) else {
            return;
        };
        if plan.on_response_complete.is_empty() {
            return;
        }
        let summary = Arc::new(summary);
        let ctx = Arc::new(ctx);
        for entry in &plan.on_response_complete {
            if !entry.enabled {
                continue;
            }
            let plugin = entry.plugin.clone();
            let plugin_name = entry.name.clone();
            let plugin_kind: &'static str = entry.kind;
            let timeout_ms = entry.timeouts.for_hook(Hook::ResponseComplete);
            let on_error = entry.on_error;
            let summary = summary.clone();
            let ctx = ctx.clone();
            // Route through supervisor instead of bare tokio::spawn.
            // P05 §6.8: lets shutdown bound the wait.
            self.supervisor.spawn_h7(plugin_name.clone(), async move {
                let _ = crate::invoke::invoke::<(), _>(
                    crate::invoke::InvokeArgs {
                        plugin_name: &plugin_name,
                        plugin_kind,
                        hook: Hook::ResponseComplete.as_str(),
                        timeout_ms,
                        on_error,
                        span_mode: crate::invoke::SpanMode::PerInvocation,
                    },
                    &ctx,
                    plugin.on_response_complete(&ctx, &summary),
                )
                .await;
            });
        }
    }
```

- [ ] **Step 4: Add a test asserting `flush_pending_h7` works end-to-end through registry**

In `registry.rs` test mod, after the existing `run_on_response_complete_*` tests, add:

```rust
    #[tokio::test(start_paused = true)]
    async fn flush_pending_h7_drops_slow_h7_plugin() {
        use crate::ResponseSummary;

        struct SlowH7;
        #[async_trait]
        impl crate::Plugin for SlowH7 {
            fn kind_name(&self) -> &'static str {
                "slow_h7"
            }
            fn hooks(&self) -> crate::HookSet {
                crate::HookSet::RESPONSE_COMPLETE
            }
            async fn on_response_complete(
                &self,
                _ctx: &crate::PluginContext,
                _summary: &ResponseSummary,
            ) -> crate::PluginResult<()> {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(())
            }
        }

        // Build registry with a slow H7 plugin.
        let entry = Arc::new(PluginEntry {
            name: "slow".to_string(),
            kind: "slow_h7",
            plugin: Arc::new(SlowH7),
            on_error: OnError::Skip,
            timeouts: HookTimeouts::default(),
            enabled: true,
        });
        let plan = RouteHookPlan {
            on_response_complete: vec![entry.clone()],
            ..Default::default()
        };
        let mut plans = HashMap::new();
        plans.insert(
            FrontendKind::AnthropicMessages,
            FrontendRoutePlans {
                specific: {
                    let mut m = HashMap::new();
                    m.insert("m".to_string(), plan);
                    m
                },
                wildcard: None,
                is_empty: false,
            },
        );
        let registry = PluginRegistry {
            plugins: {
                let mut p = HashMap::new();
                p.insert("slow".to_string(), entry);
                p
            },
            plans,
            supervisor: Arc::new(crate::supervisor::PluginSupervisor::new()),
        };
        let ctx = crate::PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        registry.run_on_response_complete(
            (FrontendKind::AnthropicMessages, "m"),
            ctx,
            ResponseSummary {
                usage: None,
                elapsed_ms: 0,
                upstream_status: crate::UpstreamStatus::Success,
            },
        );

        // Yield once so the spawn actually runs and registers in pending.
        tokio::task::yield_now().await;

        let dropped = registry
            .flush_pending_h7(Duration::from_millis(10))
            .await;
        assert_eq!(
            dropped,
            vec![("slow".to_string(), 1)],
            "slow H7 plugin dropped with attribution"
        );
    }
```

- [ ] **Step 5: Run tests**

Run: `cargo nextest run -p agent-shim-plugins`
Expected: PASS — all P04 + P05 tests including the new flush test.

- [ ] **Step 6: Update integration tests that build `PluginRegistry` literals**

Look at `crates/gateway/tests/plugins_pipeline.rs` and similar. Any `PluginRegistry { plugins, plans }` literal needs `supervisor: Arc::new(agent_shim_plugins::PluginSupervisor::new()),` added. (Tests using `PluginRegistry::for_testing_single_plugin` or `::empty()` are already covered.)

Run: `cargo build --workspace --tests` and fix any compile errors by adding `supervisor` field.

- [ ] **Step 7: Commit**

```bash
git add crates/plugins/src/registry.rs crates/plugins/src/lib.rs crates/gateway/tests/
git commit -m "feat(plugins): PluginRegistry.supervisor + H7 routed through it (P05 T8)"
```

---

## Task 9: `wrap_stream` H5 span aggregation — `GuardedH5Stream`, `StreamSpanRecorder`, AtomicU64 counters

**Files:**
- Modify: `crates/plugins/src/registry.rs`

- [ ] **Step 1: Add the two private types at the top of `registry.rs` (after imports, before `PluginRegistry`)**

```rust
use std::sync::atomic::{AtomicU64, Ordering};

// ── H5 span aggregation helpers (P05 §3) ─────────────────────────────

/// Drop-time recorder that writes aggregated counters onto the
/// `plugin.stream` span. `Span::record` after the span has been entered
/// in `GuardedH5Stream::poll_next` populates the span's fields just
/// before close, so trace backends see the final values.
struct StreamSpanRecorder {
    span: tracing::Span,
    event_count: Arc<AtomicU64>,
    failure_count: Arc<AtomicU64>,
}

impl Drop for StreamSpanRecorder {
    fn drop(&mut self) {
        self.span.record(
            "plugin.event_count",
            self.event_count.load(Ordering::Relaxed),
        );
        self.span.record(
            "plugin.failure_count",
            self.failure_count.load(Ordering::Relaxed),
        );
    }
}

/// Stream wrapper that enters the `plugin.stream` span on every poll
/// and owns the drop-guard recorder. `Entered<'_>` is `!Send` but
/// scope-bounded inside `poll_next`, which is fine because poll runs
/// synchronously on a single thread.
struct GuardedH5Stream<S> {
    inner: S,
    span: tracing::Span,
    _recorder: StreamSpanRecorder,
}

impl<S: futures::Stream + Unpin> futures::Stream for GuardedH5Stream<S> {
    type Item = S::Item;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let _enter = self.span.enter();
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}
```

- [ ] **Step 2: Modify `wrap_stream` to install the span + counters + wrap with `GuardedH5Stream`**

Replace the body of `wrap_stream` (after the fast-path bail-out):

```rust
    pub fn wrap_stream(
        &self,
        route: (FrontendKind, &str),
        ctx: crate::PluginContext,
        upstream: agent_shim_core::CanonicalStream,
    ) -> agent_shim_core::CanonicalStream {
        let plan = match self.lookup(route.0, route.1) {
            Some(p) if !p.on_stream_event.is_empty() => p.clone(),
            _ => return upstream,
        };
        let plugins: Vec<Arc<PluginEntry>> = plan.on_stream_event.clone();

        // P05 §3: aggregated plugin.stream span. NO plugin.name field
        // (covers all H5 plugins). Per-plugin attribution flows through
        // per-event log lines on failure (§7.5 noise control + Q14).
        let span = tracing::info_span!(
            "plugin.stream",
            "agent_shim.request_id" = %ctx.request_id,
            "agent_shim.route" = %ctx.route_label,
            "plugin.event_count" = tracing::field::Empty,
            "plugin.failure_count" = tracing::field::Empty,
        );
        let event_count = Arc::new(AtomicU64::new(0));
        let failure_count = Arc::new(AtomicU64::new(0));
        let recorder = StreamSpanRecorder {
            span: span.clone(),
            event_count: Arc::clone(&event_count),
            failure_count: Arc::clone(&failure_count),
        };

        let ctx = Arc::new(ctx);

        use futures::StreamExt;
        let inner = upstream
            .then(move |event_result| {
                let plugins = plugins.clone();
                let ctx = ctx.clone();
                let event_count = Arc::clone(&event_count);
                let failure_count = Arc::clone(&failure_count);
                async move {
                    // Upstream errors pass through unchanged (no plugin sees them).
                    let event = match event_result {
                        Ok(e) => e,
                        Err(err) => return vec![Err(err)],
                    };
                    event_count.fetch_add(1, Ordering::Relaxed);

                    let mut buf: Vec<agent_shim_core::StreamEvent> = vec![event];
                    for entry in &plugins {
                        if !entry.enabled {
                            continue;
                        }
                        let mut next: Vec<agent_shim_core::StreamEvent> =
                            Vec::with_capacity(buf.len());
                        for ev in buf.drain(..) {
                            let ev_for_invoke = ev.clone();
                            let ev_for_skip = ev;
                            let outcome = crate::invoke::invoke::<
                                Vec<agent_shim_core::StreamEvent>,
                                _,
                            >(
                                crate::invoke::InvokeArgs::from_entry(
                                    entry,
                                    Hook::StreamEvent,
                                    crate::invoke::SpanMode::Aggregated,
                                ),
                                &ctx,
                                entry.plugin.on_stream_event(&ctx, ev_for_invoke),
                            )
                            .await;
                            match outcome {
                                crate::invoke::InvokeOutcome::Success(events) => {
                                    next.extend(events);
                                }
                                crate::invoke::InvokeOutcome::Skipped => {
                                    next.push(ev_for_skip);
                                }
                                crate::invoke::InvokeOutcome::Propagate(err) => {
                                    failure_count.fetch_add(1, Ordering::Relaxed);
                                    return vec![Ok(agent_shim_core::StreamEvent::Error {
                                        message: err.to_string(),
                                    })];
                                }
                            }
                        }
                        buf = next;
                    }
                    buf.into_iter().map(Ok).collect::<Vec<_>>()
                }
            })
            .flat_map(futures::stream::iter);

        Box::pin(GuardedH5Stream {
            inner,
            span,
            _recorder: recorder,
        })
    }
```

- [ ] **Step 3: Run tests including the existing wrap_stream coverage**

Run: `cargo nextest run -p agent-shim-plugins wrap_stream`
Expected: PASS — existing P04 tests (`wrap_stream_fast_path_returns_identity`, `wrap_stream_h5_plugin_drops_every_other_event`, `wrap_stream_plugin_failure_emits_error_event_and_stops`) all pass because behavior is preserved.

Run: `cargo build --workspace --tests`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/plugins/src/registry.rs
git commit -m "feat(plugins): plugin.stream span + GuardedH5Stream H5 aggregation (P05 T9)"
```

---

## Task 10: `ShutdownConfig` in config schema + Layer A validation

**Files:**
- Modify: `crates/config/src/schema.rs`
- Modify: `crates/config/src/validation.rs`

- [ ] **Step 1: Add `ShutdownConfig` struct + `GatewayConfig.shutdown` field**

In `crates/config/src/schema.rs`, after `MetricsConfig` definition (search for `pub struct MetricsConfig` — around line 100-130 area), add:

```rust
/// Shutdown lifecycle timing knobs. P05 §8 / Q8.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ShutdownConfig {
    /// Seconds to wait for H7 (`on_response_complete`) plugin tasks to
    /// complete during gateway shutdown before dropping them.
    /// Default 5. Layer A validation rejects values > 300.
    #[serde(default = "default_plugin_flush_secs")]
    pub plugin_flush_secs: u64,
}

fn default_plugin_flush_secs() -> u64 {
    5
}

impl ShutdownConfig {
    pub fn default_plugin_flush_secs() -> u64 {
        default_plugin_flush_secs()
    }
}
```

The free function `default_plugin_flush_secs()` must NOT collide with anything else in the module — if the module already has a `default_*` private fn, prefix this one as `default_shutdown_plugin_flush_secs` and update the `#[serde(default = ...)]` accordingly.

`Default::default()` for `ShutdownConfig` returns `plugin_flush_secs: 0` because of `#[derive(Default)]`. We want 5. Override:

```rust
impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            plugin_flush_secs: default_plugin_flush_secs(),
        }
    }
}
```

Remove `Default` from the derive list above to avoid double impl. Final shape:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ShutdownConfig {
    #[serde(default = "default_plugin_flush_secs")]
    pub plugin_flush_secs: u64,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            plugin_flush_secs: default_plugin_flush_secs(),
        }
    }
}

fn default_plugin_flush_secs() -> u64 {
    5
}
```

Add the new field to `GatewayConfig` (around line 35, after `otel`):

```rust
    #[serde(default)]
    pub otel: Option<OtelConfig>,
    /// Shutdown lifecycle config (Phase 7 P05).
    #[serde(default)]
    pub shutdown: ShutdownConfig,
}
```

- [ ] **Step 2: Write failing test for default-on-absence**

In `crates/config/src/schema.rs` (in the existing test mod):

```rust
    #[test]
    fn shutdown_default_when_absent() {
        let yaml = "server: { bind: 127.0.0.1, port: 8787 }\nupstreams: {}\nroutes: []";
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.shutdown.plugin_flush_secs, 5);
    }

    #[test]
    fn shutdown_explicit_value_parsed() {
        let yaml = r#"
server: { bind: 127.0.0.1, port: 8787 }
upstreams: {}
routes: []
shutdown:
  plugin_flush_secs: 30
"#;
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.shutdown.plugin_flush_secs, 30);
    }

    #[test]
    fn shutdown_rejects_unknown_field() {
        let yaml = r#"
server: { bind: 127.0.0.1, port: 8787 }
upstreams: {}
routes: []
shutdown:
  plugin_flush_secs: 5
  bogus_field: 1
"#;
        let r: Result<GatewayConfig, _> = serde_yaml::from_str(yaml);
        assert!(r.is_err(), "deny_unknown_fields must reject `bogus_field`");
    }
```

- [ ] **Step 3: Add Layer A validation**

In `crates/config/src/validation.rs`, find the main `pub fn validate(cfg: &GatewayConfig)` function (around line 175). Add the shutdown check near the top alongside the other early structural checks:

```rust
    // P05 §8: bound H7 flush deadline to a sensible upper limit so
    // misconfig can't stall shutdown forever.
    if cfg.shutdown.plugin_flush_secs > 300 {
        return Err(ValidationError::InvalidRoute(format!(
            "shutdown.plugin_flush_secs {} must be <= 300 seconds",
            cfg.shutdown.plugin_flush_secs
        )));
    }
```

(Reuses `ValidationError::InvalidRoute` for the message — pattern already used elsewhere for "out of range" errors. If a more specific variant is preferred, add `ValidationError::InvalidShutdown(String)` to the enum, but reusing keeps the diff small.)

- [ ] **Step 4: Write failing test for validation**

In `crates/config/src/validation.rs` test mod (search `#[cfg(test)]`):

```rust
    #[test]
    fn validate_rejects_excessive_shutdown_flush_secs() {
        use crate::schema::ShutdownConfig;
        let mut cfg = minimal_valid_config();
        cfg.shutdown = ShutdownConfig {
            plugin_flush_secs: 301,
        };
        let err = validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("plugin_flush_secs"),
            "error message names the bad field, got: {err}"
        );
    }

    #[test]
    fn validate_accepts_max_shutdown_flush_secs() {
        use crate::schema::ShutdownConfig;
        let mut cfg = minimal_valid_config();
        cfg.shutdown = ShutdownConfig {
            plugin_flush_secs: 300,
        };
        assert!(validate(&cfg).is_ok());
    }
```

`minimal_valid_config()` is a helper that should already exist in the test mod; if not, find an existing test that builds a `GatewayConfig` from `serde_yaml::from_str` of a minimal YAML and crib from there.

- [ ] **Step 5: Run config tests**

Run: `cargo nextest run -p agent-shim-config`
Expected: PASS.

- [ ] **Step 6: Run workspace build**

Run: `cargo build --workspace --tests`
Expected: PASS — gateway tests that hand-build `GatewayConfig` literals may need `shutdown: Default::default(),` added. If so, fix those callsites.

```bash
rtk grep -n "GatewayConfig {" crates/gateway/tests/
```

Each match probably needs `shutdown: Default::default(),` somewhere in the literal.

- [ ] **Step 7: Commit**

```bash
git add crates/config/src/schema.rs crates/config/src/validation.rs crates/gateway/tests/
git commit -m "feat(config): ShutdownConfig + plugin_flush_secs Layer A validation (P05 T10)"
```

---

## Task 11: Gateway `run_core` — plugin flush between axum await and otel shutdown

**Files:**
- Modify: `crates/gateway/src/commands/serve.rs`

- [ ] **Step 1: Locate `run_core` and identify the wedge point**

Open `crates/gateway/src/commands/serve.rs`. Find the function ending with axum's `run_on_listener` / `run_with_admin_on_listeners` await + the otel shutdown. The current structure (around line 130-148):

```rust
    let result = if let Some(admin_listener) = admin_listener_opt {
        crate::server::run_with_admin_on_listeners(...).await
    } else {
        crate::server::run_on_listener(public_listener, state, shutdown_signal).await
    };

    if let Some(otel) = tracing_handles.otel {
        otel.shutdown();
    }
    result
```

- [ ] **Step 2: Clone plugins Arc + read flush_secs BEFORE moving state into axum**

Modify the function — add the clone before the if-else:

```rust
    // P05 T11: clone the plugin Arc + shutdown.plugin_flush_secs BEFORE
    // moving `state` into axum. After axum drain, no new H7 spawns occur
    // (no requests in flight) so flush can run safely on this clone.
    let plugins = state.core.plugins.clone();
    let flush_secs = state.snapshot.load().config.shutdown.plugin_flush_secs;

    let result = if let Some(admin_listener) = admin_listener_opt {
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

    // P05 T11: flush H7 plugin tasks AFTER axum drain, BEFORE otel
    // shutdown — otel must still be live to flush the warn! lines
    // emitted for dropped tasks.
    let dropped = plugins
        .flush_pending_h7(std::time::Duration::from_secs(flush_secs))
        .await;
    for (plugin_name, count) in dropped {
        agent_shim_observability::metrics::recorders::record_h7_dropped(&plugin_name, count);
        tracing::warn!(
            "plugin.name" = %plugin_name,
            dropped_count = count,
            deadline_secs = flush_secs,
            "H7 task dropped at shutdown",
        );
    }

    if let Some(otel) = tracing_handles.otel {
        otel.shutdown();
    }
    result
```

- [ ] **Step 3: Run workspace build**

Run: `cargo build --workspace --tests`
Expected: PASS.

- [ ] **Step 4: Run full workspace tests as a regression gate**

Run: `cargo nextest run --workspace`
Expected: PASS (test count = previous baseline; no new test in this task — the end-to-end behavior is exercised in T12's integration test).

- [ ] **Step 5: Commit**

```bash
git add crates/gateway/src/commands/serve.rs
git commit -m "feat(gateway): flush plugin H7 between axum drain and otel shutdown (P05 T11)"
```

---

## Task 12: Gateway integration test — `plugins_observability.rs`

**Files:**
- Create: `crates/gateway/tests/plugins_observability.rs`

This test validates end-to-end Prometheus output contains plugin metrics. It uses the hand-built-`AppState` pattern from `crates/gateway/tests/plugins_pipeline.rs` (the P04 T12 file).

- [ ] **Step 1: Write the integration test file**

Create `crates/gateway/tests/plugins_observability.rs`:

```rust
//! Phase 7 P05 T12: gateway integration tests for plugin observability.
//!
//! Exercises the full HTTP→pipeline→provider path with hand-built
//! `PluginRegistry` instances, then renders Prometheus output to assert
//! plugin metrics are populated. Mirrors the pattern from
//! `plugins_pipeline.rs` (P04 T12).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use agent_shim_config::{
    schema::{LoggingConfig, RouteEntry, ServerConfig},
    GatewayConfig,
};
use agent_shim_core::{
    BackendTarget, CanonicalRequest, CanonicalStream, ContentBlock, ContentBlockKind,
    FrontendKind, Message, MessageRole, ResponseId, StopReason, StreamEvent, TextBlock,
};
use agent_shim_gateway::{server::build_router, state::AppState};
use agent_shim_plugins::{
    HookSet, OnError, Plugin, PluginContext, PluginRegistry, PluginResult,
};
use agent_shim_providers::{
    BackendProvider, ProviderCapabilities, ProviderError, ProviderRegistry,
};
use agent_shim_router::model_index::ModelIndex;
use agent_shim_router::{
    BreakerRegistry, ModelResolver, ProviderLookup, ResilientCaller, Router as RouterTrait,
    StaticRouter,
};
use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tower::util::ServiceExt;

// Reusable stub provider — same shape as P04's CapturingStubProvider but
// without the captured-request field (we don't inspect it here).
struct StubProvider {
    capabilities: ProviderCapabilities,
}

impl StubProvider {
    fn new() -> Self {
        Self {
            capabilities: ProviderCapabilities {
                streaming: true,
                tool_use: false,
                vision: false,
                json_mode: false,
            },
        }
    }
}

#[async_trait]
impl BackendProvider for StubProvider {
    fn name(&self) -> &'static str {
        "stub-provider"
    }
    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }
    async fn complete(
        &self,
        _req: CanonicalRequest,
        _target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError> {
        let events: Vec<Result<StreamEvent, agent_shim_core::StreamError>> = vec![
            Ok(StreamEvent::ResponseStart {
                id: ResponseId("test-resp".to_string()),
                model: "test-model".to_string(),
                created_at_unix: 0,
            }),
            Ok(StreamEvent::MessageStart {
                role: MessageRole::Assistant,
            }),
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                kind: ContentBlockKind::Text,
            }),
            Ok(StreamEvent::TextDelta {
                index: 0,
                text: "hi".to_string(),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                stop_sequence: None,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

fn make_app_state(plugins: Arc<PluginRegistry>) -> (AppState, Arc<agent_shim_observability::MetricsHandle>) {
    use agent_shim_frontends::{
        anthropic_messages::AnthropicMessages, openai_chat::OpenAiChat,
        openai_responses::OpenAiResponses,
    };
    use agent_shim_gateway::state::{AppCore, AppSnapshot};

    let keepalive = Some(Duration::from_secs(15));
    let anthropic = Arc::new(AnthropicMessages { keepalive });
    let openai = Arc::new(OpenAiChat { keepalive, clock_override: None });
    let openai_responses = Arc::new(OpenAiResponses { keepalive, clock_override: None });

    let mut registry = ProviderRegistry::new();
    let stub: Arc<dyn BackendProvider> = Arc::new(StubProvider::new());
    registry.register("stub-provider".into(), stub);

    let cfg = GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams: BTreeMap::new(),
        routes: vec![RouteEntry::singular(
            "anthropic_messages",
            "test-model",
            "stub-provider",
            "test-model",
        )],
        plugins: BTreeMap::new(),
        auth: Default::default(),
        rate_limit: Default::default(),
        copilot: None,
        admin: None,
        metrics: Default::default(),
        otel: None,
        shutdown: Default::default(),
    };

    let static_router: Arc<dyn RouterTrait> = Arc::new(StaticRouter::from_config(&cfg));
    let model_index = Arc::new(ModelIndex::new(Default::default()));
    let resolver = Arc::new(ModelResolver::new(static_router, model_index));

    let providers = Arc::new(registry);
    struct Lookup(Arc<ProviderRegistry>);
    impl ProviderLookup for Lookup {
        fn get(&self, name: &str) -> Option<Arc<dyn BackendProvider>> {
            self.0.get(name)
        }
    }
    let provider_lookup: Arc<dyn ProviderLookup> = Arc::new(Lookup(Arc::clone(&providers)));
    let breaker_registry = Arc::new(BreakerRegistry::with_system_clock());
    let limiter_registry = Arc::new(arc_swap::ArcSwap::from_pointee(
        agent_shim_router::LimiterRegistry::disabled(),
    ));
    let resilient_caller = Arc::new(ResilientCaller::new(
        provider_lookup,
        Arc::clone(&breaker_registry),
        Arc::clone(&limiter_registry),
        Arc::new(agent_shim_router::DisabledLatencyProbe)
            as Arc<dyn agent_shim_router::LatencyProbe>,
    ));

    let metrics = agent_shim_observability::install_metrics(&Default::default());

    let state = AppState {
        core: Arc::new(AppCore {
            config_path: None,
            server_config: cfg.server.clone(),
            admin_config: cfg.admin.clone(),
            anthropic,
            openai,
            openai_responses,
            providers,
            resolver,
            resilient_caller,
            breaker_registry,
            limiter_registry,
            metrics: metrics.clone(),
            reload_tx: tokio::sync::mpsc::channel(1).0,
            plugins,
        }),
        snapshot: Arc::new(arc_swap::ArcSwap::new(Arc::new(AppSnapshot {
            config: Arc::new(cfg),
            auth_enabled: false,
            auth_required: false,
            configured_key_hashes: Arc::new(std::collections::HashSet::new()),
        }))),
    };
    (state, metrics)
}

const ANTHROPIC_BODY: &str = r#"{
    "model": "test-model",
    "max_tokens": 64,
    "messages": [{"role": "user", "content": "hi"}]
}"#;

#[tokio::test]
async fn h2_plugin_invocation_emits_prometheus_counter() {
    struct PassThrough;
    #[async_trait]
    impl Plugin for PassThrough {
        fn kind_name(&self) -> &'static str { "passthrough" }
        fn hooks(&self) -> HookSet { HookSet::DECODED_REQUEST }
        async fn on_decoded_request(
            &self,
            _ctx: &PluginContext,
            req: CanonicalRequest,
        ) -> PluginResult<CanonicalRequest> {
            Ok(req)
        }
    }

    let registry = Arc::new(PluginRegistry::for_testing_single_plugin(
        "p1",
        Arc::new(PassThrough),
        OnError::Skip,
        agent_shim_plugins::Hook::DecodedRequest,
        FrontendKind::AnthropicMessages,
        "test-model",
    ));
    let (state, metrics) = make_app_state(registry);
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_BODY))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = metrics.render();
    assert!(
        body.contains("agent_shim_plugin_invocations_total"),
        "Prometheus output must include plugin invocations counter"
    );
    assert!(
        body.contains(r#"plugin_kind="passthrough""#),
        "counter must carry plugin_kind label, got body: {body}"
    );
    assert!(
        body.contains(r#"plugin_name="p1""#),
        "counter must carry plugin_name label"
    );
    assert!(
        body.contains(r#"hook="on_decoded_request""#),
        "counter must carry hook label"
    );
    assert!(
        body.contains(r#"outcome="success""#),
        "successful invoke must emit outcome=success label"
    );
}

#[tokio::test]
async fn slow_h7_plugin_dropped_at_shutdown_increments_dropped_counter() {
    use agent_shim_plugins::ResponseSummary;

    struct SlowH7;
    #[async_trait]
    impl Plugin for SlowH7 {
        fn kind_name(&self) -> &'static str { "slow_h7" }
        fn hooks(&self) -> HookSet { HookSet::RESPONSE_COMPLETE }
        async fn on_response_complete(
            &self,
            _ctx: &PluginContext,
            _summary: &ResponseSummary,
        ) -> PluginResult<()> {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        }
    }

    let registry = Arc::new(PluginRegistry::for_testing_single_plugin(
        "slow",
        Arc::new(SlowH7),
        OnError::Skip,
        agent_shim_plugins::Hook::ResponseComplete,
        FrontendKind::AnthropicMessages,
        "test-model",
    ));
    let (state, metrics) = make_app_state(Arc::clone(&registry));
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_BODY))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Yield once so the H7 spawn registers in supervisor.
    tokio::task::yield_now().await;

    // Flush with tiny deadline — slow H7 should be dropped.
    let dropped = registry
        .flush_pending_h7(Duration::from_millis(10))
        .await;
    assert!(
        dropped.iter().any(|(name, _)| name == "slow"),
        "slow H7 plugin appears in dropped list, got: {dropped:?}"
    );

    // Simulate shutdown hook: record dropped → render → assert metric.
    for (name, count) in dropped {
        agent_shim_observability::metrics::recorders::record_h7_dropped(&name, count);
    }
    let body = metrics.render();
    assert!(
        body.contains("agent_shim_plugin_h7_dropped_at_shutdown_total"),
        "h7 dropped counter must be present in Prometheus output"
    );
    assert!(
        body.contains(r#"plugin_name="slow""#),
        "h7 dropped counter must carry plugin_name label"
    );
}

#[tokio::test]
async fn empty_registry_no_plugin_metrics_emitted() {
    let (state, metrics) = make_app_state(Arc::new(PluginRegistry::empty()));
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_BODY))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = metrics.render();
    // Counter is described (HELP/TYPE lines visible) but no observations.
    // Either: no `agent_shim_plugin_invocations_total ` data line at all,
    // OR a `# TYPE` header but no value lines. Assert the simpler shape:
    // any non-zero plugin counter contradicts the empty-registry contract.
    assert!(
        !body.lines().any(|l| {
            l.starts_with("agent_shim_plugin_invocations_total{")
                && !l.contains("} 0")
        }),
        "empty registry must not emit non-zero plugin counter, got body: {body}"
    );
}
```

- [ ] **Step 2: Build and run**

Run: `cargo nextest run --test plugins_observability`
Expected: PASS — 3 tests.

- [ ] **Step 3: Run full workspace tests**

Run: `cargo nextest run --workspace`
Expected: PASS, test count ~811 (baseline 789 + 10 LogFields tests + 6 invoke tests + 4 supervisor + 1 registry flush test + 2 config + 3 gateway integration ≈ +26; minor variation OK).

- [ ] **Step 4: Run clippy + fmt**

```
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Both clean.

- [ ] **Step 5: Verify frozen-core invariant**

```
git diff master..HEAD -- crates/core/
```

Expected: empty.

- [ ] **Step 6: Commit**

```bash
git add crates/gateway/tests/plugins_observability.rs
git commit -m "test(gateway): plugin observability integration tests (P05 T12)"
```

---

## Acceptance gates

After all 12 tasks land:

1. `cargo nextest run --workspace` — test count climbs from 789 to ~811 (+/- a few). Zero failures.
2. `cargo clippy --workspace --all-targets -- -D warnings` — clean.
3. `cargo fmt --all -- --check` — clean.
4. `git diff master..HEAD -- crates/core/` — empty (frozen-core preserved).
5. Manual smoke: launch `agent-shim serve --config config/gateway.example.yaml`; hit `/v1/messages`; `curl localhost:.../metrics | grep agent_shim_plugin_` — three metric families present.
6. Spec acceptance criteria 1-17 (see `docs/superpowers/specs/2026-05-15-phase-7-p05-observability-design.md`).

## Notes for the implementer

- **Task order matters.** T3+T4+T5 jointly commit because the workspace doesn't compile until all three land. After that, each task is independently green.
- **clippy::await_holding_lock**: only `std::sync::Mutex` is held in `PluginSupervisor`. Lock acquisition is via `.lock().unwrap()` and dropped before any `.await`. If clippy complains, look for an accidental `.await` while a `MutexGuard` is alive — fix by scoping the guard tighter.
- **Test parallelism + global recorder**: tests in `plugins_observability.rs` install the PrometheusRecorder once per test process via `agent_shim_observability::install_metrics`. The `OnceLock` inside makes this idempotent. If the test binary as a whole has multiple `#[tokio::test]` functions calling `install_metrics`, they all share the same handle — metric values accumulate across tests. The assertions above tolerate this because they grep for label presence, not exact counts.
- **`record_h7_dropped` counter zero observations**: the `empty_registry_no_plugin_metrics_emitted` test asserts no NON-zero observations on the invocations counter. The `agent_shim_plugin_h7_dropped_at_shutdown_total` counter may or may not appear depending on whether HELP/TYPE describe has been emitted; this is consistent with existing `metrics_endpoint.rs` test patterns.
- **`Plugin::kind_name()` literal-only enforcement**: there is no compile-time check. Acceptance criterion is rustdoc presence (grep). If a future contributor violates this, fix the impl rather than relaxing the doc.
- **If gateway tests fail to compile** after T5 or T8, the missing fields are `kind` on `PluginEntry` literals or `supervisor` on `PluginRegistry` literals. Search and patch.
