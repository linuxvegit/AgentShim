# Plugin System Design (Phase 7 candidate)

> **Status:** Draft design — approved through Section 4 of the brainstorming
> dialogue on 2026-05-14. Subject to user review before implementation
> planning.

## 1. Goal

Introduce a **statically-registered, in-process plugin system** that lets
operators run code at well-defined points in the request lifecycle to
**rewrite, filter, compress, or observe** the data flowing through the
gateway — without recompiling AgentShim per deployment, and without leaving
the existing single-binary distribution model.

The primary motivating use case is **compressing the prompt sent to upstream
LLMs** (e.g. trim Claude-Code-style verbose system prompts before they hit
DeepSeek), but the system is designed to cover any rewrite/observe scenario
along the request lifecycle.

## 2. Non-goals

- **Dynamic / hot-loadable third-party plugins.** Plugins are Rust crates
  compiled into the binary. Adding a new plugin type requires a new build.
  Third parties contribute by PR (mode "A1" — see §3.6).
- **WASM / dlopen / sidecar plugins.** Considered and rejected for v1; see
  §10 *Alternatives considered*.
- **A plugin marketplace or registry.** Out of scope.
- **Mutating provider-side wire formats.** Plugins operate on the *canonical*
  model (`CanonicalRequest`, `StreamEvent`, `CanonicalResponse`). The
  frontends and providers stay untouched.
- **Changing the v0.4 resilience layer or v0.6 cost filter.** The plugin
  system runs *around* them, not inside them.

## 3. Architecture overview

### 3.1 One-line description

A new crate `agent-shim-plugins` exposes a `Plugin` trait with four hook
methods; built-in plugin implementations live under
`crates/plugins/src/builtin/`; the gateway wires a `PluginRegistry` into
`AppState` and invokes it at four points inside `pipeline.rs::dispatch_inner`.

### 3.2 Hook points

The system exposes **four hooks**, picked from a longer candidate list during
brainstorming. Each maps to a precise position in the existing request flow:

| Hook | Position in `dispatch_inner` | Sees | Can write |
|---|---|---|---|
| `on_decoded_request` (H2) | After frontend decode, before route resolution | `CanonicalRequest` | yes — return owned new value |
| `on_resolved` (H3) | After route + policy merge, before capability gate | `CanonicalRequest` + chosen `BackendTarget` | yes — per-upstream personalisation |
| `on_stream_event` (H5) | Around each `StreamEvent` flowing upstream→client | one `StreamEvent` | yes — emit zero or more events (1-in-N-out) |
| `on_response_complete` (H7) | After streaming/unary completion | `ResponseSummary` (usage + elapsed + status) | no — observation only |

Rejected hook candidates (H1 raw bytes / H4 immediately-before-upstream /
H6 unary-only response) were excluded as redundant once H2+H3+H5+H7 are in
place. They remain available for future extension.

### 3.3 Crate boundary

```
gateway
├── plugins *                    ← new crate (`crates/plugins/`)
│   └── core
├── frontends
├── providers
└── router
```

`agent-shim-plugins` depends on `agent-shim-core` only. It does **not**
depend on `frontends`, `providers`, `gateway`, `config`, or `observability`.
This preserves the boundary rule from `docs/architecture.md` and keeps the
plugin trait surface independent of HTTP / wire-level concerns.

The new crate ships built-in plugins inside `src/builtin/`, gated by Cargo
features so operators can drop dependencies they do not use:

```toml
[features]
default = [
  "plugin-prompt-compressor",
  "plugin-pii-scrubber",
  "plugin-usage-recorder",
]
plugin-prompt-compressor = ["dep:tiktoken-rs"]   # already in workspace deps
plugin-pii-scrubber      = ["dep:regex"]
plugin-usage-recorder    = []
```

### 3.4 Plugin type vs. plugin instance

| Concept | Where it lives | Example |
|---|---|---|
| **Plugin type** | A Rust struct + `PluginFactory` registered at startup | `prompt_compressor` |
| **Plugin instance** | A named entry in `gateway.yaml` `plugins:` with its own config | `compressor_for_deepseek` |

