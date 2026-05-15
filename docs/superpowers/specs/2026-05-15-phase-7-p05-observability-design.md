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
        │  - timeout       │         │  - tasks: std::Mutex<JS> │
        │  - on_error      │         │  - pending: HashMap<     │
        │  - protected diff│         │    String, u64>          │
        │  - InvokeArgs◄─NEW│         │  - spawn_h7(name, fut)   │
        │  - SpanMode  ◄─NEW│         │  - flush_pending_h7(dur) │
        │  - LogFields ◄─NEW│         │    → Vec<(String, u64)>  │
        │  - metric record◄─NEW                                  │
        └────────┬─────────┘         └──────────────────────────┘
                 ▼                                 ▲
        ┌──────────────────────────┐               │ flush from
        │ observability crate      │               │ run_core
        │  metrics::recorders::    │               │
        │   record_plugin_invocation(...)          │
        │   record_h7_dropped(name, count)         │
        │   names::PLUGIN_*                        │
        │   catalog::iter_descriptors              │
        └──────────────────────────┘               │
                                                   │
                              ┌────────────────────┴────────────────┐
                              │ gateway::commands::serve::run_core  │
                              │   1. axum serve.await (drain HTTP)  │
                              │   2. plugins.flush_pending_h7(dur)──┘
                              │   3. otel.shutdown()                │
                              └─────────────────────────────────────┘
```

## Field naming convention

Tracing and Prometheus have different identifier rules. The two surfaces use parallel, non-interchangeable names for the same conceptual field:

| Concept | Tracing field (dotted, OTel) | Metric label (snake, Prometheus) |
|---|---|---|
| Plugin instance name | `"plugin.name"` | `plugin_name` |
| Plugin kind | `"plugin.kind"` | `plugin_kind` |
| Hook | `"plugin.hook"` | `hook` |
| Request ID | `"agent_shim.request_id"` | (n/a — high cardinality) |
| Route | `"agent_shim.route"` | (n/a — operator-controlled) |
| Outcome | `"plugin.outcome"` | `outcome` |
| Elapsed ms | `"plugin.elapsed_ms"` | (n/a — histogram value) |
| On-error policy | `"plugin.on_error_policy"` | (n/a — policy attribute) |
| Error message | `"plugin.error"` | (n/a — text) |

Dotted names go into tracing field syntax via `"plugin.name" = value` (string-keyed). Bare snake names go into `metrics::counter!("..." => value)` directly. The two are never substituted — implementer should not paste a dotted name into a metric label (rejected by Prometheus) or a snake name into an OTel attribute (violates semantic convention).

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
    pub request_id: &'a agent_shim_core::RequestId,  // emit as `%self.request_id` (Display)
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

`PluginLogFields::emit()` is the only path through which logs leave `invoke()`. There are no raw `tracing::info!` / `debug!` / `error!` / `warn!` calls inside `invoke.rs` proper.

**Implementation: `emit()` uses an internal macro to avoid duplicating the field list across the 4 tracing-level macros (Q3 decision):**

```rust
// Inside log_fields.rs, file-local:
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
            tracing::Level::WARN  => tracing::warn!(/* SAME field set */),
            tracing::Level::INFO  => tracing::info!(/* SAME field set */),
            tracing::Level::DEBUG => tracing::debug!(/* SAME field set */),
            _ => {},
        }
    };
}

