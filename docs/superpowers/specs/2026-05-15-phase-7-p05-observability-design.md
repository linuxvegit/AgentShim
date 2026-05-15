# Phase 7 P05 — Plugin Observability + H7 JoinSet Design

**Status:** Draft for review
**Date:** 2026-05-15
**Source spec:** `docs/superpowers/specs/2026-05-14-plugin-system-design.md` §7 (entire) + §6.8 (JoinSet wiring)
**Outline:** `docs/superpowers/plans/2026-05-14-phase-7-p03-p07-outline.md` § P05

## Goal

Make the plugin system **observable and gracefully terminable**:

1. Every plugin hook invocation emits a structured log line at the right level (§7.2) with the §7.1 field set, never including request content (§7.6).
2. Every invocation contributes to two Prometheus metrics (§7.4):
   - `agent_shim_plugin_invocations_total{plugin_kind, plugin_name, hook, outcome}`
   - `agent_shim_plugin_duration_seconds{plugin_kind, plugin_name, hook}`
3. Every H2/H3/H7 invocation runs inside a `plugin.invoke` OTel child span (§7.3). H5 batches per-stream invocations under one `plugin.stream` span instead of one per event (§7.5 spirit applied to spans).
4. H7 (`on_response_complete`) tasks land in a `tokio::task::JoinSet` so gateway shutdown can wait up to `plugins.shutdown_flush_secs` seconds for them, then drops survivors. A `agent_shim_plugin_h7_dropped_at_shutdown_total{plugin_name}` counter records the drop.

P05 ships the **only** Phase 7 observability work. P06 reuses these telemetry surfaces for built-in plugins. P07 wires the registry into hot-reload.

## Non-goals

- **No new plugin kinds** (P06).
- **No hot-reload of the registry** (P07).
- **No new plugin trait methods.** The `invoke()` template extension is internal; plugin authors see no API change.
- **No frozen-core changes.** `crates/core/` is untouched.
- **Histogram bucket overrides for plugin duration** stay at the `metrics-exporter-prometheus` 12-bucket default. Operators can override via `metrics.histogram_buckets` if they need to — P03 already exposed that knob.

## Architecture overview

```
┌─────────────────────────────────────────────────────────────────┐
│  Pipeline (gateway)                                             │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │ run_on_decoded_request / run_on_resolved / run_on_response │ │
│  │ _complete / wrap_stream  ── registry walks plan ──         │ │
│  └─────────────────────┬──────────────────────────────────────┘ │
└────────────────────────┼────────────────────────────────────────┘
                         ▼
┌─────────────────────────────────────────────────────────────────┐
│  PluginRegistry (plugins crate)                                 │
│  - plans (immutable, route → hook chain)                        │
│  - supervisor: Arc<PluginSupervisor>  ◄── NEW (P05)             │
│                                                                 │
│  H2/H3/H7 path: invoke() ── new per-call span ── log + metric   │
│  H5 path:       wrap_stream() opens ONE plugin.stream span;     │
│                 invoke() inside it skips its own span entry.    │
└──────────────────┬──────────────────────────┬───────────────────┘
                   ▼                          ▼
        ┌──────────────────┐         ┌──────────────────────────┐
        │  invoke() helper │         │  PluginSupervisor (NEW)  │
        │  - timeout       │         │  - tasks: Mutex<JoinSet> │
        │  - on_error      │         │  - spawn_h7(future)      │
        │  - protected diff│         │  - flush_pending_h7(dur) │
        │  - LogFields ◄── NEW       │    → returns dropped: usize
        │  - metric record◄── NEW    └──────────────────────────┘
        └────────┬─────────┘                       ▲
                 ▼                                 │ on shutdown
        ┌──────────────────────────┐               │
        │ observability crate      │               │
        │  metrics::recorders::    │               │
        │   record_plugin_invocation(...)          │
        │   record_h7_dropped(name)                │
        │   names::PLUGIN_*                        │
        │   catalog::iter_descriptors              │
        └──────────────────────────┘               │
                                                   │
                              ┌────────────────────┴──────────────┐
                              │ gateway::commands::serve / shutdown│
                              │   on shutdown signal:              │
                              │     plugins.flush_pending_h7(dur) ─┘
                              │   then continue normal shutdown    │
                              └────────────────────────────────────┘
```

## Components

### 1. `PluginLogFields` (`crates/plugins/src/log_fields.rs`, new file)