The same type can have many instances with different configs. The
`route_index` (see §6) maps `(frontend, model)` → ordered list of instances
per hook.

### 3.5 Hot reload

Plugin instances are part of the v0.5 `AppSnapshot` (the hot-swappable
arc-swap layer). `AppCore` holds the immutable `factories` map; `AppSnapshot`
holds the `Arc<HashMap<String, Arc<dyn Plugin>>>` of currently-instantiated
plugins plus the precomputed `route_index`. On SIGHUP / POST /admin/reload:

1. New YAML runs the validation rules in §5.3.
2. All instances are rebuilt via `factory.instantiate(...)`.
3. On failure, the reload is rejected and the prior snapshot stays live.
4. On success, the snapshot is swapped; in-flight requests keep their
   captured snapshot Arc.

This reuses the v0.5 reload channel — no new concurrency primitives.

### 3.6 Third-party model: A1 (vendored)

All plugin types live in this repo. Third parties contribute via PR. The
trait surface (`Plugin`, `PluginFactory`, `PluginContext`, `HookSet`,
`PluginError`) is `pub` from day one so that a future migration to either:

- **A2** — operators fork AgentShim and add their own plugin crate as a
  Cargo dependency, plus one line in `register_builtin_plugins`, or
- **A3** — official feature flags for community crates, or
- **C** — WASM sandbox

…would not require breaking the trait. Today, AgentShim is shipped with the
plugins it bundles, full stop.

## 4. Trait design

### 4.1 Core trait

```rust
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    /// Globally-unique plugin type name. Must match the YAML `type:` field.
    fn type_name(&self) -> &'static str;

    /// Hook subset this instance subscribes to. The registry only calls the
    /// instance on hooks present in this set. Decided at construction time
    /// based on parsed config.
    fn hooks(&self) -> HookSet;

    /// H2: after decode, before route resolution.
    async fn on_decoded_request(
        &self,
        _ctx: &PluginContext,
        req: CanonicalRequest,
    ) -> PluginResult<CanonicalRequest> { Ok(req) }

    /// H3: after route + policy merge, before capability gate.
    async fn on_resolved(
        &self,
        _ctx: &PluginContext,
        req: CanonicalRequest,
        _target: &BackendTarget,
    ) -> PluginResult<CanonicalRequest> { Ok(req) }

    /// H5: per upstream→client stream event. Returns zero or more events;
    /// empty Vec = drop. Identity = `vec![event]`.
    async fn on_stream_event(
        &self,
        _ctx: &PluginContext,
        event: StreamEvent,
    ) -> PluginResult<Vec<StreamEvent>> { Ok(vec![event]) }

    /// H7: after completion. Fire-and-forget; return value is logged but
    /// does not affect the response.
    async fn on_response_complete(
        &self,
        _ctx: &PluginContext,
        _summary: &ResponseSummary,
    ) -> PluginResult<()> { Ok(()) }
}
```

**Design notes:**

- **All hooks default to no-op.** Plugins override only the hooks they need.
- **Hooks consume and return owned values**, not `&mut`. The registry
  clones the request before each plugin invocation so that a half-modified
  state from an `Err`-returning plugin never leaks (§6.4).
- **`&self`, not `&mut self`.** Plugin instances are conceptually stateless;
  if state is needed, the implementation uses interior mutability
  (`parking_lot::Mutex`, atomics, dashmap).
- **`hooks()` filtered at startup.** Validation rule §5.3.3 rejects YAML
  that references an instance on a hook it does not subscribe to.

### 4.2 Factory trait

```rust
pub trait PluginFactory: Send + Sync + 'static {
    fn type_name(&self) -> &'static str;

    fn instantiate(
        &self,
        instance_name: &str,
        config: serde_json::Value,
    ) -> Result<Box<dyn Plugin>, PluginConfigError>;
}
```

The factory owns its own serde-deserialisable config struct internally;
deserialisation errors are surfaced at startup with the YAML path of the
failure (§5.3.5).

### 4.3 Context and result types