impl PluginLogFields<'_> {
    fn emit(&self) {
        // §7.5 noise control: H5 success skips log emission (metric still updates).
        if self.outcome == PluginOutcome::Success && self.plugin_hook == "on_stream_event" {
            return;
        }
        emit_at_level!(self.outcome.level(self.on_error_policy), self);
    }
}
```

Rust's `tracing::event!()` macro requires the `Level` argument to be a compile-time path expression, not a runtime variable. The 4-way `match` is unavoidable; the file-local macro keeps the field-list duplication to a single place. Adding a new tracing field → edit the macro body in one location.

### 2. `invoke()` extension (`crates/plugins/src/invoke.rs`)

The 8-positional-args version of `invoke()` is refactored to take an `InvokeArgs` struct + `ctx` + `work` (3 args total). The struct also has a `from_entry` builder for the common case (Q7 decision).

```rust
pub(crate) struct InvokeArgs<'a> {
    pub plugin_name: &'a str,
    pub plugin_kind: &'static str,
    pub hook: &'static str,
    pub timeout_ms: u64,
    pub on_error: OnError,
    pub span_mode: SpanMode,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SpanMode {
    /// H2/H3/H7: open a per-invocation `plugin.invoke` span.
    PerInvocation,
    /// H5: do NOT open a new span; events are attached to the
    /// outer `plugin.stream` span owned by `wrap_stream`.
    Aggregated,
}

impl InvokeArgs<'_> {
    pub fn from_entry<'a>(
        entry: &'a PluginEntry,
        hook: Hook,
        span_mode: SpanMode,
    ) -> InvokeArgs<'a> {
        InvokeArgs {
            plugin_name: &entry.name,
            plugin_kind: entry.kind,    // cached &'static str (Component 4)
            hook: hook.as_str(),
            timeout_ms: entry.timeouts.for_hook(hook),
            on_error: entry.on_error,
            span_mode,
        }
    }
}

pub(crate) async fn invoke<T, Fut>(
    args: InvokeArgs<'_>,
    ctx: &PluginContext,
    work: Fut,
) -> InvokeOutcome<T>
where
    Fut: Future<Output = PluginResult<T>>,
{ ... }
```

**P04 callsite changes:** all four existing callsites in `registry.rs` (`run_on_decoded_request`, `run_on_resolved`, `wrap_stream`'s `then` closure, `run_on_response_complete`) are refactored to:

```rust
let outcome = invoke::invoke(
    InvokeArgs::from_entry(entry, Hook::DecodedRequest, SpanMode::PerInvocation),
    ctx,
    plugin.on_decoded_request(ctx, candidate),
).await;
```

H5 callsite passes `SpanMode::Aggregated`; others pass `SpanMode::PerInvocation`.

**Span lifecycle inside `invoke()` (Q16 decision):** the function uses `Span::none()` for the `Aggregated` case and always calls `.instrument(span.clone())` on the work future. This keeps the timeout logic a single code path without if-else duplication:

```rust
use tracing::Instrument;

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
).await;
let elapsed_ms = started.elapsed().as_millis() as u64;

// outcome matching (unchanged from P04) ...

// Record span fields. No-op when span is Span::none() (Aggregated).
span.record("plugin.outcome", outcome.as_label());
span.record("plugin.elapsed_ms", elapsed_ms);

// LogFields emit (mode-agnostic; H5 success skips log per §7.5).
let fields = PluginLogFields {
    plugin_name: args.plugin_name,
    plugin_kind: args.plugin_kind,
    plugin_hook: args.hook,
    request_id: &ctx.request_id,
    route: &ctx.route_label,
    outcome,
    elapsed_ms,
    on_error_policy: args.on_error,
    error: error_str.as_deref(),
};
fields.emit();

// Metric recording (mode-agnostic).
record_plugin_invocation(
    args.plugin_kind,
    args.plugin_name,
    args.hook,
    outcome.as_label(),
    elapsed_ms as f64 / 1000.0,
);
```

`Span::clone()` is cheap (atomic refcount on `Arc<Inner>`). `work.instrument(span.clone())` moves the clone into the future; the original `span` survives for `record()` calls after the await.

### 3. `wrap_stream()` H5 span aggregation (`crates/plugins/src/registry.rs`)

P04's `wrap_stream` runs per-event invokes inside a `then().flat_map()` pipeline. P05 wraps that pipeline in a `GuardedH5Stream` which (a) holds a `plugin.stream` span entered on every `poll_next` (Q17), and (b) carries a `StreamSpanRecorder` drop guard that writes final `event_count` / `failure_count` to the span at stream end (Q4).

```rust
// crates/plugins/src/registry.rs (private types)