A whitelist struct enforcing §7.1 + §7.6 (PII red lines).

```rust
/// Whitelist of fields safe to emit to logs/metrics. Matches spec §7.1
/// verbatim. Adding a field MUST update §7.1 + the PII red-line doc.
///
/// NEVER add request content, user input, plugin output, or any
/// derived-from-user-content fields here. §7.6 PII red lines.
pub(crate) struct PluginLogFields<'a> {
    pub plugin_name: &'a str,
    pub plugin_kind: &'static str,
    pub plugin_hook: &'static str,
    pub request_id: &'a str,
    pub route: &'a str,
    pub outcome: PluginOutcome,
    pub elapsed_ms: u64,
    pub on_error_policy: OnError,
    pub error: Option<&'a str>,
}

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
    pub fn as_label(self) -> &'static str { ... }   // for metric labels (§7.4)
    pub fn level(self, on_error: OnError) -> tracing::Level { ... }  // §7.2 policy
}
```

The fields struct is `pub(crate)`. The `as_label` and `level` helpers encode the §7.2 level table:

| Outcome | OnError::Skip | OnError::Fail |
|---|---|---|
| Success | DEBUG | DEBUG |
| Skipped | WARN | (n/a; on_error=fail propagates → Failed) |
| Failed | (n/a; on_error=skip becomes Skipped) | ERROR |
| TimedOut | WARN | ERROR |
| Aborted | INFO | INFO |
| ProtectedFieldMutated | ERROR | ERROR |

The `emit()` method on `PluginLogFields` is the only path through which logs leave `invoke()`. There are no raw `tracing::info!` calls inside `invoke()`.

### 2. `invoke()` extension (`crates/plugins/src/invoke.rs`)

Signature changes minimally — adds `plugin_kind: &'static str` because metric label demands it (Q6). H5 callers (wrap_stream) pass `emit_span: false`; H2/H3/H7 callers pass `emit_span: true` (Q5).

```rust
pub(crate) async fn invoke<T, Fut>(
    plugin_name: &str,
    plugin_kind: &'static str,    // ← NEW
    ctx: &PluginContext,
    hook: &'static str,
    timeout_ms: u64,
    on_error: OnError,
    emit_span: bool,              // ← NEW; false for H5, true otherwise
    work: Fut,
) -> InvokeOutcome<T>
```

Inside `invoke()`:

1. If `emit_span`, enter `tracing::info_span!("plugin.invoke", plugin.name, plugin.kind, plugin.hook, plugin.outcome = tracing::field::Empty, plugin.elapsed_ms = tracing::field::Empty)`. The two `Empty` fields are recorded on the span before exit.
2. Time the work via `Instant::now()`.
3. Build a `PluginLogFields` based on the outcome.
4. Call `fields.emit()` to log + record on span.
5. Call `record_plugin_invocation(...)` (observability crate helper) to update counter + histogram.

The internal logic for outcome mapping is unchanged from P04: aborted always propagates, timeout/failed/protected mapped through on_error, success returns value.

### 3. `wrap_stream()` H5 span aggregation (`crates/plugins/src/registry.rs`)

Today `wrap_stream()` runs per-event invokes inside a `then().flat_map()` pipeline. P05 wraps that pipeline in a single `plugin.stream` span entered before the first event and exited when the stream is dropped.

```rust
pub fn wrap_stream(
    &self,
    route: (FrontendKind, &str),
    ctx: PluginContext,
    upstream: CanonicalStream,
) -> CanonicalStream {
    let plan = match self.lookup(...) { ... };  // fast path unchanged

    // ── P05: open plugin.stream span for the whole stream lifetime ──
    let span = tracing::info_span!(
        "plugin.stream",
        plugin.event_count = tracing::field::Empty,
        plugin.failure_count = tracing::field::Empty,
        // No plugin.name — the span covers ALL H5 plugins on this route.
        // Per-plugin attribution is in the per-event metrics.
    );

    // Wrap the existing stream pipeline in `.instrument(span)` so every
    // poll keeps the span entered. Per-event `invoke()` is called with
    // emit_span=false so it does not stack another span.
    ...
}
```

Per-event log lines are still emitted (with the same `PluginLogFields`), and the §7.5 success-path noise control still applies (Q5): success outcome's `PluginLogFields::emit()` will skip the log line but still update metrics. This is done by the `emit()` method itself — it checks outcome ∈ {Success, Skipped(success path)} + hook == "on_stream_event" and short-circuits log emission.