```rust
pub struct PluginContext {
    pub request_id: agent_shim_core::RequestId,
    pub frontend: agent_shim_core::FrontendKind,
    pub route_label: String,         // e.g. "anthropic_messages/deepseek-chat"
    pub instance_name: &'static str, // YAML instance name
    // Reserved: future extension fields go here.
}

pub struct ResponseSummary {
    pub usage: Option<Usage>,
    pub elapsed_ms: u64,
    pub upstream_status: UpstreamStatus,
}

pub enum UpstreamStatus { Success, Error, Cancelled }

pub type PluginResult<T> = Result<T, PluginError>;

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("plugin {instance} timed out after {elapsed_ms}ms in {hook}")]
    Timeout { instance: String, hook: &'static str, elapsed_ms: u64 },

    #[error("plugin {instance} failed in {hook}: {message}")]
    Failed { instance: String, hook: &'static str, message: String },

    /// Plugin actively rejected the request. Mapped to HTTP 400 by the
    /// gateway (distinct from internal-error 502).
    #[error("plugin {instance} aborted request: {reason}")]
    Aborted { instance: String, reason: String },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HookSet(u8);

impl HookSet {
    pub const DECODED_REQUEST:    Self = Self(1 << 0);
    pub const RESOLVED:           Self = Self(1 << 1);
    pub const STREAM_EVENT:       Self = Self(1 << 2);
    pub const RESPONSE_COMPLETE:  Self = Self(1 << 3);
}
```

### 4.4 Where on-error / timeout live

`on_error` and `timeout_ms` are **not on the trait**. They live on
`InstanceEntry` inside the registry (§6.1) and are applied uniformly by the
registry's `invoke()` template (§6.3). This keeps the trait surface minimal
and lets new policy knobs land without touching every plugin.

## 5. YAML schema

### 5.1 Layout

```yaml
plugins:
  <instance_name>:
    type: <plugin_type_name>      # required
    config: <opaque map>          # optional, factory-defined shape
    on_error: skip | fail         # optional, default skip
    timeout_ms: <u64 | map>       # optional, see §5.4
    enabled: <bool>               # optional, default true

routes:
  - frontend: anthropic_messages
    model: deepseek-chat
    upstream: deepseek
    upstream_model: deepseek-chat
    plugins:
      on_decoded_request:   [<instance>, <instance>]   # order matters
      on_resolved:          [<instance>]
      on_stream_event:      [<instance>]
      on_response_complete: [<instance>]
```

Routes that omit `plugins:` run zero plugins (fast path, zero overhead — see
§6.5).

### 5.2 Full example

```yaml
plugins:
  compressor_for_deepseek:
    type: prompt_compressor
    config:
      strategy: summarize_old_turns
      keep_last: 4
      max_input_tokens: 32000
    on_error: skip
    timeout_ms: 50

  pii_scrubber_strict:
    type: pii_scrubber
    config:
      patterns: [email, phone, ssn]
    on_error: fail
    timeout_ms: 20

  usage_recorder:
    type: usage_recorder
    config: { sink: prometheus }

routes:
  - frontend: anthropic_messages
    model: deepseek-chat
    upstream: deepseek
    upstream_model: deepseek-chat
    plugins:
      on_decoded_request:   [pii_scrubber_strict, compressor_for_deepseek]
      on_stream_event:      [pii_scrubber_strict]
      on_response_complete: [usage_recorder]

  - frontend: anthropic_messages
    model: claude-sonnet
    upstream: anthropic
    upstream_model: claude-sonnet
    plugins:
      on_response_complete: [usage_recorder]

  - frontend: openai_chat
    model: gpt-4o
    upstream: copilot
    upstream_model: gpt-4o
    # no `plugins:` → zero plugins, fast path
```

### 5.3 Startup validation (fail-fast)

Validation runs in two layers because `agent-shim-config` is independent of
the plugin factory registry (which is wired in `gateway`).

**Layer A — `agent-shim-config::validate()` (schema-only checks):**

1. **Undeclared instance reference** — a route hook list contains a name not
   in `plugins:`.
2. **`timeout_ms == 0`** — rejected as misconfiguration.
3. **Duplicate instance names** — caught by YAML map-key uniqueness +
   `deny_unknown_fields` on the surrounding struct.

**Layer B — `gateway` boot, after factories are registered, before listener
binds:**