/// Drop-time recorder that writes aggregated counters to `plugin.stream` span.
struct StreamSpanRecorder {
    span: tracing::Span,
    event_count: Arc<AtomicU64>,
    failure_count: Arc<AtomicU64>,
}
impl Drop for StreamSpanRecorder {
    fn drop(&mut self) {
        self.span.record("plugin.event_count", self.event_count.load(Ordering::Relaxed));
        self.span.record("plugin.failure_count", self.failure_count.load(Ordering::Relaxed));
    }
}

/// Stream wrapper that enters `plugin.stream` for each poll and owns the recorder.
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
        let _enter = self.span.enter();  // RAII; `Entered<'_>` is !Send but stack-local within poll body
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}
```

`wrap_stream` body:

```rust
pub fn wrap_stream(
    &self,
    route: (FrontendKind, &str),
    ctx: PluginContext,
    upstream: CanonicalStream,
) -> CanonicalStream {
    let plan = match self.lookup(...) { /* fast path returns upstream unchanged */ };

    // Open the aggregated H5 span. NO plugin.name field — the span covers
    // ALL H5 plugins on this route. Per-plugin attribution lives in the
    // per-event LogFields (request_id correlation; Q14 trade-off).
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

    let plugins: Vec<Arc<PluginEntry>> = plan.on_stream_event.clone();
    let ctx = Arc::new(ctx);

    let inner_stream = upstream.then(move |event_result| {
        let plugins = plugins.clone();
        let ctx = ctx.clone();
        let event_count = Arc::clone(&event_count);
        let failure_count = Arc::clone(&failure_count);
        async move {
            event_count.fetch_add(1, Ordering::Relaxed);
            // existing P04 logic for chained invoke() calls per event,
            // with SpanMode::Aggregated and failure_count.fetch_add on errors
        }
    }).flat_map(futures::stream::iter);

    Box::pin(GuardedH5Stream { inner: inner_stream, span, _recorder: recorder })
}
```

**Per-event invoke() call inside the closure** passes `SpanMode::Aggregated`:

```rust
let outcome = invoke::invoke(
    InvokeArgs::from_entry(entry, Hook::StreamEvent, SpanMode::Aggregated),
    &ctx,
    plugin.on_stream_event(&ctx, ev_for_invoke),
).await;
```

`PluginLogFields::emit()` inside invoke() checks `(outcome == Success && hook == on_stream_event)` → skip log (§7.5 noise control). Failure outcomes still log normally, and the event attaches to the `plugin.stream` span via `Span::current()`.

**H5 failure attribution trade-off (Q14):** When a H5 plugin fails on event N, the trace shows only one `plugin.stream` span with `failure_count > 0`; no per-plugin child spans. Per-plugin attribution lives in the failure log line: `outcome="failed"` + `plugin.name="..."` + `agent_shim.request_id="..."`. Operators correlate failures to specific plugins via log search rather than the trace tree. This is consistent with §7.5 noise control — span granularity matches log granularity.

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

**Constraint (Q1):** `Plugin::kind_name()` must return a string literal (`&'static str` referring to constant data), not a leaked allocation. This is enforced by rustdoc convention — the trait method's documentation MUST explicitly state "return a `&'static str` that is a string literal; do not return `Box::leak`-derived references; new kinds at runtime are not supported." Acceptance criterion checks for this rustdoc note on the trait definition.

### 5. `PluginSupervisor` (`crates/plugins/src/supervisor.rs`, new file)