### 4. `PluginEntry.kind` field (`crates/plugins/src/registry.rs`)

```rust
pub struct PluginEntry {
    pub name: String,
    pub kind: &'static str,           // ← NEW, cached from plugin.kind_name()
    pub plugin: Arc<dyn Plugin>,
    pub on_error: OnError,
    pub timeouts: HookTimeouts,
    pub enabled: bool,
}
```

Construction sites (`for_testing_single_plugin` + future P06 `from_specs`) populate it once from `plugin.kind_name()`.

### 5. `PluginSupervisor` (`crates/plugins/src/supervisor.rs`, new file)

```rust
pub struct PluginSupervisor {
    // std::sync::Mutex is sufficient: spawn_h7 is called from sync code
    // (run_on_response_complete returns synchronously) and the critical
    // section is just a JoinSet::spawn call (a few ns). The async path
    // (flush_pending_h7) is the only one that takes the lock for longer,
    // but during flush the gateway is shutting down — no concurrent spawns.
    tasks: std::sync::Mutex<tokio::task::JoinSet<HandleResult>>,
    names: std::sync::Mutex<HashMap<tokio::task::Id, String>>,
}

/// Per-task outcome captured by spawn_h7's wrapper so flush can attribute
/// drops to plugin_name.
struct HandleResult {
    plugin_name: String,
}

impl PluginSupervisor {
    pub fn new() -> Self { ... }

    /// Spawn an H7 future. The wrapper records the result so flush can
    /// attribute drops by plugin_name. Returns synchronously.
    pub fn spawn_h7(
        &self,
        plugin_name: String,
        fut: impl Future<Output = ()> + Send + 'static,
    ) {
        // std::sync::Mutex: lock held for nanoseconds (JoinSet::spawn).
        // Sync function — safe to call from non-async context if needed.
        let mut tasks = self.tasks.lock().unwrap();
        let handle = tasks.spawn(async move {
            fut.await;
            HandleResult { plugin_name: plugin_name.clone() }
        });
        self.names.lock().unwrap().insert(handle.id(), plugin_name);
    }

    /// Wait for pending H7 tasks until `deadline` elapses. Tasks that don't
    /// complete are aborted; their plugin_name is reported via the H7
    /// dropped counter. Returns the count of dropped tasks.
    pub async fn flush_pending_h7(&self, deadline: Duration) -> Vec<String> {
        // Synchronously take the JoinSet out of the Mutex — we own it for
        // the duration of flush. spawn_h7 calls after this point will
        // panic on lock acquisition (poison) but shutdown is monotonic so
        // this is acceptable.
        let mut tasks = std::mem::take(&mut *self.tasks.lock().unwrap());

        // Drain by polling until either all tasks finish or deadline elapses.
        let drain = async {
            let mut completed_ids = Vec::new();
            while let Some(res) = tasks.join_next_with_id().await {
                if let Ok((id, _)) = res {
                    completed_ids.push(id);
                }
            }
            completed_ids
        };

        match tokio::time::timeout(deadline, drain).await {
            Ok(_completed) => Vec::new(),
            Err(_) => {
                // Deadline elapsed; tasks left over get aborted.
                let mut names = self.names.lock().unwrap();
                let dropped: Vec<String> = tasks
                    .join_next_with_id()  // would block, but we just need IDs;
                    .now_or_never()        // implementation must enumerate
                    .into_iter()
                    .filter_map(|res| match res {
                        Some(Ok((id, _))) => names.remove(&id),
                        Some(Err(je)) => names.remove(&je.id()),
                        None => None,
                    })
                    .collect();
                tasks.shutdown().await;
                dropped
            }
        }
    }
}
```

**Note on the drop attribution:** the public API returns the plugin_names of tasks that didn't complete in time so the shutdown hook can call `record_h7_dropped(name)` per name. The `names: Mutex<HashMap<task::Id, String>>` side-table is the implementation; cleared on successful join, looked up on timeout abort.

The above pseudocode is approximate; the implementer should refine the timeout/abort dance to actually iterate the remaining handle ids before `shutdown().await`. Two acceptable alternatives:

1. `tokio_util::task::TaskTracker` instead of `JoinSet` — cleaner shutdown semantics, but adds a workspace dep (we already have `tokio-util` for `CancellationToken` though, so this might be free).
2. Manual `Vec<JoinHandle<HandleResult>>` + `select!` loop — most explicit, most code.

