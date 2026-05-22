# Architecture

## Crate Graph

```
agent-shim (gateway binary)
├── agent-shim-core        — canonical types (StreamEvent, CanonicalRequest, …)
├── agent-shim-config      — YAML + env configuration
├── agent-shim-observability — tracing / metrics setup
├── agent-shim-frontends   — request decoding + response encoding per API dialect
│   ├── anthropic_messages
│   ├── openai_chat
│   └── openai_responses
├── agent-shim-providers   — upstream HTTP clients
│   ├── oai_chat_wire      — shared OpenAI Chat encode/decode lib
│   ├── openai_compatible  — generic OpenAI-compatible client
│   ├── github_copilot     — Copilot token exchange + relay
│   ├── anthropic          — direct api.anthropic.com client (passthrough + canonical)
│   ├── deepseek           — native api.deepseek.com client (reasoning_content interleave)
│   └── gemini             — Generate Content API client (JSON-array streaming)
├── agent-shim-router      — route table: match frontend request → provider
└── agent-shim-protocol-tests — integration & fuzz tests (dev only)
```

## Request Lifecycle

```
Client HTTP request
  │
  ▼
axum router  ──►  FrontendProtocol::decode_request
                        │
                        ▼
                  CanonicalRequest
                        │
                        ▼
              Router::resolve  →  ProviderConfig
                        │
                        ▼
              Capability gate (image-vs-vision; aborts before upstream)
                        │
                        ▼
              Provider::call_stream / call_unary
                        │
                        ▼
                  CanonicalStream  (StreamEvent)
                        │
                        ▼
              FrontendProtocol::encode_stream / encode_unary
                        │
                        ▼
              SSE / JSON HTTP response to client
```

## Canonical Model

All internal data flows through types defined in `agent-shim-core`:

| Type | Purpose |
|------|---------|
| `CanonicalRequest` | Normalised inference request (messages, tools, params) |
| `CanonicalStream` | `Pin<Box<dyn Stream<Item=Result<StreamEvent,…>>>>` |
| `StreamEvent` | Tagged union of all streaming lifecycle events |
| `CanonicalResponse` | Completed non-streaming response |
| `StopReason` | Normalised stop cause across providers |

## Streaming Pipeline

Encoding is fully lazy: `encode_stream` returns a `FrontendResponse::Stream` whose
inner `BoxStream<Bytes>` pulls from the upstream `CanonicalStream` on demand.
Dropping the output stream propagates backpressure to the provider connection.

The Gemini provider is special: its streaming endpoint emits a **JSON array**
of response objects rather than SSE events. The provider ships a custom
byte-level scanner (`crates/providers/src/gemini/stream.rs`) that splits the
incoming array into per-element JSON objects, handling chunk boundaries at
any byte position (mid-string, mid-escape, between objects). The scanner is
exhaustively chunk-fuzzed.

## Boundary Rule

Frontends and providers **must not** import each other.
Both depend only on `agent-shim-core`.
The gateway crate is the only place that wires them together.

The shared `oai_chat_wire` lib lives inside `agent-shim-providers` so the
DeepSeek native provider, OpenAI-compatible provider, and GitHub Copilot
provider can all reuse the canonical→Chat-Completions encoder + parser
without circular imports. It exposes `canonical_to_chat::build`,
`chat_sse_parser`, and `chat_unary_parser`.

## Resilience layer (v0.4+)

Phase 4 introduces a `ResilientCaller` orchestrator in `crates/router/`
that sits between the existing `ModelResolver` and the existing
`BackendProvider` trait. It composes four subsystems:

1. **Per-route fallback chains.** Each route lists primary + backups
   in `upstreams: [...]`. The chain is walked in order on retry
   exhaustion when the last error is fallback-eligible.
2. **Per-route retries.** Exponential backoff + jitter + total-time
   budget within a single upstream.
3. **Per-`(upstream, model)` circuit breakers.** Sliding-window
   failure-rate trip; single half-open probe.