```rust
pub struct PluginSupervisor {
    /// std::sync::Mutex (NOT tokio::sync): the critical section is only
    /// `JoinSet::spawn` (ns) or `std::mem::take` (ns) — never held across
    /// `.await`. Confirmed clean by `clippy::await_holding_lock`.
    tasks: std::sync::Mutex<tokio::task::JoinSet<()>>,
    /// plugin_name → pending count. spawn_h7 increments; the task body
    /// (clone of this Arc) decrements on completion. On flush timeout
    /// the remaining (name, count) pairs are the dropped attribution.
    pending: Arc<std::sync::Mutex<HashMap<String, u64>>>,
}

impl PluginSupervisor {
    pub fn new() -> Self {
        Self {
            tasks: std::sync::Mutex::new(JoinSet::new()),
            pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Spawn an H7 future. Returns synchronously. Records plugin_name in
    /// the pending map; the task body decrements on completion.
    pub fn spawn_h7(
        &self,
        plugin_name: String,
        fut: impl Future<Output = ()> + Send + 'static,
    ) {
        // Increment pending count for this plugin name.
        *self.pending.lock().unwrap()
            .entry(plugin_name.clone()).or_insert(0) += 1;

        let pending = Arc::clone(&self.pending);
        self.tasks.lock().unwrap().spawn(async move {
            fut.await;
            // On task completion, decrement count. Remove zero entries.
            let mut p = pending.lock().unwrap();
            if let Some(cnt) = p.get_mut(&plugin_name) {
                *cnt -= 1;
                if *cnt == 0 { p.remove(&plugin_name); }
            }
        });
    }

    /// Wait for pending H7 tasks until `deadline` elapses. Returns
    /// `Vec<(plugin_name, dropped_count)>` for tasks that did not
    /// complete in time. Detaches survivors (they get aborted on
    /// JoinSet drop or process exit). The caller emits one
    /// `record_h7_dropped(name, count)` per entry.
    pub async fn flush_pending_h7(&self, deadline: Duration) -> Vec<(String, u64)> {
        // Take ownership of the JoinSet for the duration of flush. After
        // axum drain, no new H7 spawns happen — this take is safe.
        let mut tasks = std::mem::take(&mut *self.tasks.lock().unwrap());

        // Drain tasks until empty or deadline elapses.
        let _ = tokio::time::timeout(deadline, async {
            while tasks.join_next().await.is_some() {}
        }).await;

        // Whatever remains in `pending` did not finish in time.
        let p = self.pending.lock().unwrap();
        let dropped: Vec<(String, u64)> = p.iter()
            .map(|(name, count)| (name.clone(), *count))
            .collect();

        // tasks drops here -> aborts in-flight survivors.
        drop(tasks);
        dropped
    }
}
```

**Why std::sync::Mutex over tokio::sync::Mutex:** critical sections are short (ns-scale: JoinSet::spawn or HashMap entry update), held without `.await`. `clippy::await_holding_lock` linting confirms no violations.