4. **Unknown plugin type** — instance references a `type:` no factory has
   registered. Error lists known types.
5. **Config deserialisation failure** — factory `instantiate(name, config)`
   returns `Err`. Error carries YAML path.
6. **Hook subscription mismatch** — once instances exist, check that every
   YAML route hook reference matches an instance whose `Plugin::hooks()`
   includes that hook. Lists allowed hooks in the error.

Both layers run before the listener binds, matching AgentShim's existing
fail-loudly philosophy. Layer A is exercised by `validate-config` CLI even
when no factories are linked (the CLI binary still includes them, but the
split makes the dependency explicit).

### 5.4 Per-hook timeouts

Default `timeout_ms` differs by hook because H5 runs per stream event:

| Hook | Default `timeout_ms` |
|---|---|
| `on_decoded_request` | 50 |
| `on_resolved` | 50 |
| `on_stream_event` | **5** |
| `on_response_complete` | 50 |

YAML `timeout_ms` accepts two shapes (serde `untagged`):

```yaml
# Single value — applies to all subscribed hooks
timeout_ms: 50

# Map — per-hook override
timeout_ms:
  default: 50
  on_stream_event: 5
```

### 5.5 Env-var overlay

Existing `AGENT_SHIM__` prefix mechanism handles plugin config without
additional code:

```
AGENT_SHIM__PLUGINS__COMPRESSOR_FOR_DEEPSEEK__ENABLED=false
AGENT_SHIM__PLUGINS__COMPRESSOR_FOR_DEEPSEEK__ON_ERROR=fail
AGENT_SHIM__PLUGINS__COMPRESSOR_FOR_DEEPSEEK__CONFIG__KEEP_LAST=8
```

### 5.6 `deny_unknown_fields` boundaries

The plugin-system structs (top-level `plugins:`, the per-instance entry, the
per-route `plugins:` map) all carry `#[serde(deny_unknown_fields)]`. Typos
such as `on_stream_evnt` fail at startup.

**Exception:** the per-instance `config:` block is an opaque
`serde_json::Value` because the config crate cannot statically know each
plugin's schema. Plugin factories enforce strictness internally with
`deny_unknown_fields` on their own config structs.

## 6. Implementation details

### 6.1 Registry internals

```rust
pub struct PluginRegistry {
    factories: HashMap<&'static str, Arc<dyn PluginFactory>>,
    instances: HashMap<String, Arc<InstanceEntry>>,
    route_index: HashMap<(FrontendKind, String), RouteHookPlan>,
}

struct InstanceEntry {
    plugin: Arc<dyn Plugin>,
    on_error: OnError,
    timeout_ms: HookTimeouts,
    enabled: bool,
}

#[derive(Clone)]
struct RouteHookPlan {
    on_decoded_request:   Vec<Arc<InstanceEntry>>,
    on_resolved:          Vec<Arc<InstanceEntry>>,
    on_stream_event:      Vec<Arc<InstanceEntry>>,
    on_response_complete: Vec<Arc<InstanceEntry>>,
}
```

`route_index` is pre-built at construction time. Wildcard routes
(`model: "*"`) follow the same "specific wins over wildcard" resolution as
the v0.4 router (§6.5).

### 6.2 Gateway-facing API

```rust
impl PluginRegistry {
    pub async fn run_on_decoded_request(
        &self,
        route: &(FrontendKind, String),
        ctx: &PluginContext,
        req: CanonicalRequest,
    ) -> Result<CanonicalRequest, PluginError>;

    pub async fn run_on_resolved(
        &self,
        route: &(FrontendKind, String),
        ctx: &PluginContext,
        req: CanonicalRequest,
        target: &BackendTarget,
    ) -> Result<CanonicalRequest, PluginError>;

    pub fn wrap_stream(
        &self,
        route: &(FrontendKind, String),
        ctx: PluginContext,
        upstream: CanonicalStream,
    ) -> CanonicalStream;

    pub async fn run_on_response_complete(
        &self,
        route: &(FrontendKind, String),
        ctx: &PluginContext,
        summary: &ResponseSummary,
    );  // no Result — fire-and-forget
}
```