4. **Token-bucket rate limiting on four dimensions:** per API key,
   per route, per upstream, per source IP.

See [ADR-0004](adr/0004-resilient-caller.md) for the layering rationale
and [`docs/resilience.md`](resilience.md) for the operator-facing guide.

### Performance overhead targets

(From §8 of the [Phase 4 design spec](superpowers/specs/2026-05-08-phase-4-resiliency-design.md).)

- Rate-limit gate disabled: zero atomic ops, zero allocations on hot path.
- Rate-limit gate enabled, no buckets exceeded: ≤4 atomic loads per request.
- Breaker gate (always on): one `RwLock::read()` per chain element;
  uncontended path ~50ns.
- Retry overhead: zero on success.

No benchmark in v0.4 — Phase 5's metrics will surface real-world overhead.

### Frozen-core invariant resumes

ADR-0003 was a bounded one-time exception in Phase 3. All five Phase 4
plan files declared `core changes: NONE`, and `git diff v0.3.0..v0.4.0
-- crates/core/` is empty for the v0.4.0 release tag.

### Disciplined lift of the frozen-core invariant (v0.6.1)

The strict frozen-core invariant from v0.2 ("empty diff against the
v0.2 release baseline for `crates/core/`") was lifted in v0.6.1 to
admit one new trait module (`crates/core/src/cost.rs`, hosting the
`ImageTokenEstimator` trait that the cost-aware filter consumes
from the router crate). The lift is bound by four discipline rules
documented in [ADR-0007](adr/0007-frozen-core-lift-discipline.md):

1. **Trait-only additions** — no field changes, no signature
   changes on existing items.
2. **Category tag per hunk** — every `crates/core/` diff entry is
   classified in the release notes.
3. **Re-state in the next phase spec** — Phase 7's design will
   diff against the v0.6.1 baseline, not the v0.5.0 baseline.
4. **Other frozen crates stay frozen** — `crates/frontends/` and
   `crates/providers/src/` remain at zero diff against the v0.6.0
   baseline in v0.6.1.

ADR-0007 lays out a worked example for how v0.7+ phases may navigate
similar additions.

## Observability layer (v0.5)

Three subsystems, all running concurrently with each request, all
optional:

- **Prometheus metrics** — `agent_shim_*` counters/histograms via
  `metrics-rs`, scraped from `/metrics` on the admin port.
- **OpenTelemetry tracing** — `gateway.request` root span, child spans
  for `route.resolve`, `auth.verify`, `rate_limit.check`,
  `provider.complete`, `retry.attempt`, `stream.encode`. Exported via
  OTLP/gRPC when `otel.endpoint` is configured.
- **Hot-reload** — `arc-swap`-backed `AppSnapshot` swap on SIGHUP or
  POST /admin/reload. Validates rules 11-14 against the running
  `AppCore` baseline before swap. ADR-0005.

The boundary between immutable lifecycle state (`AppCore`, itself
`#[derive(Clone)]` so the reload-applying task can fork a handle
alongside the gateway server) and hot-swappable policy state
(`AppSnapshot`) is the architectural seam that makes hot-reload
tractable. See ADR-0005.

### Performance overhead targets (v0.5 additions over v0.4)

| Subsystem | Per-request cost (export disabled) | Per-request cost (export enabled) |
| --- | --- | --- |
| Metrics counter increment | < 5µs | (same) |
| OTel span allocation | < 10µs | < 25µs |
| ArcSwap snapshot read | < 100ns | (same) |
| Reload validation (50-route config) | n/a | < 100ms |

### Cost-aware routing (v0.6)

The Phase 6 cost filter sits **between** the rate-limit gate and the
v0.4 `ResilientCaller` chain walk. It's a pure pass over the
operator-defined fallback chain:

```
Client → FrontendProtocol::decode → Router::resolve → Vec<BackendTarget>
              │
              ▼
        Rate-limit gate (per-key, per-route, per-upstream, per-IP)
              │
              ▼
        CostFilter::filter_chain
          ├─ axis 1: tier        (upstream.tier ≥ route.min_tier?)
          ├─ axis 2: latency     (probe.recent_p95_ms ≤ budget?)
          └─ axis 3: cost cap    (estimate ≤ route.max_cost_usd?)
              │
              ▼
        Survivors (original chain order preserved)
              │
              ▼
        ResilientCaller (retry · breaker · fallback chain walk)
              │
              ▼
        Provider::complete → CanonicalStream → FrontendProtocol::encode
```

The filter applies four independent axes (tier / latency / cap /
estimated cost) as constraints, not as a re-sorter. Survivors keep
their original chain order — the operator's "preferred upstream"
intent from v0.4 is preserved.

Latency data is sourced from a `LatencyProbe` trait, with the
Prometheus-backed implementation in
`crates/gateway/src/latency_probe.rs` reading the
`agent_shim_upstream_duration_seconds` histogram via the
`metrics-exporter-prometheus` text scrape. The trait lives in the
router crate; the impl lives in the gateway crate, keeping the router
free of an observability-crate dependency.

Cost estimates are produced by `crates/router/src/cost_estimate.rs`
using `tiktoken-rs`'s `cl100k_base` encoder (initialised lazily via
`OnceLock`, matching `count_tokens.rs`). The estimate is the
upper-bound `(input_tokens × input_price + max_tokens × output_price) / 1M`.