**Why HashMap<String, u64> instead of HashMap<task::Id, String>:** allowing the same plugin to spawn multiple H7 tasks concurrently (high QPS) means using `String` as key with a count. The H7 dropped metric label only needs plugin_name (not task_id), so no need for task::Id ↔ name reverse lookup. Simpler.

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
    let summary = Arc::new(summary);
    let ctx = Arc::new(ctx);
    for entry in &plan.on_response_complete {
        if !entry.enabled { continue; }
        let plugin = entry.plugin.clone();
        let plugin_name = entry.name.clone();
        let plugin_kind = entry.kind;     // &'static str cached on entry
        let timeout_ms = entry.timeouts.for_hook(Hook::ResponseComplete);
        let on_error = entry.on_error;
        let summary = summary.clone();
        let ctx = ctx.clone();
        self.supervisor.spawn_h7(plugin_name.clone(), async move {
            let args = InvokeArgs {
                plugin_name: &plugin_name,
                plugin_kind,
                hook: Hook::ResponseComplete.as_str(),
                timeout_ms,
                on_error,
                span_mode: SpanMode::PerInvocation,  // H7 gets its own plugin.invoke span
            };
            let _ = invoke::invoke::<(), _>(
                args,
                &ctx,
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
  ) {
      metrics::counter!(
          names::PLUGIN_INVOCATIONS_TOTAL,
          "plugin_kind" => plugin_kind,
          "plugin_name" => plugin_name.to_string(),
          "hook" => hook,
          "outcome" => outcome,
      ).increment(1);
      metrics::histogram!(
          names::PLUGIN_DURATION_SECONDS,
          "plugin_kind" => plugin_kind,
          "plugin_name" => plugin_name.to_string(),
          "hook" => hook,
      ).record(duration_secs);
  }

  pub fn record_h7_dropped(plugin_name: &str, count: u64) {
      metrics::counter!(names::PLUGIN_H7_DROPPED_TOTAL,
          "plugin_name" => plugin_name.to_string()).increment(count);
  }
  ```

**Histogram bucket defaults (Q10, B+):** `agent_shim_plugin_duration_seconds` gets a hardcoded default bucket set covering sub-millisecond to seconds. Default exporter buckets (5ms-10s) are unsuitable for H5 which fires per SSE event with sub-ms latency.

In `crates/observability/src/metrics/mod.rs::install`, BEFORE iterating `cfg.histogram_buckets` (so operator override still wins), apply:

```rust
const PLUGIN_DURATION_DEFAULT_BUCKETS: &[f64] =
    &[0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.5, 1.0, 5.0];

builder = builder
    .set_buckets_for_metric(
        Matcher::Full(names::PLUGIN_DURATION_SECONDS.to_string()),
        PLUGIN_DURATION_DEFAULT_BUCKETS,
    )
    .expect("plugin duration default buckets must be valid");

// ... existing histogram_buckets cfg loop here, which may override the above ...
```

Operator can override via `metrics.histogram_buckets: { agent_shim_plugin_duration_seconds: [...] }` in `gateway.yaml`.

### 8. YAML config: `shutdown.plugin_flush_secs` (`crates/config/src/schema.rs`)

P03's `plugins: BTreeMap<String, PluginEntry>` field is a flat map of named instances. Adding `shutdown_flush_secs` there would require restructuring (breaking change). Instead (Q8), introduce a new top-level `shutdown:` block — the natural home for cross-cutting shutdown timing knobs:

```rust
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ShutdownConfig {
    /// Seconds to wait for H7 (on_response_complete) plugin tasks to
    /// complete during gateway shutdown before dropping them. Default 5.
    #[serde(default = "default_plugin_flush_secs")]
    pub plugin_flush_secs: u64,
}

fn default_plugin_flush_secs() -> u64 { 5 }

// In GatewayConfig:
pub struct GatewayConfig {
    // ... existing fields ...
    #[serde(default)]
    pub shutdown: ShutdownConfig,
}
```

YAML:

```yaml
shutdown:
  plugin_flush_secs: 5    # default 5, Layer A validates 0..=300
```

**Validation:** Layer A (`crates/config/src/validation.rs`) rejects `plugin_flush_secs > 300` as a likely misconfig.

**Default-on-absence:** Existing `gateway.yaml` files without a `shutdown:` block continue to work (`#[serde(default)]` on `GatewayConfig.shutdown`); plug_flush_secs defaults to 5.

### 9. Gateway shutdown wiring (`crates/gateway/src/commands/serve.rs::run_core`)

`run_core` calls axum's `run_on_listener` / `run_with_admin_on_listeners` which await graceful shutdown internally (axum returns once in-flight requests drain or its timeout expires). After axum returns, the runtime is still alive — H7 tasks spawned during the drain are still pending in the supervisor's JoinSet.

P05 wedges flush between axum-await-completion and OTel shutdown:

```rust
// Before axum.await — clone the supervisor Arc and read the flush deadline.
let plugins = state.core.plugins.clone();  // Arc<PluginRegistry>
let flush_secs = state.snapshot.load().config.shutdown.plugin_flush_secs;

// axum runs and drains in-flight requests (existing P04 code path).
let result = if let Some(admin_listener) = admin_listener_opt {
    crate::server::run_with_admin_on_listeners(public_listener, admin_listener, state, shutdown_signal).await
} else {
    crate::server::run_on_listener(public_listener, state, shutdown_signal).await
};

// Plugin flush BEFORE otel.shutdown — flush emits warn! logs and
// record_h7_dropped() metric points that otel still needs to pick up.
let dropped = plugins.flush_pending_h7(Duration::from_secs(flush_secs)).await;
for (plugin_name, count) in dropped {
    agent_shim_observability::metrics::recorders::record_h7_dropped(&plugin_name, count);
    tracing::warn!(
        plugin.name = %plugin_name,
        dropped_count = count,
        deadline_secs = flush_secs,
        "H7 task dropped at shutdown",
    );
}

// OTel last so it can flush the H7-dropped warn! events.
if let Some(otel) = tracing_handles.otel {
    otel.shutdown();
}
result
```

**Ordering rationale (漏4 verify):**
1. SIGTERM received → `shutdown_signal` future resolves
2. axum stops accepting new requests, drains in-flight → during drain, H7 tasks get spawned into supervisor
3. axum returns → no new H7 spawns can happen
4. `flush_pending_h7(deadline)` — wait up to `flush_secs` for spawned H7 tasks; emit warn + metric for survivors
5. `otel.shutdown()` — flush remaining tracing events including step 4's warn lines
6. main returns → tokio runtime drops → any leftover background tasks aborted

## Testing strategy

### Plugins crate unit tests (DebuggingRecorder)

Plugins crate dev-dependency:

```toml
[dev-dependencies]
metrics-util = "0.17"
```

**Recorder installation pattern (Q15):** the test module installs `DebuggingRecorder` once via `OnceLock<Snapshotter>` (sibling to observability crate's `INSTALLED: OnceLock<PrometheusHandle>` pattern). cargo test runs each test crate as its own binary/process, so this is process-local — no interference with other crates' recorders.

```rust
use metrics_util::debugging::{DebuggingRecorder, Snapshotter};
use std::sync::OnceLock;

static SNAPSHOTTER: OnceLock<Snapshotter> = OnceLock::new();

fn snapshotter() -> &'static Snapshotter {
    SNAPSHOTTER.get_or_init(|| {
        let recorder = DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        metrics::set_global_recorder(recorder)
            .expect("DebuggingRecorder must install in plugin tests");
        snap
    })
}

fn count_invocations(snap: &Snapshot, plugin_name: &str, outcome: &str) -> u64 {
    snap.into_vec().iter()
        .filter_map(|(key, _unit, _desc, value)| {
            if key.key().name() != "agent_shim_plugin_invocations_total" { return None; }
            // ... label-set match ...
            // ... extract counter value ...
        })
        .sum()
}
```

**Tests added (6 outcome variants × 1 metric assertion each, in `invoke.rs::tests`):**

- `success_emits_counter_and_histogram_with_outcome_success`
- `skipped_emits_counter_with_outcome_skipped`
- `failed_emits_counter_with_outcome_failed`
- `timed_out_emits_counter_with_outcome_timed_out`
- `aborted_emits_counter_with_outcome_aborted`
- `protected_field_mutated_emits_counter_with_outcome_protected_field_mutated`

Pattern (snapshot diff to handle test parallelism + shared global recorder):

```rust
#[tokio::test]
async fn success_emits_counter_with_outcome_success() {
    let snap = snapshotter();
    let before = count_invocations(&snap.snapshot(), "test_success", "success");
    invoke::invoke(
        InvokeArgs { plugin_name: "test_success", plugin_kind: "test_kind", /* ... */ },
        &ctx,
        async { Ok::<_, PluginError>(()) },
    ).await;
    let after = count_invocations(&snap.snapshot(), "test_success", "success");
    assert_eq!(after - before, 1);
}
```

**LogFields tests in `log_fields.rs::tests`:** assert level mapping for every outcome × on_error combination (12 cases via `PluginOutcome::level(OnError)`).

**Supervisor tests in `supervisor.rs::tests`:**

- `spawn_then_flush_completes_within_deadline` — spawn 3 fast tasks, flush(1s) returns empty Vec
- `spawn_then_flush_drops_slow_tasks_returns_attribution` — spawn 1 slow task (sleep 1s), flush(10ms), assert dropped == `[("slow_plugin", 1)]`
- `spawn_concurrent_same_plugin_returns_aggregated_count` — spawn the same plugin name 5 times slow, flush(10ms), assert dropped == `[("slow_plugin", 5)]` (Q9 verify)
- `spawn_concurrent_then_flush_handles_all` — 100 fast tasks, flush(1s), assert no panic + empty Vec

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

1. `invoke()` is refactored to take `(InvokeArgs, &PluginContext, work)` — 3 args. `InvokeArgs::from_entry` builder exists.
2. `SpanMode` enum (`PerInvocation` / `Aggregated`) is `pub(crate)`; H5 callsite passes `Aggregated`; H2/H3/H7 pass `PerInvocation`.
3. `PluginLogFields` struct exists with the 9 fields in §1; only its `emit()` (via internal macro) emits tracing events from inside `invoke.rs`. Search-and-confirm: no other `tracing::info!`/`debug!`/`error!`/`warn!` inside `invoke.rs`.
4. `PluginLogFields.request_id: &'a RequestId` (NOT `&str`) — emitted as `%self.request_id` Display.
5. Every outcome variant maps to the level table above (asserted by 12 unit tests in `log_fields.rs::tests` covering outcome × on_error pairs).
6. `PluginEntry.kind: &'static str` exists and is populated at construction. `Plugin::kind_name()` rustdoc explicitly requires string-literal returns (no leaked allocations); acceptance check grep.
7. Tracing field names use dotted form (`"plugin.name"`, `"agent_shim.request_id"` etc.) with string-quoted syntax. Metric labels use snake_case (`plugin_name`, `plugin_kind`, `hook`, `outcome`). See § Field naming convention.
8. `PluginSupervisor` struct exists with `spawn_h7(plugin_name: String, fut)` (sync) + `flush_pending_h7(deadline) -> Vec<(String, u64)>` (async). Uses `std::sync::Mutex` (not `tokio::sync::Mutex`). `clippy::await_holding_lock` clean.
9. `PluginRegistry.supervisor: Arc<PluginSupervisor>` exists; `run_on_response_complete` routes through `supervisor.spawn_h7`. No bare `tokio::spawn` in `run_on_response_complete` post-P05.
10. `wrap_stream` opens `plugin.stream` span via `GuardedH5Stream`. `plugin.stream` records `event_count` + `failure_count` via `StreamSpanRecorder` drop guard. Per-event `invoke()` calls pass `SpanMode::Aggregated`. §7.5 noise control: H5 success skips the log line (verified by integration test).
11. `shutdown.plugin_flush_secs: u64` (default 5, Layer A max 300) lands in `GatewayConfig.shutdown`. Old configs without a `shutdown:` block load with default applied.
12. Gateway `commands::serve::run_core` calls `state.core.plugins.flush_pending_h7(deadline).await` AFTER axum await, BEFORE `otel.shutdown()`. Each dropped task emits one `record_h7_dropped(name, count)` + one WARN log.
13. Three new Prometheus metrics added to observability `names` + `catalog` + `recorders`. `agent_shim_plugin_duration_seconds` gets hardcoded default buckets `[0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.5, 1.0, 5.0]`, applied via `Matcher::Full` BEFORE iterating operator's `metrics.histogram_buckets` overrides.
14. `cargo nextest run --workspace`: workspace test count increases by ~22 (12 LogFields × outcome×on_error tests + 6 invoke outcome tests + 4 supervisor tests).
15. `cargo clippy --workspace --all-targets -- -D warnings`: clean.
16. `cargo fmt --all -- --check`: clean.
17. **Frozen-core invariant preserved.** `git diff master..HEAD -- crates/core/` empty.

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