The implementer picks based on what compiles cleanly with our `tokio` version. The contract is: `flush_pending_h7(Duration) -> Vec<String>` returning the dropped plugin_names.

### 6. Registry wiring (`crates/plugins/src/registry.rs`)

```rust
pub struct PluginRegistry {
    plugins: HashMap<String, Arc<PluginEntry>>,
    plans: HashMap<FrontendKind, FrontendRoutePlans>,
    supervisor: Arc<PluginSupervisor>,    // ← NEW (P05)
}

impl PluginRegistry {
    pub fn empty() -> Self {
        Self {
            plugins: HashMap::new(),
            plans: HashMap::new(),
            supervisor: Arc::new(PluginSupervisor::new()),
        }
    }

    pub async fn flush_pending_h7(&self, deadline: Duration) -> Vec<String> {
        self.supervisor.flush_pending_h7(deadline).await
    }
}
```

P04's `run_on_response_complete` keeps its outer shape but uses the supervisor:

```rust
pub fn run_on_response_complete(
    &self,
    route: (FrontendKind, &str),
    ctx: PluginContext,
    summary: ResponseSummary,
) {
    let Some(plan) = self.lookup(route.0, route.1) else { return; };
    if plan.on_response_complete.is_empty() { return; }
    // ... (rest unchanged) ...
    for entry in &plan.on_response_complete {
        if !entry.enabled { continue; }
        // ...
        self.supervisor.spawn_h7(plugin_name_for_supervisor, async move {
            let _ = invoke::invoke::<(), _>(
                &plugin_name, plugin_kind, &ctx,
                Hook::ResponseComplete.as_str(),
                timeout_ms,
                on_error,
                /* emit_span */ true,
                plugin.on_response_complete(&ctx, &summary),
            ).await;
        });
    }
}
```

### 7. Observability crate additions (`crates/observability/src/metrics/`)

- `names.rs`: add three `pub const`:
  - `PLUGIN_INVOCATIONS_TOTAL = "agent_shim_plugin_invocations_total"`
  - `PLUGIN_DURATION_SECONDS = "agent_shim_plugin_duration_seconds"`
  - `PLUGIN_H7_DROPPED_TOTAL = "agent_shim_plugin_h7_dropped_at_shutdown_total"`
- `catalog.rs`: three descriptors with HELP/TYPE
- `recorders.rs`:
  ```rust
  pub fn record_plugin_invocation(
      plugin_kind: &'static str,
      plugin_name: &str,
      hook: &'static str,
      outcome: &'static str,
      duration_secs: f64,
  ) { /* counter + histogram */ }

  pub fn record_h7_dropped(plugin_name: &str) {
      metrics::counter!(names::PLUGIN_H7_DROPPED_TOTAL,
          "plugin_name" => plugin_name.to_string()).increment(1);
  }
  ```

### 8. YAML config: `plugins.shutdown_flush_secs` (`crates/config/src/plugins.rs`)

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    #[serde(default)]
    pub instances: BTreeMap<String, PluginEntry>,
    /// Seconds to wait for H7 (on_response_complete) tasks to complete
    /// during gateway shutdown before dropping them. Default 5.
    #[serde(default = "default_shutdown_flush_secs")]
    pub shutdown_flush_secs: u64,
}

fn default_shutdown_flush_secs() -> u64 { 5 }
```

Validation: `0 <= shutdown_flush_secs <= 300` (Layer A — reject any >5min as a likely misconfig).

The exact path inside the existing `plugins:` block depends on P03's actual layout — if `plugins:` is currently a map directly (not a struct), this adds a small refactor:

```yaml
# Before P03 (instances at top of plugins block):
plugins:
  my_compressor:
    type: prompt_compressor
    ...

# After P05 (need to nest instances under `instances:`):
plugins:
  shutdown_flush_secs: 5      # NEW
  instances:                   # existing entries move under here
    my_compressor:
      ...
