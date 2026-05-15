# ADR-0008: In-process Rust plugin system (Phase 7 candidate)

**Status:** Proposed, 2026-05-14.
**Related:** ADR-0002 (frozen-core), ADR-0004 (resilient caller —
the analogous "pipeline-wrapping orchestrator" pattern), ADR-0005
(hot-reload snapshot model — the substrate this plugin registry lives on),
ADR-0006 (cost filter — the prior phase's pipeline pre-pass), ADR-0007
(frozen-core lift discipline — checked against and respected).

## Context

AgentShim today is a fixed pipeline: frontend decode → router → policy →
capability gate → cost filter → resilient caller → provider → frontend
encode. Operators have asked for the ability to **rewrite or observe** the
data flowing through that pipeline without forking the codebase — most
concretely, to **compress verbose prompts** before sending them to less
context-tolerant upstreams.

The full design lives in
`docs/superpowers/specs/2026-05-14-plugin-system-design.md`. This ADR
captures the small set of decisions that are (a) hard to reverse, (b)
surprising without context, and (c) the result of explicit trade-offs.

## Decision

### (1) Plugin discovery model: A1 (vendored, compiled-in)

All plugin **kinds** ship inside this repository. Third parties contribute
new kinds via PR. There is no runtime plugin loading — no `dlopen`, no
WASM, no sidecar.

**Trade-offs considered:**

- WASM (`wasmtime`) was rejected for v1: ~5-20 MB binary size growth and
  measurable per-call overhead on the streaming hot path. Reserved as a
  future phase if a third-party ecosystem materialises.
- `dlopen` was rejected because Rust ABI is unstable; cross-platform FFI
  pain is real.
- gRPC sidecar plugins were rejected because of the IPC hop per request.

**Reversibility:** the public trait surface (`Plugin`, `PluginFactory`,
`PluginContext`, `HookSet`, `PluginError`) is designed to be reused if A1
is ever upgraded to A2 (operator-fork + private crate) or A3 (feature-
flagged community crates). The trait signatures do not change in those
scenarios; only the registration mechanism does.

### (2) Four hooks, statically defined: H2, H3, H5, H7

The system exposes exactly four hooks:

| Hook | Position |
|---|---|
| `on_decoded_request` (H2) | after decode, before route resolution |
| `on_resolved` (H3) | after route + policy merge, before capability gate |
| `on_stream_event` (H5) | per upstream→client `StreamEvent` |
| `on_response_complete` (H7) | after completion |

Adding a fifth hook is a non-breaking trait extension (default no-op
methods). Removing one is breaking.

**Trade-offs considered:**

- A "raw bytes" pre-decode hook (H1) was rejected as redundant given H2.
- A pre-upstream hook (H4) was rejected as redundant given H3.
- A unary-only response hook (H6) was rejected as redundant given H5
  (unary collapses into a small stream).

v1 ships built-in plugins covering only H2 (`prompt_compressor`,
`pii_scrubber`) and H7 (`usage_recorder`); H3 and H5 are kept in the
trait for future use and third-party extensibility.

### (3) Hook semantics: owned input, owned output (clone-then-swap)

Rewrite hooks (H2, H3) take `CanonicalRequest` by value and return
`PluginResult<CanonicalRequest>`. The registry clones the request before
each plugin call and only swaps on `Ok(_)`.

**Why:** `&mut` semantics let a plugin half-modify a request, then return
`Err`, leaking a tainted state to the next plugin. With clone-then-swap,
`Err` → original state is preserved.

The clone cost is `O(messages length)`. For a 100 KB prompt × 3 H2
plugins, that's 300 KB of transient allocation per request. Acceptable
for AgentShim's scale.

### (4) Protected fields: `id`, `frontend`, `model`, `stream`

Plugins may modify any `CanonicalRequest` field except these four. The
registry's `invoke()` template diffs them after each successful plugin
call; any change becomes `PluginError::ProtectedFieldMutated` (treated as
Failed-class for `on_error` purposes).

**Why these four:**

- `id`: changes break request tracing and metrics correlation.
- `frontend` / `model`: changes would silently bypass routing (since
  route resolution already ran). The most surprising failure mode in a
  pre-hook design.
- `stream`: pipeline branches on `is_stream` before H2 runs; changing it
  mid-flight is unimplementable without restructuring the dispatch loop.

Documented and enforced; cannot be turned off.

### (5) Configuration model: global plugins + per-route hook lists

Plugins are declared once under top-level `plugins:` (named, kind-typed,
configured), and referenced by name from `routes[].plugins.<hook>: [...]`
arrays. Each hook gets its own ordered list per route.

**Why per-hook lists (not a single list with per-plugin hook subscription):**

Order on the request side ("scrub then compress") is the inverse of the
response side ("compress then scrub" — pretending those were a real
thing). Forcing both into one list would mean either no order control on
one side or duplicating the list. Explicit per-hook lists are most clear.

**Why global declaration + per-route reference (not per-route inline
config):** matches how `upstreams:` and `routes:` already work in
AgentShim. Operators can DRY across routes that share a plugin.

### (6) Error policy: `on_error: skip | fail`, per-plugin

Plugins can fail (timeout, panic, return `Err`). Each plugin declares its
own policy:

- `skip` — log, increment metric, continue with prior state
- `fail` — propagate error, abort request (status depends on hook and
  stream position — see (7))

PII-class plugins (e.g. `pii_scrubber`) default to `fail` to be
fail-closed; routing-affecting plugins (e.g. `prompt_compressor`) default
to `skip` because failing a request because the compressor crashed is
worse than sending the uncompressed prompt.

**Special case: `Aborted` is not subject to `on_error`.** A plugin
returning `PluginError::Aborted { reason }` is always propagated and
mapped to HTTP 400.

### (7) Mid-stream error semantics

H5 plugin failures **after** the first event has been emitted cannot
change the HTTP status (already 200). Instead, the wrapped stream emits a
single `event: error` SSE frame in the inbound frontend's dialect and
closes — propagating the close to the upstream connection via Drop.

This means `on_error: fail` on H5 still **closes the stream** (preserving
fail-closed semantics) but the status code is whatever was sent at
headers commit. Documented; alternative (silently degrading to `skip`
mid-stream) would break the fail-closed promise that `pii_scrubber_strict`
relies on.

### (8) H7 lifecycle: spawn + tracked JoinSet + shutdown flush

`on_response_complete` is async, but the streaming path's response-drain
guard (`H7Guard`, generalised from today's `StreamLogger`) is
synchronous. The guard's `Drop` spawns H7 onto an internal `JoinSet`
owned by `PluginRegistry`. The gateway shutdown handler awaits this
JoinSet with a 5-second deadline; tasks beyond that are abandoned with a
counter increment.

**Trade-offs considered:**

- "Don't track" was rejected because shutdown-time `usage_recorder`
  drops are real-world unacceptable for billing-class plugins.
- "Per-plugin worker pool with bounded channel" was rejected as over-
  engineered for v1; can be revisited if H7 spawn count becomes a
  problem.

### (9) Tokenizer single source of truth: new `agent-shim-tokens` crate

The `prompt_compressor` plugin needs `tiktoken-rs::cl100k_base`. So does
`frontends::count_tokens` (Anthropic Messages token counting) and
`router::cost_estimate` (cost filter). Today each maintains its own
`OnceLock<CoreBPE>` cell.

This ADR creates a new leaf crate `crates/tokens/` that hosts the single
encoder. The two existing call sites migrate; the plugin uses the same
crate. Gateway startup pre-warms the encoder.

**Trade-offs considered:**

- Adding `pub fn cl100k_encoder()` to `crates/core/` was rejected: ADR-0007's
  "trait-only additions" rule does not permit `pub fn`. Adding it would
  require a wider ADR proposing a new invariant.
- Placing it in `crates/providers/oai_chat_wire/` was rejected: frontends
  do not depend on providers (boundary rule from `docs/architecture.md`).

The new leaf crate avoids both problems. Frontends, router, and plugins
all depend on it; it depends only on `tiktoken-rs`.

### (10) Frozen-core invariant: unchanged

This phase adds **no** items to `crates/core/`. The four hooks, registry,
trait types, and built-in plugin kinds all live in the new
`agent-shim-plugins` crate. ADR-0007's discipline rules are not
exercised here — Phase 7's design diffs against the v0.6.1 baseline for
`crates/core/` must be empty.

## Consequences

### Positive

- Operators can rewrite, filter, and observe canonical-model data at
  precise lifecycle points without forking AgentShim.
- The trait surface is small enough to maintain semver compatibility
  through v0.x → v1.0.
- The "no plugins configured" fast path is genuinely zero-cost (no
  atomic operations, no allocations) — validated by the benchmark in
  the spec's §9.
- Hot reload is uniform with v0.5's snapshot model; no new concurrency
  primitives.
- The tokenizer-init unification cleans up two existing duplicated
  `OnceLock` cells.

### Negative

- Plugin kinds are compile-time; adding one requires a rebuild. Users
  who want runtime extensibility must wait for a future WASM-based
  phase.
- H5 plugins introduce up to 5 ms × N plugins of cancellation
  propagation delay on client disconnect.
- H7 spawn count is unbounded under burst; bounded only by per-plugin
  timeout.

### Neutral

- Three new crates in the workspace (`tokens`, `plugins`). Brings the
  count to 11. Project structure remains a leaf-DAG.
- One new variant on `HandlerError` (`PluginFailed`); error envelope
  rendering reuses existing per-frontend machinery.