### 6.3 The `invoke()` template

All hooks share one template that handles timeout, on_error, span, log,
metrics:

```rust
async fn invoke<T, F, Fut>(
    entry: &InstanceEntry,
    ctx: &PluginContext,
    hook: &'static str,
    timeout_ms: u64,
    work: F,
) -> Result<Option<T>, PluginError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, PluginError>>;
```

- `Ok(Some(v))` = success, caller applies the result
- `Ok(None)` = error was swallowed by `on_error: skip`; caller keeps prior
  state
- `Err(_)` = either `on_error: fail` or `Aborted`; caller propagates

### 6.4 Clone-then-swap for rewrite hooks

For H2 and H3 the registry clones `CanonicalRequest` before each plugin
call and only swaps on `Ok(Some(new_req))`. This guarantees:

- Partial writes from `Err`-returning plugins never leak.
- The order semantics in the YAML are exact: each plugin sees the request
  as left by the previous (successful) plugin.

`CanonicalRequest` is `Clone`; the clone cost is dominated by the messages
vector — typically O(prompt size) but allocation-light because `String`s
inside are heap copies. We accept this; the gateway is not high-frequency
trading.

### 6.5 Stream wrapper (H5)

`wrap_stream` short-circuits to identity (zero overhead) when the route has
no `on_stream_event` plugins:

```rust
let plan = match self.route_index.get(route) {
    Some(p) if !p.on_stream_event.is_empty() => p.clone(),
    _ => return upstream,    // fast path
};
```

When plugins are present, the wrapped stream uses `then` to invoke plugins
per event and `flat_map` to splice 1-in-N-out:

```rust
upstream.then(invoke_plugins).flat_map(futures::stream::iter)
```

Upstream errors pass through untouched (plugins never see `Err` items).
Plugin-side `Err` becomes a `Result::Err` on the downstream stream, which the
frontend encoder renders as an SSE error event in the appropriate dialect.

### 6.6 Pipeline integration points

`crates/gateway/src/pipeline.rs::dispatch_inner` adds plugin calls at four
locations:

| Approx. line (today) | Existing code | New code |
|---|---|---|
| ~521 | `let mut canonical = decoded.expect(...)` | `canonical = registry.run_on_decoded_request(&route, &ctx, canonical).await?` |
| ~532 | `canonical.resolved_policy = first_target.policy.resolve(&canonical)` | `canonical = registry.run_on_resolved(&route, &ctx, canonical, &first_target).await?` |
| ~738 (run_stream) and ~884 (run_unary) | `let stream = ...complete(...)?` | `let stream = registry.wrap_stream(&route, ctx, stream)` |
| ~893 (run_unary) and `Drop for StreamLogger` ~1025 | usage log | spawn `tokio::spawn(registry.run_on_response_complete(...))` — see §6.8 |

The pipeline does not learn about specific plugin types; only the registry
does. This is the "narrow seam" pattern from v0.5 / v0.6.

### 6.8 H7 invocation in sync `Drop` paths

`run_on_response_complete` is `async`, but `StreamLogger::Drop` (the
guard that fires when an Anthropic-style streaming response is dropped) is
synchronous. Two-part handling:

- **Unary path (`run_unary`)** — H7 is awaited inline before returning the
  HTTP response. The plugin's elapsed time is included in the request's
  total time.
- **Streaming path** — `StreamLogger` captures a clone of the
  `PluginRegistry` Arc, the route key, and the `PluginContext`. Its `Drop`
  impl spawns a `tokio::spawn(async move { registry.run_on_response_complete(...).await })`.
  Because H7 is fire-and-forget (no `Result` propagation), the spawn is
  fine; the response stream has already been drained when Drop runs.

The spawn happens on the gateway's main Tokio runtime (the same one that
hosts requests). H7 plugins are bound by their own `timeout_ms`, so a slow
plugin cannot starve the runtime indefinitely.

### 6.9 Errors → HTTP status mapping

`HandlerError` gains one new variant:

```rust
HandlerError::PluginFailed {
    kind: FrontendKind,
    instance: String,
    hook: &'static str,
    aborted: bool,        // true = Aborted, false = Failed/Timeout
}
```