When the filter empties the chain, the gateway short-circuits with
HTTP 503 `NoEligibleUpstream` before any provider is contacted. The
response body lists each skipped upstream and the per-axis reason via
the inbound frontend's existing error envelope.

See [ADR-0006](adr/0006-cost-aware-routing.md) for the design
context, the rejected alternatives, and the v0.6.1 / v0.7 deferred
items.

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
lists early-return without allocating. The empty-registry overhead is
measured at < 1 µs / hook on commodity hardware
(see `crates/plugins/benches/`).

### Hot reload (P07)

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
(with its old plugins) keeps running. There is no partial commit. The
admin endpoint surfaces this as `400 Bad Request` with
`{"ok": false, "errors": ["plugin validation error: ..."]}`.

In-flight requests bind an `Arc<AppSnapshot>` at the top of `dispatch`.
The arc-swap of the snapshot does not invalidate that bound Arc —
refcount remains > 0 until the request completes. The result: a reload
mid-request never changes plugin behavior for an in-flight request, only
for the next request that enters `dispatch`. This property is
regression-protected by
`crates/gateway/tests/plugins_reload.rs::reload_swap_isolates_in_flight_requests`.

### Failure semantics

The `Plugin::on_*` trait methods return `PluginResult<T>`. `PluginError`
is an enum whose primary variants are:
- `PluginError::Failed { plugin, hook, message }` — plugin returned an
  error from the hook. Routed through the per-plugin `on_error` policy.
- `PluginError::Aborted { plugin, hook, message }` — the plugin asked the
  request to be aborted. Always surfaces as `400 Bad Request`,
  irrespective of `on_error`.

The `on_error` policy lives in the YAML config:
- `on_error: fail` (default) → plugin error short-circuits the chain
  with HTTP 502 (Bad Gateway).
- `on_error: skip` → plugin error is swallowed; the chain continues
  with the un-modified request.

Per-hook timeouts are configurable via the `timeout_ms` knob on each
plugin entry; defaults are 50 ms for H2/H3/H7 and 5 ms for H5
(one event tick).

### v1 built-ins

- `pii_scrubber` (P06b1): regex-based PII redaction, inbound (H2) and
  outbound (H5). Per-rule match counter.
- `prompt_compressor` (P06b2): token-aware conversation compression with
  three strategies (`drop_old_turns`, `truncate_to_tokens`,
  `summarize_old_turns` with upstream call + timeout + fallback).
