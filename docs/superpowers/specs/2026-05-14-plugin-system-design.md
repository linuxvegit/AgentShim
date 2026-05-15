# Plugin System Design (Phase 7 candidate)

> **Status:** Draft design — approved through brainstorming Sections 1-4
> on 2026-05-14, then hardened through grilling Q1-Q14 on the same day.
> Subject to user review before implementation planning.
>
> **Decision log:** This revision incorporates 14 grilling decisions
> (Q1-Q14). ADR-0008 captures the most architecturally significant ones.

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
  compiled into the binary. Adding a new plugin kind requires a new build.
  Third parties contribute by PR (mode "A1" — see §3.6).
- **WASM / dlopen / sidecar plugins.** Considered and rejected for v1; see
  §10 *Alternatives considered*.
- **A plugin marketplace or registry.** Out of scope.
- **Mutating provider-side wire formats.** Plugins operate on the *canonical*
  model (`CanonicalRequest`, `StreamEvent`, `CanonicalResponse`). The
  frontends and providers stay untouched.
- **Changing the v0.4 resilience layer or v0.6 cost filter.** The plugin
  system runs *around* them, not inside them.
- **Stateful per-request stream plugins.** v1 H5 plugins are stateless
  across events; cross-event state (e.g. buffer-based PII scanning across
  `TextDelta` chunks) is deferred to a future phase. See §11 risks.

## 3. Architecture overview

### 3.1 Domain language (matches CONTEXT.md)

This spec adopts the existing AgentShim naming conventions; matching
glossary entries land in CONTEXT.md once approved.

- **Plugin** — YAML-declared, named entry under `plugins:` in
  `gateway.yaml`. Carries a `kind:`, `config:`, `on_error:`, `timeout_ms:`.
  Analogous shape to **Upstream**.
- **PluginKind** — Rust implementation of a plugin behavior. One kind
  backs many plugins. Naming follows the existing `FrontendKind` precedent.
- **Hook** — One of four well-defined points in the request lifecycle:
  `on_decoded_request`, `on_resolved`, `on_stream_event`,
  `on_response_complete`.

"Plugin instance" terminology from earlier drafts is replaced by **Plugin**
everywhere (Q1).

### 3.2 Hook points

The system exposes **four hooks**:

| Hook | Position in `dispatch_inner` | Sees | Can write |
|---|---|---|---|
| `on_decoded_request` (H2) | After frontend decode, before route resolution | `CanonicalRequest` | yes — return owned new value |
| `on_resolved` (H3) | After route + policy merge, before capability gate | `CanonicalRequest` + chosen `BackendTarget` | yes — per-upstream personalisation |
| `on_stream_event` (H5) | Around each `StreamEvent` flowing upstream→client | one `StreamEvent` | yes — emit zero or more events (1-in-N-out) |
| `on_response_complete` (H7) | After streaming/unary completion | `ResponseSummary` (usage + elapsed + status) | no — observation only |

All four hooks ship in v1 even though v1 built-ins cover only H2 and H7
(Q10). The complete trait surface preserves third-party extensibility and
avoids ABI churn when in-tree H3/H5 plugins are added later.

Rejected hook candidates (H1 raw bytes / H4 immediately-before-upstream /
H6 unary-only response) were excluded as redundant given H2+H3+H5+H7. They
remain available for future extension.

### 3.3 Crate layout

```
gateway
├── plugins *                    ← new crate (`crates/plugins/`)
│   ├── core
│   └── tokens *                 ← new leaf crate (Q14)
├── frontends
│   └── tokens *                 ← (migrated from inline OnceLock)
├── providers
├── router
│   └── tokens *                 ← (migrated from inline OnceLock)
└── tokens *                     ← new leaf crate
```

**New crate `agent-shim-tokens` (Q14, Section E):** a leaf crate that hosts
the single `cl100k_base` BPE encoder used by `prompt_compressor`,
`frontends::count_tokens`, and `router::cost_estimate`. Workspace-level
single source of truth. Adopts:

```rust
pub fn cl100k_encoder() -> &'static tiktoken_rs::CoreBPE;
pub fn count_text(s: &str) -> u32;   // convenience used by 3 call sites today
```

Both existing call sites in `crates/frontends/src/anthropic_messages/count_tokens.rs`
and `crates/router/src/cost_estimate.rs` migrate to this crate; their
inline `OnceLock` cells are removed. The new crate adds **no** workspace
dependency — `tiktoken-rs` is already a workspace dep.

Gateway startup calls `agent_shim_tokens::cl100k_encoder()` once during
`AppCore` construction to pay the ~50-100ms initialisation cost off the
hot request path.

**Plugin crate `agent-shim-plugins`** depends on `agent-shim-core` and
`agent-shim-tokens` only. It does **not** depend on `frontends`,
`providers`, `gateway`, `config`, or `observability`. This preserves the
boundary rule from `docs/architecture.md`.

The plugin crate ships built-in plugins inside `src/builtin/`, gated by
Cargo features so operators can drop dependencies they do not use:

```toml
[features]
default = [
  "plugin-prompt-compressor",
  "plugin-pii-scrubber",
  "plugin-usage-recorder",
]
plugin-prompt-compressor = ["agent-shim-tokens"]
plugin-pii-scrubber      = ["dep:regex"]
plugin-usage-recorder    = []
```

### 3.4 Plugin vs PluginKind

| Concept | Where it lives | Example |
|---|---|---|
| **PluginKind** | Rust struct + `PluginFactory` registered at startup | `prompt_compressor` |
| **Plugin** | A named entry in `gateway.yaml` `plugins:` with its own config | `compressor_for_deepseek` |

The same kind can have many plugins with different configs. The
`route_index` (see §6) maps `(frontend, model)` → ordered list of plugins
per hook.

### 3.5 Hot reload

PluginRegistry is **immutable after construction**; reload rebuilds it
entirely and swaps via arc-swap on `AppSnapshot`. In-flight requests retain
the prior registry's `Arc` for their lifetime. This guarantees route_index
and instance state stay consistent — there is no field-level swap (Q12).

On SIGHUP / POST /admin/reload:

1. New YAML runs the validation rules in §5.3.
2. A new PluginRegistry is constructed from scratch (`factory.instantiate(...)`
   for every plugin, including disabled ones — see §5.3 rule 7).
3. On any failure, the reload is rejected and the prior snapshot stays
   live. Operator sees a detailed error envelope.
4. On success, `AppSnapshot` (containing the new registry) is swapped.

This reuses the v0.5 reload channel — no new concurrency primitives.

### 3.6 Third-party model: A1 (vendored)

All plugin kinds live in this repo. Third parties contribute via PR. The
trait surface (`Plugin`, `PluginFactory`, `PluginContext`, `HookSet`,
`PluginError`) is `pub` from day one so that a future migration to either:

- **A2** — operators fork AgentShim and add their own plugin crate as a
  Cargo dependency, plus one line in `register_builtin_plugins`, or
- **A3** — official feature flags for community crates, or
- **C** — WASM sandbox

…would not require breaking the trait. Today, AgentShim is shipped with the
plugins it bundles, full stop.

**Stability:** the public trait surface is **not** semver-stable until
v1.0 (treated like every other AgentShim public type pre-1.0). The crate
declares this in its top-level `lib.rs` rustdoc.

## 4. Trait design

### 4.1 Core trait