Status mapping (in `handler_error_status_hint`):

- `aborted: true` → **400** (plugin actively rejected; client problem)
- `aborted: false` → **502** (gateway internal: plugin malfunctioned)

Error envelope is rendered by the inbound frontend, matching how
`HandlerError::CapabilityMismatch` is rendered today.

## 7. Logging and observability

### 7.1 Structured log fields (every invocation)

| Field | Type | Example |
|---|---|---|
| `plugin.instance` | string | `compressor_for_deepseek` |
| `plugin.type` | string | `prompt_compressor` |
| `plugin.hook` | string | `on_decoded_request` |
| `agent_shim.request_id` | string | inherited from root span |
| `agent_shim.route` | string | `anthropic_messages/deepseek-chat` |
| `plugin.outcome` | string | `success` \| `skipped` \| `failed` \| `timed_out` \| `aborted` |
| `plugin.elapsed_ms` | u64 | `4` |
| `plugin.on_error_policy` | string | `skip` \| `fail` |
| `plugin.error` | string \| null | only when outcome ≠ `success` |

### 7.2 Level policy

| Outcome | Level | Rationale |
|---|---|---|
| `success` | `DEBUG` | Avoid noise; per-event H5 calls would flood INFO |
| `skipped` | `WARN` | Silent failure must be visible |
| `failed` | `ERROR` | Accompanies a 502 response |
| `timed_out` | `WARN` (skip) / `ERROR` (fail) | Mirror failure mapping |
| `aborted` | `INFO` | Expected behaviour, not error |

### 7.3 OTel span

```
gateway.request                       (existing root span)
└── plugin.invoke
    fields: plugin.instance, plugin.type, plugin.hook,
            plugin.outcome (recorded after), plugin.elapsed_ms (after)
```

Pattern matches the existing `route.resolve` / `auth.verify` / `stream.encode`
child spans from v0.5.

### 7.4 Prometheus metrics

```
agent_shim_plugin_invocations_total{
    plugin_type, plugin_instance, hook, outcome
}

agent_shim_plugin_duration_seconds{
    plugin_type, plugin_instance, hook
}
```

Label cardinality is bounded by YAML (typical < 20 instances × 4 hooks × 5
outcomes = 400 series — well within Prometheus tolerances).

### 7.5 Stream hook noise control

`on_stream_event` success path skips structured log emission and updates
metrics only. Errors still log normally. Operators can enable per-event
trace logging via `RUST_LOG=agent_shim_plugins::stream=trace`.

### 7.6 PII red lines

The log/metric pipeline **never records request content**. Only:

- Instance name, type, hook (operator-defined static strings)
- Outcome, elapsed, error *category* / message (plugin-author-controlled —
  documented as "must not include user content")

Trace-level logging may opt into content for debugging; docs explicitly
forbid this in production.

## 8. Built-in plugins (v1)

Three plugins ship by default, each behind its own Cargo feature:

### 8.1 `prompt_compressor`

The primary motivating plugin. Subscribes to `on_decoded_request`.

Strategies (config-selected):

- `summarize_old_turns` — keep the last `keep_last` user/assistant turns;
  collapse the rest into a single synthetic system message
- `drop_old_turns` — keep only the last `keep_last` turns; drop the rest
- `truncate_to_tokens` — count tokens with `tiktoken-rs` (`cl100k_base`);
  drop oldest turns until total ≤ `max_input_tokens`

Reuses the workspace's existing `tiktoken-rs` dep (Cargo.toml:16). Zero new
dependencies.

### 8.2 `pii_scrubber`

Subscribes to `on_decoded_request` and `on_stream_event`.

- Built-in pattern set: `email`, `phone`, `ssn` (US format), `credit_card`
- Custom patterns via config (`regex` crate)
- Replacement: `[REDACTED_<KIND>]`
- Default `on_error: fail` — PII compliance must be fail-closed

### 8.3 `usage_recorder`

Subscribes to `on_response_complete` only. Emits operator-defined custom
metrics from `ResponseSummary`. Sinks (config-selected):

- `prometheus` — adds counters/histograms with operator-defined labels
- `log` — emits a structured INFO line per request