```

**Decision:** Use the nested form to keep `plugins.*` an extensible namespace. Migration: the old flat form is detected via Serde untagged enum and `tracing::warn!`-logged as deprecated; we silently rewrite to the new shape at parse time. If this turns out to be too painful, we drop the warning and break — P03 was just shipped, no third party depends on the YAML yet.

(**Implementer note:** check P03's actual struct first. If `PluginsBlock` is already a struct with an `instances` field, just add `shutdown_flush_secs` and skip the deprecation path.)

### 9. Gateway shutdown wiring (`crates/gateway/src/commands/serve.rs` or `shutdown.rs`)

Where `commands::serve::run` awaits `shutdown_signal` today, after it resolves but before returning:

```rust
// P05: flush H7 plugin tasks before exit.
let flush_secs = state.snapshot.load().config.plugins.shutdown_flush_secs;
let deadline = Duration::from_secs(flush_secs);
let dropped = state.core.plugins.flush_pending_h7(deadline).await;
for plugin_name in dropped {
    agent_shim_observability::metrics::recorders::record_h7_dropped(&plugin_name);
    tracing::warn!(
        plugin.name = %plugin_name,
        deadline_secs = flush_secs,
        "H7 task dropped at shutdown",
    );
}
```

## Testing strategy

### Plugins crate unit tests (DebuggingRecorder)

`crates/plugins/src/invoke.rs::tests` adds 6 metric-assertion tests, one per outcome:

```rust
fn assert_invocation_recorded(
    snapshot: &Snapshot,
    plugin_kind: &'static str,
    plugin_name: &str,
    hook: &'static str,
    outcome: &'static str,
) { ... }

#[tokio::test]
async fn success_emits_counter_and_histogram_with_outcome_success() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, || {
        // ... runs invoke() with a stub plugin returning Ok(()) ...
    });
    let snapshot = snapshotter.snapshot();
    assert_invocation_recorded(&snapshot, "test_kind", "test_name", "on_decoded_request", "success");
}
// ... 5 more analogous: skipped, failed, timed_out, aborted, protected_field_mutated
```

`crates/plugins/src/log_fields.rs::tests`: assert level mapping for every outcome × on_error combination (12 cases).

`crates/plugins/src/supervisor.rs::tests`:
- `spawn_then_flush_completes_within_deadline` — spawn 3 fast tasks, flush(1s), assert dropped is empty
- `spawn_then_flush_drops_slow_tasks` — spawn 1 slow task (sleep 1s), flush(10ms), assert dropped == [task_name]
- `spawn_concurrent_then_flush_handles_all` — 100 tasks, flush, assert no panic

### Gateway integration test (`crates/gateway/tests/plugins_observability.rs`, new)

3 tests using the existing `vision_capability_mismatch.rs` pattern (hand-built AppState):

1. `request_with_h2_plugin_increments_invocations_counter` — drive a request, render Prometheus, grep for `agent_shim_plugin_invocations_total{... outcome="success"} 1`
2. `slow_h7_plugin_dropped_at_shutdown_increments_dropped_counter` — start request with a slow H7, fire shutdown via dropping the runtime, then a separate flush call, assert `agent_shim_plugin_h7_dropped_at_shutdown_total` registers
3. `stream_hook_success_does_not_log` — capture log output via `tracing_subscriber::fmt::TestWriter`, drive a streaming request with H5 plugin success path, assert log output does NOT contain `plugin.outcome="success"` (no per-event noise)

## Data flow detail: log + metric per outcome

| Outcome | Log level | Log emitted? | Counter label | Histogram updated |
|---|---|---|---|---|
| Success (H2/H3/H7) | DEBUG | yes | `outcome="success"` | yes |
| Success (H5) | DEBUG | **no** (noise control) | `outcome="success"` | yes |
| Skipped | WARN | yes | `outcome="skipped"` | yes |
| Failed | ERROR | yes | `outcome="failed"` | yes |
| TimedOut (skip) | WARN | yes | `outcome="timed_out"` | yes |
| TimedOut (fail) | ERROR | yes | `outcome="timed_out"` | yes |
| Aborted | INFO | yes | `outcome="aborted"` | yes |
| ProtectedFieldMutated | ERROR | yes | `outcome="protected_field_mutated"` | yes |

Histogram is updated unconditionally because operators want timing distributions even on failure (slow failures are a different story than fast ones).

## Error handling

- **Failure of metric recording:** `metrics-rs` is infallible — `counter!()` and `histogram!()` are macros that return `()`. There's no error path. If no recorder is installed (test scenarios), the call is a no-op. Safe.
- **Failure of log emission:** `tracing::event!` is also infallible. If no subscriber, it's discarded. Safe.
- **Failure during `flush_pending_h7`:** if the JoinSet itself panics during shutdown, the `tokio::time::timeout` resolves to `Err`, we report `tasks.len()` as dropped, and the supervisor proceeds. Gateway shutdown continues regardless.
- **Failure during `spawn_h7`:** `tokio::task::JoinSet::spawn` is infallible (it just stores a handle). Lock acquisition is via `blocking_lock`; if the current task is not in a Tokio runtime, this panics — but `spawn_h7` is always called from inside `run_on_response_complete` which is always inside a Tokio context. Safe.

## Acceptance criteria

1. `invoke()` signature accepts `plugin_kind: &'static str` + `emit_span: bool`.
2. `PluginLogFields` struct exists; only it can `emit()` to the tracing subsystem from inside `invoke()`. Search-and-confirm: no other `tracing::info!`/`debug!`/`error!`/`warn!` inside `invoke.rs` proper.
3. Every outcome variant maps to the level table above (asserted by 6 unit tests).
4. `PluginEntry.kind: &'static str` exists and is populated at construction.
5. `PluginSupervisor` struct exists with `spawn_h7` + `flush_pending_h7(deadline) -> Vec<String>` API.
6. `PluginRegistry.supervisor: Arc<PluginSupervisor>` exists; `run_on_response_complete` routes through it (no bare `tokio::spawn`).
7. `plugins.shutdown_flush_secs: u64` (default 5, max 300) lands in config schema with Layer A validation.
8. Gateway shutdown calls `state.core.plugins.flush_pending_h7(deadline).await` and emits `agent_shim_plugin_h7_dropped_at_shutdown_total{plugin_name}` + a WARN log per dropped task.
9. `wrap_stream` enters `plugin.stream` span; inner `invoke()` calls use `emit_span: false`. Per-event `PluginLogFields::emit()` skips the log on success/H5 hook combo.
10. Three metrics added to observability crate's `names` + `catalog` + `recorders`.
11. `cargo nextest run --workspace`: 789 → ~800+ (depending on exact test count).
12. `cargo clippy --workspace --all-targets -- -D warnings`: clean.
13. `cargo fmt --all -- --check`: clean.
14. **Frozen-core invariant preserved.** `git diff master..HEAD -- crates/core/` empty.