```rust
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    /// Globally-unique plugin kind name. Must match the YAML `type:` field.
    /// (YAML keeps `type:` for consistency with `upstreams[].type:`; Q2.)
    fn kind_name(&self) -> &'static str;

    /// Hook subset this plugin subscribes to. The registry only calls the
    /// plugin on hooks present in this set. Decided at construction time
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
- **`&self`, not `&mut self`.** Plugins are conceptually stateless across
  events; if state is needed, the implementation uses interior mutability
  (`parking_lot::Mutex`, atomics, dashmap). Note: per-request stateful
  plugins on H5 are **out of scope for v1** (Q9).
- **`hooks()` filtered at startup.** Validation rule §5.3.6 rejects YAML
  that references a plugin on a hook it does not subscribe to.

### 4.2 Factory trait

```rust
pub trait PluginFactory: Send + Sync + 'static {
    fn kind_name(&self) -> &'static str;

    fn instantiate(
        &self,
        plugin_name: &str,
        config: serde_json::Value,
    ) -> Result<Box<dyn Plugin>, PluginConfigError>;
}
```

The factory owns its own serde-deserialisable config struct internally;
deserialisation errors surface at startup with the YAML path of the
failure (§5.3.5).

### 4.3 Context and result types

`PluginContext` carries **only request-level metadata**. It does **not**
carry the plugin's own name — that is an `invoke()`-local detail used by
the logging template, never exposed to plugins (Q4):

```rust
pub struct PluginContext {
    pub request_id: agent_shim_core::RequestId,
    pub frontend: agent_shim_core::FrontendKind,
    pub route_label: String,    // e.g. "anthropic_messages/deepseek-chat"
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
    #[error("plugin {plugin} timed out after {elapsed_ms}ms in {hook}")]
    Timeout { plugin: String, hook: &'static str, elapsed_ms: u64 },

    #[error("plugin {plugin} failed in {hook}: {message}")]
    Failed { plugin: String, hook: &'static str, message: String },

    /// Plugin actively rejected the request. Mapped to HTTP 400 (when
    /// possible — see §6.9 for stream-mid-flight semantics).
    #[error("plugin {plugin} aborted request: {reason}")]
    Aborted { plugin: String, reason: String },

    /// Plugin mutated a routing-affecting field. Treated as Failed.
    /// See §4.5 / Q3.
    #[error("plugin {plugin} modified protected field `{field}` in {hook}")]
    ProtectedFieldMutated { plugin: String, hook: &'static str, field: &'static str },
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

`on_error` and `timeout_ms` are **not on the trait**. They live on the
registry's per-plugin entry (§6.1) and are applied uniformly by the
registry's `invoke()` template (§6.3). This keeps the trait surface
minimal and lets new policy knobs land without touching every plugin.

### 4.5 Protected fields (runtime mutation check)

Plugins receive owned `CanonicalRequest` and may modify any field — except
the four that drive routing, identity, and pipeline control flow. After
each plugin returns successfully, the registry's `invoke()` template
compares **id**, **frontend**, **model**, **stream** between input and
output. Any change triggers `PluginError::ProtectedFieldMutated`, handled
per the plugin's `on_error` (Q3).

Allowed mutations include all message content, system instructions,
tools, generation options, response format, metadata, extensions,
inbound_anthropic_headers (carries no routing impact post-H2), and
resolved_policy (legitimate use case: a plugin that escalates reasoning
effort for premium users).

The diff cost is O(1) — three string compares + a bool compare.

## 5. YAML schema

### 5.1 Layout

```yaml
plugins:
  <plugin_name>:
    type: <plugin_kind_name>      # required; YAML keeps `type:` per Q2
    config: <opaque map>          # optional, kind-specific shape
    on_error: skip | fail         # optional, default skip
    timeout_ms: <u64 | map>       # optional, see §5.4
    enabled: <bool>               # optional, default true

routes:
  - frontend: anthropic_messages
    model: deepseek-chat
    upstream: deepseek
    upstream_model: deepseek-chat
    plugins:
      on_decoded_request:   [<plugin>, <plugin>]   # order matters
      on_resolved:          [<plugin>]
      on_stream_event:      [<plugin>]
      on_response_complete: [<plugin>]
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

1. **Undeclared plugin reference** — a route hook list contains a name not
   in `plugins:`.
2. **`timeout_ms == 0`** — rejected as misconfiguration.
3. **Duplicate plugin names** — caught by YAML map-key uniqueness +
   `deny_unknown_fields` on the surrounding struct.

**Layer B — `gateway` boot, after factories are registered, before listener
binds:**

4. **Unknown plugin kind** — `type:` value has no registered factory.
   Error lists known kinds.
5. **Config deserialisation failure** — factory `instantiate(name, config)`
   returns `Err`. Error carries YAML path.
6. **Hook subscription mismatch** — once plugins exist, every YAML route
   hook reference must match a plugin whose `Plugin::hooks()` includes
   that hook. Error lists allowed hooks for that kind.
7. **Disabled plugins still instantiate.** Plugins with `enabled: false`
   are constructed and validated as normal — config errors surface at
   startup regardless of enabled state. The disabled flag only suppresses
   registration into the route plan. This guarantees flipping `enabled`
   later via env-var overlay can't reveal a hidden config bug.

Both layers run before the listener binds. Layer A is exercised by
`validate-config` CLI even when no factories are linked (the CLI binary
still includes them, but the split makes the dependency explicit).

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

The plugin-system structs (top-level `plugins:`, the per-plugin entry, the
per-route `plugins:` map) all carry `#[serde(deny_unknown_fields)]`. Typos
such as `on_stream_evnt` fail at startup.

**Exception:** the per-plugin `config:` block is an opaque
`serde_json::Value` because the config crate cannot statically know each
kind's schema. Plugin factories enforce strictness internally with
`deny_unknown_fields` on their own config structs.

## 6. Implementation details

### 6.1 Registry internals

```rust
pub struct PluginRegistry {
    factories: HashMap<&'static str, Arc<dyn PluginFactory>>,
    plugins: HashMap<String, Arc<PluginEntry>>,
    /// Per-frontend route plans. Outer `None` (or all-empty inner) →
    /// fast path with zero atomic ops (Q5 option C).
    plans: HashMap<FrontendKind, FrontendRoutePlans>,
    /// JoinSet tracking spawned H7 tasks so shutdown can flush them (Q7).
    pending_h7: Arc<tokio::sync::Mutex<tokio::task::JoinSet<()>>>,
}

struct FrontendRoutePlans {
    specific: HashMap<String, RouteHookPlan>,
    wildcard: Option<RouteHookPlan>,
    is_empty: bool,    // true iff specific.is_empty() && wildcard.is_none()
}

struct PluginEntry {
    plugin: Arc<dyn Plugin>,
    on_error: OnError,
    timeout_ms: HookTimeouts,
    enabled: bool,
}

#[derive(Clone)]
struct RouteHookPlan {
    on_decoded_request:   Vec<Arc<PluginEntry>>,
    on_resolved:          Vec<Arc<PluginEntry>>,
    on_stream_event:      Vec<Arc<PluginEntry>>,
    on_response_complete: Vec<Arc<PluginEntry>>,
}
```

`plans` is built at construction time. Lookup is `(FrontendKind, &str)`:
the outer `HashMap<FrontendKind, _>` first; then the inner `specific` map;
finally `wildcard` as a fallback. The `is_empty` flag lets the fast path
exit at the outer level with zero inner work — used by the no-plugins
benchmark in §9.

### 6.2 Gateway-facing API

```rust
impl PluginRegistry {
    pub async fn run_on_decoded_request(
        &self,
        route: (FrontendKind, &str),
        ctx: &PluginContext,
        req: CanonicalRequest,
    ) -> Result<CanonicalRequest, PluginError>;

    pub async fn run_on_resolved(
        &self,
        route: (FrontendKind, &str),
        ctx: &PluginContext,
        req: CanonicalRequest,
        target: &BackendTarget,
    ) -> Result<CanonicalRequest, PluginError>;

    pub fn wrap_stream(
        &self,
        route: (FrontendKind, &str),
        ctx: PluginContext,
        upstream: CanonicalStream,
    ) -> CanonicalStream;

    /// Spawns H7 plugins onto an internal JoinSet; returns immediately.
    /// Tasks are tracked so shutdown can await them (§6.8).
    pub fn run_on_response_complete(
        &self,
        route: (FrontendKind, &str),
        ctx: PluginContext,
        summary: ResponseSummary,
    );

    /// Called by gateway shutdown handler.
    pub async fn flush_pending_h7(&self, deadline: Duration);
}
```

### 6.3 The `invoke()` template

All hooks share one template that handles timeout, on_error, span, log,
metrics:

```rust
async fn invoke<T, F, Fut>(
    entry: &PluginEntry,
    plugin_name: &str,         // local-only; never enters PluginContext (Q4)
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
- `Err(_)` = either `on_error: fail`, `Aborted`, or `ProtectedFieldMutated`;
  caller propagates

For H2 / H3, the template also performs the protected-field diff check
(§4.5) before returning the new request.

### 6.4 Clone-then-swap for rewrite hooks

For H2 and H3 the registry clones `CanonicalRequest` before each plugin
call and only swaps on `Ok(Some(new_req))`. This guarantees:

- Partial writes from `Err`-returning plugins never leak.
- The order semantics in the YAML are exact: each plugin sees the request
  as left by the previous (successful) plugin.

`CanonicalRequest` is `Clone`; the clone cost is dominated by the messages
vector — typically O(prompt size) but allocation-light because `String`s
inside are heap copies. Acceptable: the gateway is not on the hot
allocation path of a high-volume inference system.

### 6.5 Stream wrapper (H5)

`wrap_stream` short-circuits to identity (zero overhead) when the route has
no `on_stream_event` plugins:

```rust
let plan = match self.plans.get(&route.0) {
    None => return upstream,
    Some(fp) if fp.is_empty => return upstream,
    Some(fp) => match fp.specific.get(route.1).or(fp.wildcard.as_ref()) {
        Some(plan) if !plan.on_stream_event.is_empty() => plan.clone(),
        _ => return upstream,
    },
};
```

When plugins are present, the wrapped stream uses `then` to invoke plugins
per event and `flat_map` to splice 1-in-N-out:

```rust
upstream.then(invoke_plugins).flat_map(futures::stream::iter)
```

**Error handling at mid-stream (Q13):** Upstream errors pass through
untouched (plugins never see `Err` items). Plugin-side `Err` causes the
wrapped stream to emit one error event (shaped by the inbound frontend's
error envelope) and then close, propagating the close to the upstream
connection via Drop. Since HTTP status (200) was committed at first
event, the failure surfaces as a `event: error` SSE frame; the
`on_error: fail` semantic is preserved (stream actually closes) but the
HTTP status cannot change.

**Cancellation propagation (Q8):** H5 plugins introduce up to
`Σ on_stream_event_timeout_ms` of delay between client disconnect and
upstream cancellation. Bounded by the 5 ms default — typically tens of
milliseconds total, imperceptible to humans. Documented as a known
trade-off.

### 6.6 Pipeline integration points

`crates/gateway/src/pipeline.rs::dispatch_inner` adds plugin calls at four
locations. Line numbers omitted (they change with every patch); positions
are pinned by code comments referencing this spec section:

| Position | Existing code anchor | New code |
|---|---|---|
| After body decode | `let mut canonical = decoded.expect(...)` | `canonical = registry.run_on_decoded_request(...).await?` |
| After policy resolve | `canonical.resolved_policy = first_target.policy.resolve(&canonical)` | `canonical = registry.run_on_resolved(..., &first_target).await?` |
| Both streaming and unary paths, after `complete_with_cost_filter`/`complete` | `let stream = ...await?` | `let stream = registry.wrap_stream(..., stream)` |
| Unary completion + universal streaming guard | post-response log | `registry.run_on_response_complete(...)` (see §6.7) |

The pipeline does not learn about specific plugin kinds; only the
registry does. This is the "narrow seam" pattern from v0.5 / v0.6.

### 6.7 Universal H7 guard for all streaming paths (Q6)

`run_on_response_complete` is `async`, but Anthropic-style streaming uses
a synchronous `Drop` guard (`StreamLogger`) that fires after the response
stream finishes draining. Two cases:

- **Unary path (`run_unary`)** — H7 is awaited inline before returning
  the HTTP response. Plugin time included in total elapsed.
- **Streaming path (all frontends)** — All streaming branches share a
  generalised `H7Guard` (rename / extension of today's `StreamLogger`).
  The guard captures `Arc<PluginRegistry>`, the route key, and
  `PluginContext`. Its `Drop` impl calls `registry.run_on_response_complete(...)`,
  which delegates to a `tokio::spawn` since `Drop` is synchronous.

The `H7Guard` runs **uniformly across all three frontends** (Anthropic,
OpenAI Chat, OpenAI Responses); the per-frontend
`log_streaming_usage_on_drop` flag from today's pipeline is folded into
this guard. This is a clarification of today's behavior: today, only
Anthropic uses a Drop-time guard; v1 streaming paths for OpenAI Chat and
Responses also get one.

### 6.8 H7 spawn lifecycle and shutdown flush (Q7)

`run_on_response_complete` does not block its caller. Internally it
spawns each H7 plugin onto an internal `JoinSet<()>` owned by
`PluginRegistry`. The spawn is unbounded — high-QPS spikes can produce
many concurrent H7 tasks. Acceptable because H7 plugins are observation-
only and each is bounded by `timeout_ms` (50 ms default).

**Shutdown flush:** the gateway shutdown handler (`crates/gateway/src/shutdown.rs`)
calls `registry.flush_pending_h7(deadline)` after in-flight requests
drain but before runtime termination. `deadline` defaults to 5 s. Tasks
that fail to complete within the deadline are abandoned and logged as
`agent_shim_plugin_h7_dropped_at_shutdown_total{plugin_name}` counter
increments.

### 6.9 Errors → HTTP status mapping

`HandlerError` gains one new variant:

```rust
HandlerError::PluginFailed {
    kind: FrontendKind,
    plugin: String,
    hook: &'static str,
    aborted: bool,
}
```

Status mapping in `handler_error_status_hint`:

| Failure point | Outcome | HTTP status | Body |
|---|---|---|---|
| H2 / H3 plugin returns Failed (after on_error: fail) | 502 | inbound-frontend envelope, "gateway internal" |
| H2 / H3 plugin returns Aborted | 400 | inbound-frontend envelope, plugin's reason |
| H2 / H3 plugin mutates protected field | 502 (ProtectedFieldMutated → Failed-class) | inbound-frontend envelope, "plugin malfunctioned" |
| H5 plugin fails *before* first event surfaces downstream | 502 | inbound-frontend envelope |
| H5 plugin fails *after* HTTP headers commit (stream mid-flight) | already 200 | SSE `event: error` frame in inbound frontend's dialect; stream closes |
| H5 plugin aborts mid-stream | already 200 | same as fail-mid-stream; reason carried in error frame |
| H7 plugin fails | response already returned | no client-visible effect; logged + metered as `outcome=failed` |

Error envelope is rendered by the inbound frontend, matching how
`HandlerError::CapabilityMismatch` is rendered today.

## 7. Logging and observability

### 7.1 Structured log fields (every invocation)

| Field | Type | Example |
|---|---|---|
| `plugin.name` | string | `compressor_for_deepseek` |
| `plugin.kind` | string | `prompt_compressor` |
| `plugin.hook` | string | `on_decoded_request` |
| `agent_shim.request_id` | string | inherited from root span |
| `agent_shim.route` | string | `anthropic_messages/deepseek-chat` |
| `plugin.outcome` | string | `success` \| `skipped` \| `failed` \| `timed_out` \| `aborted` \| `protected_field_mutated` |
| `plugin.elapsed_ms` | u64 | `4` |
| `plugin.on_error_policy` | string | `skip` \| `fail` |
| `plugin.error` | string \| null | only when outcome ≠ `success` |

### 7.2 Level policy

| Outcome | Level | Rationale |
|---|---|---|
| `success` | `DEBUG` | Avoid noise; per-event H5 calls would flood INFO |
| `skipped` | `WARN` | Silent failure must be visible |
| `failed` | `ERROR` | Accompanies a 502 response (or SSE error frame) |
| `timed_out` | `WARN` (skip) / `ERROR` (fail) | Mirror failure mapping |
| `aborted` | `INFO` | Expected behaviour, not error |
| `protected_field_mutated` | `ERROR` | Programming error in the plugin |

### 7.3 OTel span

```
gateway.request                       (existing root span)
└── plugin.invoke
    fields: plugin.name, plugin.kind, plugin.hook,
            plugin.outcome (recorded after), plugin.elapsed_ms (after)
```

Pattern matches the existing `route.resolve` / `auth.verify` / `stream.encode`
child spans from v0.5.

### 7.4 Prometheus metrics

```
agent_shim_plugin_invocations_total{
    plugin_kind, plugin_name, hook, outcome
}

agent_shim_plugin_duration_seconds{
    plugin_kind, plugin_name, hook
}

agent_shim_plugin_h7_dropped_at_shutdown_total{
    plugin_name
}
```

**Cardinality (Q11):** the counter contributes `N_plugins × N_hooks × N_outcomes`
series. With a typical N_plugins=20, N_hooks=4, N_outcomes=6: **480 series**.
The histogram contributes `N_plugins × N_hooks × N_buckets` series.
`metrics-exporter-prometheus` uses 12 buckets by default, plus `_sum` and
`_count` per series, so each label combination expands to 14 series:
**1120 series**. Plus the shutdown counter (small).

**Total: ~1600 series for a typical config.** Well within Prometheus
tolerances. `plugin_kind` is technically redundant with `plugin_name`
(many-to-one) but kept because operators commonly aggregate by kind in
dashboards.

### 7.5 Stream hook noise control

`on_stream_event` success path skips structured log emission and updates
metrics only. Errors still log normally. Operators can enable per-event
trace logging via `RUST_LOG=agent_shim_plugins::stream=trace`.

### 7.6 PII red lines

The log/metric pipeline **never records request content**. Only:

- Plugin name, kind, hook (operator-defined static strings)
- Outcome, elapsed, error *category* / message (plugin-author-controlled —
  documented as "must not include user content")

Trace-level logging may opt into content for debugging; docs explicitly
forbid this in production.

## 8. Built-in plugins (v1)

Three plugins ship by default, each behind its own Cargo feature.

### 8.1 `prompt_compressor`

The primary motivating plugin. Subscribes to `on_decoded_request` only.

Strategies (config-selected):

- `summarize_old_turns` — keep the last `keep_last` user/assistant turns;
  collapse the rest into a single synthetic system message
- `drop_old_turns` — keep only the last `keep_last` turns; drop the rest
- `truncate_to_tokens` — count tokens via `agent_shim_tokens::count_text`;
  drop oldest turns until total ≤ `max_input_tokens`

### 8.2 `pii_scrubber`

Subscribes to **`on_decoded_request` only** in v1 (Q9 — stateful per-request
H5 deferred).

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
| `plugins` unit tests | Registry timeout / skip / fail / abort / protected-field-mutated logic with mock plugins |
| `plugins` proptest | Stream wrapper: random event sequences × random drop/split patterns; assert no deadlock, no missed events |
| `gateway` integration (`tests/plugins_integration.rs`) | End-to-end through pipeline using mockito upstreams: each hook fires; YAML round-trip; on_error skip vs fail; abort → 400; H5 error mid-stream → SSE error frame; H7 shutdown flush |
| `gateway` benchmark | No-plugins config matches pre-plugin throughput. Routes with `plugins:` block but empty hook lists also fast-path |
| `tokens` unit tests | `count_text` correctness on basic inputs; encoder identity across crate boundary |
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
| **Cancellation propagation via `tokio::select!` on a disconnect signal** (Q8 B) | No stream-level primitive for "downstream dropped"; engineering cost > benefit at 5 ms timeouts |
| **Stateful per-request H5 plugins** (Q9 B/C) | Real value, but `Box<dyn Any>` ergonomics are poor; defer to a future phase with its own ADR |
| **Tokenizer init unified into `agent-shim-core`** (Q14 A) | Violates ADR-0007 — `pub fn` not on the trait-only-additions whitelist |
| **Tokenizer init unified into `agent-shim-providers::oai_chat_wire`** (Q14 A') | Frontends would need to depend on providers — violates boundary rule |

## 11. Risks and open questions

1. **Plugin authors writing slow `on_stream_event` code.** Mitigated by
   the 5 ms default timeout, fail-fast on timeout, and metrics that surface
   p95 plugin duration.
2. **Clone cost for very large prompts.** Worst case is one clone per
   plugin per hook; for a 100 KB prompt across 3 plugins on H2 that is
   300 KB of transient allocation per request. Acceptable.
3. **Plugin author writing PII into logs.** Mitigated by documentation and
   a review-time checklist for new built-in plugins. Cannot be enforced
   by the type system.
4. **Hot-reload partial failure.** Already handled: validation runs
   before swap, snapshot stays consistent (§3.5, Q12).
5. **H5 cancellation delay.** Plugins delay client-disconnect → upstream-
   cancellation by up to `Σ on_stream_event_timeout_ms`. Bounded by the
   5 ms default. Documented (Q8).
6. **Stateful per-request H5 plugins deferred.** Cross-event PII scanning,
   token-rate gating, etc. need a per-request state slot. v1 H5 plugins
   are stateless across events. (Q9)
7. **`tiktoken_rs` initialisation cold-start.** Mitigated by gateway
   startup warmup call to `agent_shim_tokens::cl100k_encoder()` (§3.3, Q14).
8. **H7 task accumulation under burst.** Spawned tasks count is unbounded
   per process. Each is bounded by `timeout_ms`. Shutdown flush has a
   5-second deadline; tasks beyond that are abandoned with a counter
   increment.

## 12. Implementation phases (preview)

This will be expanded into a detailed plan by `writing-plans`. Rough shape:

1. **P01 — `agent-shim-tokens` extraction.** New leaf crate; migrate the
   two existing `OnceLock` cells; gateway warmup call. No plugin code.
2. **P02 — Plugin crate scaffold + trait + registry skeleton.** New crate;
   trait types public; registry with `invoke()` template; no plugin kinds
   yet; no pipeline integration.
3. **P03 — Config integration.** `plugins:` and `routes[].plugins:` in
   `agent-shim-config`; Layer A validation; env-var overlay test.
4. **P04 — Pipeline integration.** Wire `run_on_decoded_request` /
   `run_on_resolved` / `wrap_stream` / `run_on_response_complete` into
   `pipeline.rs`. Empty registry = zero overhead. Universal `H7Guard`.
5. **P05 — Observability.** Logs, OTel span, Prometheus metrics; shutdown
   flush counter; PII red-lines documented.
6. **P06 — Layer B validation + built-in plugin kinds.** Gateway-boot
   factory registration; `prompt_compressor`, `pii_scrubber`,
   `usage_recorder`; Cargo features.
7. **P07 — Hot-reload + integration tests + benchmark.** Snapshot swap;
   end-to-end `tests/plugins_integration.rs`; no-plugins benchmark vs
   v0.6 baseline.

## 13. Out of scope for v1 (deferred to later phases)

- WASM-based third-party plugins
- A registry / marketplace
- Per-route plugin overrides of per-plugin `config` (today route-level
  controls only ordering and enable, not per-route config overrides)
- Plugin-to-plugin communication primitives
- Per-upstream (vs per-route) plugin chains — operators today achieve this
  by giving each upstream its own route
- Stateful per-request plugins on H5 (cross-event state buffering)
- Per-frontend H5 wrapper specialisation (today the wrapper is identical
  across frontends; if e.g. Gemini's JSON-array stream needs special
  handling, that's future work)