## 9. Testing strategy

| Layer | Coverage |
|---|---|
| `plugins` unit tests | Registry timeout/skip/fail/abort logic with mock plugins |
| `plugins` proptest | Stream wrapper: random event sequences × random drop/split patterns; assert no deadlock, no missed events |
| `gateway` integration (`tests/plugins_integration.rs`) | End-to-end through pipeline using mockito upstreams: each hook fires; YAML round-trip; on_error skip vs fail; abort → 400 |
| `gateway` benchmark | No-plugins config matches pre-plugin throughput (fast-path zero overhead) |
| Existing v0.4 / v0.5 / v0.6 tests | Must continue to pass unchanged with empty `plugins:` |

## 10. Alternatives considered

| Alternative | Why rejected |
|---|---|
| **Dynamic library (`dlopen`)** | Rust ABI instability; FFI burden; cross-platform pain |
| **WASM sandbox (`wasmtime`)** | +5–20 MB binary; per-call overhead measurable on streaming hot path; reserved for a future phase if a plugin ecosystem emerges |
| **gRPC sidecar plugins** | Adds an IPC hop per request — too costly for streaming |
| **A single Tower middleware layer** | Tower middleware operates on HTTP types, not the canonical model; would force plugin authors to think in axum primitives |
| **Hooks on `&mut CanonicalRequest`** | Partial-write hazard when a plugin errors halfway; clone-then-swap is safer (§6.4) |
| **One global `plugins:` list per route (single ordered list)** | Loses per-hook ordering control (e.g. request-side "scrub then compress" vs response-side "compress then scrub") |
| **Plugins act on provider wire format** | Would require N implementations per provider; canonical model is the right boundary |

## 11. Risks and open questions

1. **Plugin authors writing slow `on_stream_event` code.** Mitigated by the
   5ms default timeout, fail-fast on timeout, and metrics that surface
   p95 plugin duration. Operators see slow plugins in dashboards.
2. **Clone cost for very large prompts.** Worst case is one clone per plugin
   per hook; for a 100KB prompt across 3 plugins on H2 that is 300KB of
   transient allocation per request. Acceptable; the gateway is not on the
   hot allocation path of a high-volume inference system.
3. **Plugin author writing PII into logs.** Mitigated by documentation and a
   review-time checklist for new built-in plugins. Cannot be enforced by the
   type system.
4. **Hot-reload partial failure.** Already handled: validation runs before
   swap, snapshot stays consistent.
5. **Should the registry expose a way for a plugin to talk to another
   plugin's state?** Out of scope for v1; if needed later, a plugin can
   share state via a `OnceLock` / `dashmap` keyed by instance name.

## 12. Implementation phases (preview)

This will be expanded into a detailed plan by `writing-plans`. Rough shape:

1. **P01 — Crate scaffold + trait + registry skeleton.** New crate; trait
   types public; registry with `invoke()` template; no plugins yet; no
   pipeline integration.
2. **P02 — Config integration.** `plugins:` and `routes[].plugins:` in
   `agent-shim-config`; 6 validation rules; env-var overlay test.
3. **P03 — Pipeline integration.** Wire `run_on_decoded_request` /
   `run_on_resolved` / `wrap_stream` / `run_on_response_complete` into
   `pipeline.rs`. Empty registry = zero overhead.
4. **P04 — Observability.** Logs, OTel span, Prometheus metrics; PII
   red-lines documented.
5. **P05 — Built-in plugins.** `prompt_compressor`, `pii_scrubber`,
   `usage_recorder`; Cargo features.
6. **P06 — Hot-reload + integration tests + bench.** Snapshot swap; end-to-end
   `tests/plugins_integration.rs`; no-plugins benchmark vs v0.6 baseline.

## 13. Out of scope for v1 (deferred to later phases)

- WASM-based third-party plugins
- A registry / marketplace
- Per-route plugin overrides of per-instance `config` (today route-level
  controls only ordering and enable, not per-route config overrides)
- Plugin-to-plugin communication primitives
- Per-upstream (vs per-route) plugin chains — operators today achieve this
  by giving each upstream its own route