- `usage_recorder` (P06a): on-H7 Prometheus + log sinks for usage telemetry.

## Capability gate

Plan 04 added a capability gate at the gateway boundary. It runs after route
resolution and policy resolve, but BEFORE the provider's `complete()` call:

```rust
fn check_capabilities(req: &CanonicalRequest, caps: &ProviderCapabilities)
    -> Result<(), ProviderError>
{
    if request_has_image(req) && !caps.vision {
        return Err(ProviderError::CapabilityMismatch(
            "target provider does not support vision (image blocks present in request)".into(),
        ));
    }
    Ok(())
}
```

When the gate fires, no network call to the upstream is made. The error
surfaces back through `HandlerError::CapabilityMismatch { kind, message }`
and is rendered into the inbound frontend's error envelope shape:

* Anthropic Messages → `{"type":"error","error":{"type":"invalid_request_error","message":"…"}}`
* OpenAI Chat / Responses → `{"error":{"message":"…","type":"invalid_request_error","code":"capability_mismatch"}}`

Both with HTTP 400. See `crates/gateway/src/pipeline.rs::check_capabilities`.

In v0.3, the OpenAI Responses frontend joined the capability matrix —
Responses cells are now active for OAI-compat, Copilot, Anthropic, and
Gemini, and the DeepSeek negative cell goes through the same gate as the
existing Anthropic Messages and OpenAI Chat cells. See
[ADR-0003](adr/0003-promote-safety-ratings.md) and the Phase 3 plan files
under `docs/superpowers/plans/`.

## Extension namespaces

Per ADR-0002 (frozen-core), the canonical model never grows new variants for
provider-specific fields. Provider-specific data lands on the relevant
content block's `extensions` map under a per-provider prefix:

| Wire field | Origin provider | Extension key |
|---|---|---|
| `cache_control` | Anthropic | `cache_control` |
| `cache_creation_input_tokens` | Anthropic | (Usage struct field) |
| `prompt_cache_hit_tokens` | DeepSeek | (Usage struct field) |
| `safetyRatings` | Gemini | `gemini.safety_ratings` (deprecated; see below) |
| `citationMetadata` | Gemini | `gemini.citation_metadata` |

When adding a new provider, namespace any new extension keys with
`<provider_name>.` so they don't collide.

**v0.3 typed-field exception.** v0.3 promoted exactly one extension key
to a typed canonical field — `Usage.safety_ratings: Option<Vec<SafetyRating>>`,
the first new core field since v0.1, per
[ADR-0003](adr/0003-promote-safety-ratings.md). The Gemini provider
double-writes both the typed field and the legacy
`extensions["gemini.safety_ratings"]` key for v0.3.0; the extension write
is removed in v0.3.1 (see the `// REMOVE in v0.3.1` markers in
`crates/providers/src/gemini/response.rs`). Plan files for v0.3.x and
v0.4 carry `core changes: NONE` again — the freeze remains the default.

## The hybrid Anthropic path

The Anthropic provider has TWO outbound paths:

1. **Passthrough (byte-identity)**: when the inbound frontend is also
   Anthropic Messages, the request body is forwarded to api.anthropic.com
   verbatim and the SSE response is streamed back unchanged. This is the
   only path that round-trips lossless features like `cache_control`
   markers, signature deltas on thinking blocks, and Anthropic-only
   metadata fields.
2. **Canonical translation**: when the inbound frontend is OpenAI Chat (or
   any non-Anthropic dialect), the request is encoded by the
   `anthropic::request::build` module and the response is parsed by
   `anthropic::response`. This path is lossy for Anthropic-only features
   (signatures, redacted thinking) but supports vision, tools, and
   reasoning round-trip.

The pipeline picks between them via `BackendProvider::proxy_raw` returning
`Ok(Some(...))` for the passthrough case and `Ok(None)` for the canonical
case.