## Risks + mitigations

| Risk | Mitigation |
|---|---|
| `JoinSet` Mutex contention on H7-heavy routes | `std::sync::Mutex` lock is held nanoseconds (push handle into JoinSet, drop lock). Per-request overhead negligible compared to actual H7 work. If proven contentious in benchmarks (P07), switch to `RwLock<JoinSet>` or use `tokio_util::task::TaskTracker` |
| `std::sync::Mutex::lock` blocking inside async context | Lock is held nanoseconds for the JoinSet::spawn case, microseconds for the flush case. Cooperative scheduling cost is below noise floor — the same pattern is used throughout the gateway (e.g. `arc_swap` is built on `std::sync::Mutex`). Confirmed acceptable by `clippy::await_holding_lock` lint (we never hold across await) |
| `metrics-util` version skew with `metrics` crate | both pinned via workspace deps; `metrics-util` 0.x is the official sibling crate, version is locked to `metrics` major |
| PII leak via `PluginError::Failed::message` | Mitigated by rustdoc warning + changelog note. Cannot be prevented by type system without disallowing useful debug messages. Code review expected. |
| `plugin.stream` span lifetime exceeds request lifetime | `wrap_stream` returns a `BoxStream` and the span is owned by the wrapped future. When the stream is dropped (client disconnect or completion), the future is dropped and the span exits. Verified by P05 supervisor test reproducing client disconnect |
| H7 task dropped at shutdown without `plugin_name` attribution | Side-table of `JoinHandle::id() → plugin_name` populated in `spawn_h7`; on flush timeout, abort surviving handles and look up names via the side-table. Implementation detail in `supervisor.rs` |

## YAGNI watch

- **No per-hook metric** beyond `hook` label. Operators can `sum by (hook) (...)` in PromQL.
- **No `plugin.outcome` enum exported**. The label string is the public surface; the enum is `pub(crate)`.
- **No "verbose H5 logging" config knob.** `RUST_LOG=agent_shim_plugins::stream=trace` (mentioned in §7.5) is documented but not implemented as a config field — the env var route is sufficient.
- **No async `metrics::Registry`-replacement abstraction.** We use the global `metrics-rs` facade everywhere, including in plugins. Adding a sink trait (Q2 option 3) is over-engineered for one callsite.
- **No span sampling** for plugin spans. OTel sampling at the request level is sufficient; child spans inherit.
