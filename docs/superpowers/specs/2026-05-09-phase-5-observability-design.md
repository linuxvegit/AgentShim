# Phase 5 / v0.5 — Observable & Operable Gateway

**Status:** Approved (brainstorming complete; ready for implementation planning)
**Date:** 2026-05-09
**Workspace baseline:** 0.4.1 (master HEAD `8945514`)
**Workspace target:** 0.5.0
**Source brainstorm:** elicited via `superpowers:brainstorming` on 2026-05-09
**Predecessor:** [Phase 4 — Resiliency](2026-05-08-phase-4-resiliency-design.md) (v0.4.0)

---

## 1. Scope

### Goal

Turn AgentShim from "a single-binary that runs" into "a single-binary that
prod ops teams can monitor, trace, and reconfigure without restart."

No new request-path features. No new providers. No new frontends. Phase 5
is about giving operators visibility (metrics, traces) and control
(hot-reload) over the gateway already shipped in v0.4.

### Three pillars (in scope)

1. **Prometheus metrics** — `/metrics` endpoint on a separate admin
   listener. Counters, histograms, and gauges instrumenting every layer
   of the request path.
2. **OpenTelemetry tracing** — first-class spans created via the
   `tracing-opentelemetry` bridge; honors inbound `traceparent`; exports
   via OTLP/gRPC when `otel.endpoint` is set, otherwise no-op.
3. **Hot-reload of routing & policy config** — atomic snapshot swap
   (arc-swap) on `SIGHUP` (Unix) or `POST /admin/reload` (cross-platform).
   Validates the new config before swapping; rolls back on validation
   failure; in-flight requests retain their original snapshot.

### Out of scope (punted to v0.6+)

- Redacted request/response capture
- k8s manifests / Helm chart
- Distributed breaker/rate-limit state (Redis backend)
- Closing the proxy_raw resilience bypass (documented v0.4 gap)
- Cost/latency-aware routing
- New providers, new frontends, new model alias features
- Filesystem-watcher reload (operators trigger reload explicitly)

### Locked decisions

| #   | Decision                       | Choice                                                              |
| --- | ------------------------------ | ------------------------------------------------------------------- |
| D1  | Theme                          | Observability & ops                                                 |
| D2  | Three pillars                  | Prometheus + OpenTelemetry + hot-reload                             |
| D3  | Metrics endpoint exposure      | Separate admin listener (default `127.0.0.1:9100`)                  |
| D4  | OTel posture                   | First-class spans always; export optional via `otel.endpoint`       |
| D5  | Hot-reload scope               | Routing & policy only (not credentials, not server config)          |
| D6  | Metrics library                | `metrics-rs` facade + `metrics-exporter-prometheus`                 |
| D7  | Snapshot pattern               | `arc-swap` over `Arc<AppSnapshot>`                                  |
| D8  | Plan structure                 | 5 plans, foundation-first (mirrors Phase 4)                         |
| D9  | Reload trigger                 | SIGHUP (Unix) + POST /admin/reload (cross-platform). No fs-watcher. |
| D10 | Default admin bind             | Loopback only; absent config → admin listener disabled              |
| D11 | OTel sampling                  | Parent-based; operator overrides via `otel.sample_ratio` (0.0–1.0)  |
| D12 | Frozen-core invariant          | Empty diff for `crates/core/`, `crates/frontends/`, `crates/providers/src/` against v0.4.1 |

### Frozen-core invariant continues

Every commit on the Phase 5 branch must keep
`git diff master -- crates/core/ crates/frontends/ crates/providers/src/`
empty (where `master` is at v0.4.1 baseline `8945514`). Phase 5 work
lives in:

- `crates/config` (new schema blocks, validation rules 11-14)
- `crates/observability` (new `metrics`, `otel`, `reload` modules)
- `crates/router` (instrumentation hooks; no behavior change)
- `crates/gateway` (admin listener, AppState pivot, handlers)
- `crates/protocol-tests` (1-2 end-to-end smoke tests)

---

## 2. Architecture

### 2.1 Crate-level changes

```
crates/
  core/           [FROZEN — zero diff]
  frontends/      [FROZEN — zero diff]
  providers/      src/   [FROZEN — zero diff]
                  tests/ [may add tests]
  config/         + AdminConfig, OtelConfig, MetricsConfig blocks
                  + reload-validation (rules 11-14)
  observability/  + metrics module (registry, names, helpers)
                  + otel module (subscriber init, traceparent ingestion)
                  + reload module (snapshot semantics, ArcSwap helpers)
  router/         + instrumentation hooks on ResilientCaller (events
                    emit metrics + span attrs alongside existing
                    tracing events)
                  + AppSnapshot type wrapping route table / policies
  gateway/        + admin_listener.rs (second axum::Router)
                  + handlers/admin/{metrics,reload,health}.rs
                  + signal handler for SIGHUP (Unix) → reload trigger
                  + AppState becomes Arc<AppCore> + Arc<ArcSwap<AppSnapshot>>
  protocol-tests/ + reload smoke tests
```

**Why `observability` becomes the home for all three pillars:** the
crate already exists for tracing setup. Adding metrics + otel +
reload-snapshot helpers there keeps the gateway crate thin (just
listener/handler wiring) and lets unit tests for naming/init logic
live next to the code.

### 2.2 The AppSnapshot / AppCore split

Today's `AppState` (in `crates/gateway/src/state.rs`) holds: router,
breaker_registry, limiter_registry, retry policies, auth state,
providers. Phase 5 splits it into a hot-swappable layer and an
immutable layer.

```rust
// What CAN reload (hot-swappable on SIGHUP / POST /admin/reload):
pub struct AppSnapshot {
    pub router: Arc<Router>,
    pub retry_policies: HashMap<RouteKey, RetryPolicy>,
    pub breaker_policies: HashMap<RouteKey, BreakerPolicy>,
    pub fallback_chains: HashMap<RouteKey, Vec<UpstreamRef>>,
    pub auth_keys: HashSet<KeyHash>,
    pub auth_required: bool,
    pub rate_limit_buckets: LimiterRegistry, // see §5.4 reset semantics
    pub logging_filter: EnvFilter,           // reload via tracing reload-layer
}

// What CANNOT reload (build-once, restart required):
pub struct AppCore {
    pub providers: HashMap<String, Arc<dyn BackendProvider>>, // creds bound here
    pub breaker_registry: Arc<BreakerRegistry>,               // state, not policy
    pub server_config: ServerConfig,                          // bind/port
    pub admin_config: AdminConfig,
    pub otel: Option<OtelHandle>,
    pub metrics: Arc<MetricsHandle>,
}

// Top-level state seen by handlers:
pub struct AppState {
    pub snapshot: Arc<ArcSwap<AppSnapshot>>,  // the hot-swappable bit
    pub core: Arc<AppCore>,                   // immutable for the process lifetime
}
```

**Critical invariant — breaker state survives reload, breaker policy
doesn't.** `BreakerRegistry` lives in `core` (state); `breaker_policies`
live in `snapshot` (config). On reload, the registry keeps tripping
based on real upstream behavior; only the thresholds get updated. Same
shape for the rate-limit `LimiterRegistry`, with one asymmetry: see §5.4.

**Pre-stream snapshot capture.** Each request reads
`state.snapshot.load_full()` once at the top of the pipeline and uses
that `Arc<AppSnapshot>` for the entire request lifetime. Mid-request
reload doesn't reach in-flight streams. New requests after reload see
the new snapshot.

### 2.3 Admin listener

```rust
// Sketch — gateway/src/main.rs serve path
let public_listener = TcpListener::bind(server_config.bind_addr()).await?;
let admin_listener  = TcpListener::bind(admin_config.bind_addr()).await?;

let public_router = build_public_router(state.clone());
let admin_router  = build_admin_router(state.clone());

tokio::select! {
    res = axum::serve(public_listener, public_router)
            .with_graceful_shutdown(shutdown_signal()) => res?,
    res = axum::serve(admin_listener, admin_router)
            .with_graceful_shutdown(shutdown_signal()) => res?,
}
```

**Default bind:** loopback `127.0.0.1:9100`. If `admin.bind` is omitted,
the admin listener is **disabled entirely** (not bound to a default).
Operators who want metrics/reload opt in by setting `admin.bind`.

**Endpoints on admin port:**

| Method | Path             | Purpose                                                |
| ------ | ---------------- | ------------------------------------------------------ |
| GET    | `/metrics`       | Prometheus text format scrape                          |
| GET    | `/healthz`       | Liveness — process is up (moved here from public port) |
| GET    | `/readyz`        | Readiness — config loaded, providers initialized       |
| POST   | `/admin/reload`  | Trigger config reload (200 / 4xx / 403)                |

**Endpoints staying on public port:** `/v1/messages`,
`/v1/chat/completions`, `/v1/responses`. Nothing else.

---

## 3. Prometheus metrics (Plan 02)

### 3.1 Metric set

All metrics live under the `agent_shim_` prefix and are registered at
startup so they appear in `/metrics` even with zero observations.

**Request lifecycle:**

```
agent_shim_requests_total{frontend, route, status_class}            counter
agent_shim_request_duration_seconds{frontend, route, status_class}  histogram
agent_shim_request_body_bytes{frontend, route}                      histogram
```

- `frontend` ∈ `{anthropic_messages, openai_chat, openai_responses}`
- `route` = the matched route alias (bounded by config; ~50 in practice)
- `status_class` ∈ `{2xx, 4xx, 5xx, cancelled}`

**Resilience layer (mirrors v0.4 tracing taxonomy):**

```
agent_shim_retry_attempts_total{route, upstream, attempt}                 counter
agent_shim_retry_exhausted_total{route, upstream}                         counter
agent_shim_fallback_transitions_total{route, from_upstream, to_upstream}  counter
agent_shim_breaker_state_changes_total{upstream, model, from, to}         counter
agent_shim_rate_limit_rejected_total{dimension}                           counter
```

**Upstream call:**

```
agent_shim_upstream_duration_seconds{upstream, model, status_class}  histogram
agent_shim_upstream_errors_total{upstream, model, error_class}       counter
```

`error_class` reuses `error_class_label()` from
`crates/router/src/fallback.rs` (`network`, `upstream_5xx`,
`upstream_429`, `upstream_4xx`, `cancelled`, `other`).

**Token accounting:**

```
agent_shim_tokens_input_total{route, upstream, model}   counter
agent_shim_tokens_output_total{route, upstream, model}  counter
```

**Reload:**

```
agent_shim_config_reloads_total{result}  counter
```

`result` ∈ `{ok, validation_error, immutable_field}`.

**Process gauges (auto, via `metrics-exporter-prometheus`):**

```
agent_shim_in_flight_requests                                      gauge
process_resident_memory_bytes / process_cpu_seconds_total / etc.   (auto)
```

### 3.2 Cardinality bounds

Big risk in metrics is unbounded label cardinality. Concrete bounds:

- `frontend` — 3 values, fixed
- `route` — bounded by the number of routes in YAML (typically <50)
- `upstream` / `model` — bounded by `upstreams.*` × `routes[].upstream_model`
- `status_class` / `error_class` — small fixed sets
- `attempt` — capped at `retry.max_attempts` (typically ≤5)

**Explicitly NOT labels:** `model_alias_requested_by_client` (could be
anything an agent sends), `api_key_hash` (cardinality blows up),
`request_id` (uniqueness == disaster). These flow through tracing,
not metrics.

### 3.3 Implementation surface

```
crates/observability/src/metrics/
  mod.rs        — pub fn install(cfg: &MetricsConfig) -> MetricsHandle
                  — describe_metrics() registers names & help text up front
  names.rs      — pub const REQUESTS_TOTAL: &str = "agent_shim_requests_total"; …
  recorders.rs  — typed wrappers calling counter!()/histogram!() to enforce
                  label sets at compile time

crates/router/src/resilient_caller.rs
  — emits metrics at the same call sites as the existing tracing events
  — no new tracing event names; metrics reuse the v0.4 taxonomy

crates/gateway/src/handlers/{messages,chat_completions,responses}.rs
  — record agent_shim_requests_total / agent_shim_request_duration_seconds
    via a tower middleware (Layer + Service<Request>)
  — agent_shim_in_flight_requests via Drop-guard counter
```

### 3.4 /metrics handler

```rust
// crates/gateway/src/handlers/admin/metrics.rs
pub async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    let body = state.core.metrics.render();   // metrics-exporter-prometheus
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
}
```

### 3.5 Tests

| Test                               | Layer       | Coverage                                                                 |
| ---------------------------------- | ----------- | ------------------------------------------------------------------------ |
| `metrics::names::all_unique`       | unit        | every const name distinct                                                |
| `metrics::install_then_render`     | unit        | install→render returns non-empty Prometheus text                         |
| `metrics::cardinality_bound`       | unit        | known fixed-cardinality labels enumerate to N values                     |
| `metrics_endpoint_serves_text`     | integration | hit `/metrics`, parse with `prometheus-parse`, assert metrics present    |
| `request_increments_counter`       | integration | issue 3 requests, scrape, assert counter == 3                            |
| `retry_attempts_counted`           | integration | mock 503-then-200, assert `retry_attempts_total{attempt=2} == 1`         |
| `breaker_state_change_counted`     | integration | force breaker open, assert `breaker_state_changes_total{closed→open}=1`  |
| `cardinality_label_set_stable`     | unit        | property test: any (frontend, route, status) drawn from generators stays within enumerated set |

---

## 4. OpenTelemetry tracing (Plan 03)

### 4.1 Span model

One span tree per HTTP request. Span names use OTel semantic-convention
style (`<operation>.<noun>` lowercase).

```
gateway.request                                       (root)
├─ route.resolve                                       (instant; ~µs)
├─ auth.verify                                         (instant; only when auth.enabled)
├─ rate_limit.check                                    (instant)
├─ provider.complete (upstream=copilot, model=gpt-4o)  (the hot one)
│  ├─ retry.attempt #1
│  ├─ retry.attempt #2
│  └─ retry.attempt #3
├─ provider.complete (upstream=openai, model=gpt-4o)   (only on fallback)
│  └─ retry.attempt #1
└─ stream.encode                                       (encompasses SSE encoding)
```

**`gateway.request` attributes (always present):**

- `http.request.method`, `http.route` — OTel HTTP semconv
- `agent_shim.frontend` — `anthropic_messages` / `openai_chat` / `openai_responses`
- `agent_shim.route` — the matched route alias
- `agent_shim.identity` — `sha256:<hex>` or `anonymous`
- `agent_shim.request_id` — UUID; same one v0.4 tracing layer generates
- `http.response.status_code`, `agent_shim.status_class` — set on close

**`provider.complete` attributes:**

- `agent_shim.upstream` — provider name from config
- `agent_shim.model` — `upstream_model` actually called
- `agent_shim.attempts` — final retry count (1 if no retries)
- `agent_shim.fallback_position` — 0 for primary, 1+ for fallback positions

**Span events (attached to the relevant span):**

- `breaker.state_change` → on the `provider.complete` span that observed the transition
- `rate_limit.rejected` → on the `rate_limit.check` span
- `retry.attempt` → on the `retry.attempt #N` child span (not as event)
- `retry.exhausted` → on the parent `provider.complete` span

The v0.4 structured tracing events under
`target = "agent_shim::resilience"` continue to fire — they're how
local-dev logs work without an OTel collector. The OTel layer attaches
the same events to the corresponding spans automatically via the
`tracing-opentelemetry` bridge.

### 4.2 Subscriber stack

```rust
// crates/observability/src/otel/init.rs
pub fn init(otel_cfg: &OtelConfig, log_cfg: &LoggingConfig) -> Result<TracingHandles> {
    let env_filter = make_filter(&log_cfg.filter);
    let fmt_layer = make_fmt_layer(&log_cfg.format);

    let otel_layer = if let Some(endpoint) = &otel_cfg.endpoint {
        let tracer = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_exporter(
                opentelemetry_otlp::new_exporter()
                    .tonic()
                    .with_endpoint(endpoint),
            )
            .with_trace_config(make_resource(otel_cfg))
            .install_batch(opentelemetry_sdk::runtime::Tokio)?;
        Some(tracing_opentelemetry::layer().with_tracer(tracer))
    } else {
        None
    };

    Registry::default()
        .with(env_filter)
        .with(fmt_layer)
        .with(otel_layer)             // None → no-op
        .try_init()?;

    Ok(TracingHandles { /* shutdown handle for graceful drain */ })
}
```

**Spans always exist. Export is optional.** When `otel.endpoint` is
unset, the `otel_layer` is `None` and spans flow only to the fmt layer.
Local development gets richer log output (span structure visible)
without needing a collector. Production sets
`otel.endpoint: http://otel-collector:4317`.

**Sampling:** parent-based sampling is the default — if the inbound
`traceparent` header has the sampled bit set, we sample; otherwise we
drop. Operators override via `otel.sample_ratio` (0.0–1.0, default 1.0
= head-sample everything).

### 4.3 Inbound trace context

The Phase 4 request middleware already extracts a request ID. Phase 5
adds: parse `traceparent` per the W3C spec, attach to the root span as
parent context. Outbound HTTP calls (`reqwest`) inject the current
span's `traceparent` so upstreams can continue the trace.

Implementation: a `tower-http::trace::TraceLayer` already wraps the
public router. We add an `OtelInjectLayer` (custom, ~30 LOC) on the
upstream side that mutates outbound request headers using the current
`tracing::Span` context.

### 4.4 Configuration

```yaml
otel:
  endpoint: http://otel-collector:4317   # optional; absent → no export
  service_name: agent-shim               # default
  service_version: ${CARGO_PKG_VERSION}  # auto-populated
  sample_ratio: 1.0                      # default; 0.0–1.0
  resource_attrs:                        # operator-defined extras
    deployment.environment: prod
    cloud.region: us-west-2
```

### 4.5 Tests

| Test                                           | Layer       | Coverage                                                                         |
| ---------------------------------------------- | ----------- | -------------------------------------------------------------------------------- |
| `otel::init_no_endpoint`                       | unit        | `endpoint = None` returns `Ok` and produces a Registry                           |
| `otel::init_with_invalid_endpoint`             | unit        | invalid endpoint returns config error at startup, not at first request           |
| `traceparent_continued`                        | integration | inbound `traceparent` → outbound carries same trace_id, fresh span_id            |
| `span_attributes_complete`                     | integration | scrape exported spans (in-memory exporter), assert all required attrs present    |
| `retry_creates_child_spans`                    | integration | mock 503-then-200 → 2 child spans named `retry.attempt`                          |
| `fallback_creates_sibling_provider_complete`   | integration | primary fails, secondary succeeds → 2 sibling spans, second has `fallback_position=1` |
| `cancelled_span_marks_status`                  | integration | client disconnect mid-stream → root span status = ERROR                          |

---

## 5. Hot-reload config (Plan 04)

### 5.1 Reload triggers

```
Trigger                     Behavior
─────────────────────────────────────────────────────────────
SIGHUP (Unix)               Re-read config from --config path,
                            validate, swap. Idempotent.
POST /admin/reload          Same. Returns JSON summary or error.
                            Cross-platform (works on Windows).
POST /admin/reload          With `Content-Type: application/yaml`
  + body                    body — validate that body, swap on
                            success. Lets operators reload from
                            kubectl without re-mounting a configmap.
                            (Body form is opt-in; absent body
                            re-reads --config from disk.)
```

**No filesystem watching.** `notify`-crate-based watchers are flaky
across platforms. Triggering reload is the operator's choice —
`kill -HUP` from a sidecar, a CI script, or kubectl exec all work.

### 5.2 Reload algorithm

```
1. Snapshot current AppCore (immutable parts: providers, breaker_registry, otel)
2. Read new config (from path or body)
3. Validate new config against AppCore:
   a. Every route's upstream must reference an existing AppCore.providers entry
   b. Every auth key must be valid SHA-256 format
   c. Every retry/breaker/rate-limit policy must pass schema validation
   d. Logging filter must parse as a valid EnvFilter
4. If validation fails → return 4xx with errors, OLD snapshot stays in place
5. If validation passes:
   a. Rebuild AppSnapshot from new config + existing AppCore handles
   b. atomic_swap(state.snapshot, Arc::new(new_snapshot))
   c. Increment agent_shim_config_reloads_total counter
   d. Emit `tracing::info!(target = "agent_shim::reload", "config reloaded")`
   e. Return 200 with reload summary (route count, policy diffs)
6. In-flight requests retain their original Arc<AppSnapshot> clone — finished
   in their own time, on the OLD config
7. New requests after the swap pick up the NEW snapshot via
   state.snapshot.load_full()
```

**The atomic swap is one `arc_swap.store(new)` call.** No locks, no
torn reads. `load_full()` returns an `Arc<AppSnapshot>` stable for its
caller's lifetime.

### 5.3 What CANNOT reload

Validator rejects with `403 Forbidden` listing immutable fields:

| Field                                | Reason                                                | Workaround                       |
| ------------------------------------ | ----------------------------------------------------- | -------------------------------- |
| `server.bind`, `server.port`         | Listener bound at startup                             | Restart                          |
| `admin.bind`, `admin.port`           | Admin listener bound at startup                       | Restart                          |
| `upstreams.<name>.api_key`           | Provider client built once with embedded credentials  | Restart, sidecar credential rotation |
| `upstreams.<name>.base_url`          | Provider client lifecycle                             | Restart                          |
| `upstreams.<name>.type`              | Cannot swap a provider's protocol mid-flight          | Restart                          |
| `otel.endpoint`                      | Exporter pipeline initialized at startup              | Restart                          |
| Adding/removing `upstreams.*` entries | Provider construction not reload-safe in v0.5         | Restart                          |

**Adding a new route that references an existing upstream IS
reloadable.** Removing a route IS reloadable. Changing
`upstream_model` on an existing route IS reloadable. The rule:
anything that's "policy" reloads; anything that's "lifecycle" doesn't.

### 5.4 Breaker & rate-limit state semantics across reload

This is the subtlest part:

- **Breaker policy change** (e.g. `failure_threshold_pct: 50` → `30`):
  the next decision uses the new threshold; the existing window (last
  N events at the upstream) keeps accumulating. No state reset. If a
  breaker was already Open, it stays Open until its current cooldown
  elapses — the new policy doesn't shrink the cooldown.
- **Rate-limit bucket change** (e.g. `rate_per_sec: 100` → `200`): the
  **bucket is replaced**, not retuned. `governor`'s `RateLimiter`
  doesn't expose retuning, so on policy change we drop the old bucket
  and create a new one with full burst available. Documented as a
  known limitation: a reload that loosens limits effectively gives
  every key/upstream a fresh burst.

  **v0.5.0 implementation gap (deferred to v0.6):** the reload-applying
  task in `commands::serve::handle_reload` swaps `AppSnapshot` only;
  `LimiterRegistry` lives on the immutable `AppCore` and is therefore
  NOT rebuilt on policy change. In v0.5.0 a reload that changes any
  `rate_limit.*` field has no effect on the running registry — the
  existing buckets persist with their original quotas. This trades a
  spec-correct rate-limit reset against the larger refactor of moving
  `LimiterRegistry` behind an `ArcSwap` (or adding a `replace_policy`
  hook on the frozen-router side). Spec compliance review for Plan 04
  flagged this; the fix is tracked as a v0.6 follow-up. Operators who
  need a rate-limit policy change to take effect immediately must
  restart the process. The breaker side of §5.4 is fully implemented
  (test: `crates/gateway/tests/reload_in_flight.rs::breaker_state_survives_reload`).
- **Auth key set change** (add or remove keys): instant on next request.
- **`auth.required: false` → `true`**: instant. In-flight anonymous
  requests already hold their old snapshot, finish unblocked. New
  anonymous requests after swap → 401.

### 5.5 Reload-validation invariants (rules 11-14)

Added to `crates/config/src/validation.rs`:

```
Rule 11. Reload validation receives:
           - the candidate Config
           - the existing AppCore manifest (snapshot of providers
             known at startup)
         and rejects any config whose route references an unknown
         upstream.

Rule 12. Adding/removing/mutating upstreams.* entries during reload is
         forbidden. Validator compares candidate.upstreams keys to
         AppCore.providers keys; any mismatch →
         ConfigError::UpstreamSetChanged { added, removed }.

Rule 13. server.* and admin.* fields must equal AppCore.server_config
         and AppCore.admin_config exactly. Mismatch →
         ConfigError::ImmutableFieldChanged.

Rule 14. otel.endpoint must equal AppCore.otel_endpoint. Mismatch →
         same. (Other otel.* fields like sample_ratio CAN reload.)
```

These rules apply ONLY in reload context. Startup config validation
continues to use rules 1-10 from v0.4.

### 5.6 Reload response shape

**Success (HTTP 200):**

```json
{
  "ok": true,
  "applied": {
    "routes_total": 12,
    "routes_added": 1,
    "routes_removed": 0,
    "routes_modified": 2,
    "policies_changed": ["retry.gpt-4o", "breaker.openai/gpt-4o"],
    "auth_keys_added": 1,
    "auth_keys_removed": 0
  },
  "warnings": []
}
```

**Validation failure (HTTP 400):**

```json
{
  "ok": false,
  "errors": [
    "route[3]: upstream 'redis' not declared in upstreams.* (immutable in v0.5)",
    "auth.keys: key 'sha256:zzz' is not 64 lowercase hex chars"
  ]
}
```

**Immutable-field rejection (HTTP 403):**

```json
{
  "ok": false,
  "errors": [
    "server.port: 8787 → 9000 forbidden (restart required)"
  ]
}
```

### 5.7 Tests

| Test                                       | Layer                     | Coverage                                                                  |
| ------------------------------------------ | ------------------------- | ------------------------------------------------------------------------- |
| `reload::validate_immutable_fields`        | unit                      | rules 12, 13, 14 enforced                                                 |
| `reload::policy_diff`                      | unit                      | given old + new snapshot, summarize what changed                          |
| `reload_via_sighup`                        | integration `#[cfg(unix)]` | spawn process, send SIGHUP, scrape /metrics for `config_reloads_total==1` |
| `reload_via_admin_post`                    | integration               | POST /admin/reload (no body) → 200, counter increments                    |
| `reload_via_admin_post_with_body`          | integration               | POST /admin/reload with YAML body → 200, new policies in effect           |
| `reload_validation_failure_keeps_old`      | integration               | post invalid YAML → 4xx, next request uses old config                     |
| `reload_immutable_field_rejected`          | integration               | post YAML changing server.port → 403, listener still bound to original    |
| `in_flight_request_uses_old_snapshot`      | integration               | start streaming request, reload changing route, finish stream, assert old route used |
| `breaker_state_survives_reload`            | integration               | trip breaker open, reload (policy unchanged), assert breaker still open   |

### 5.8 ADR-0005

`docs/adr/0005-hot-reload-snapshot-model.md` (drafted in P05) records:

- Why arc-swap over RwLock or per-subsystem atomics.
- Why "policy reloads, lifecycle doesn't" as the boundary.
- Why breaker state survives reload but rate-limit buckets don't —
  asymmetry forced by `governor`'s API.
- Why no filesystem watcher — cross-platform reliability beats
  convenience.

---

## 6. Plans

Foundation-first ordering (Option A from brainstorm):

| Plan | Title                              | Foundations | User-visible payoff |
| ---- | ---------------------------------- | ----------- | ------------------- |
| P01  | Admin listener + healthz/readyz move | second axum::Router, AdminConfig, graceful-shutdown coordination, AppState pivot to Arc<AppCore> + Arc<ArcSwap<AppSnapshot>> | `/healthz`/`/readyz` on admin port |
| P02  | Prometheus metrics                  | metrics-rs facade, named metric registry, instrumentation injected into ResilientCaller, frontends, providers | `/metrics` endpoint with full metric set |
| P03  | OpenTelemetry tracing               | otel-tracing layer, traceparent ingestion/propagation, span model | exported spans when `otel.endpoint` set |
| P04  | Hot-reload config                   | rules 11-14, reload algorithm, in-flight semantics, breaker-state-survives-reload | SIGHUP + `POST /admin/reload` |
| P05  | Docs, ADR-0005, CHANGELOG, version bump | — | shipped 0.5.0 |

Each plan declares `core changes: NONE` and is verified per-commit
against the frozen-core invariant.

---

## 7. Testing & rollout

### 7.1 Test inventory

```
v0.4.1 baseline:        585 tests (verified via `cargo test --workspace`)
v0.5.0 target:          ~640 tests

Per-plan estimates:
  P01 (admin port):      +6
  P02 (Prometheus):      +18
  P03 (OpenTelemetry):   +15
  P04 (hot-reload):      +14
  P05 (docs/release):    +1   smoke test exercising all three pillars
```

### 7.2 New testing techniques

| Technique                         | Use                                                                            | Where     |
| --------------------------------- | ------------------------------------------------------------------------------ | --------- |
| `prometheus-parse` crate          | parse `/metrics` text and assert specific counter/histogram presence + value   | P02       |
| `tracing-test` (already present)  | capture tracing events; v0.5 adds span-tree assertions via custom subscriber    | P03       |
| In-memory OTel exporter           | hand-rolled `Exporter` impl pushing spans into `Mutex<Vec<SpanData>>`           | P03       |
| Process-level test harness        | spawn `agent-shim serve` subprocess, send SIGHUP, scrape `/metrics`             | P04 `#[cfg(unix)]` |
| `arc-swap` race test              | spawn 100 readers loading snapshots while 1 writer swaps; assert no torn reads  | P04       |

### 7.3 Performance gates

Phase 4 set tracing-overhead targets in `docs/architecture.md`. Phase 5
extends them:

```
Metrics overhead (per request):       < 5µs (counter increments only)
OTel span creation (export disabled): < 10µs (allocation + drop)
OTel span creation (export enabled):  < 25µs (allocation + queue push)
Reload validation latency:            < 100ms for a 50-route config
ArcSwap snapshot read:                < 100ns (lock-free Arc clone)
```

Measured via `criterion` benches in `crates/observability/benches/`.
CI fails on >20% regression. (Phase 4 set 10%; loosening to 20% for
Phase 5 because observability measurements are noisier.)

### 7.4 Documentation deliverables (P05)

| File                                              | New / modified | Content |
| ------------------------------------------------- | -------------- | ------- |
| `docs/observability.md`                           | new            | Operator guide: enabling /metrics, scrape config, OTel collector setup, hot-reload examples (kubectl, SIGHUP), example Grafana dashboard JSON |
| `docs/adr/0005-hot-reload-snapshot-model.md`      | new            | The reload-model ADR drafted in §5.8 |
| `docs/architecture.md`                            | modified       | + "Observability layer" section between Resilience layer and Capability Gate; perf overhead targets table |
| `docs/contributing.md`                            | modified       | + "How to add a metric" recipe, "How to add a span attribute" recipe |
| `docs/configuration.md`                           | modified       | + admin, otel, top-level reload-validation section |
| `README.md`                                       | modified       | + capability matrix v0.5 row; quick-start for `--admin.bind` |
| `CHANGELOG.md`                                    | modified       | `[0.5.0]` entry following v0.4.0 structure |
| `docs/providers/*.md` (5 files)                   | modified       | + "Observability behavior" subsection |

### 7.5 Rollout & version bump

```
P01 lands → 0.5.0-dev (no version bump until P05)
P02 lands → 0.5.0-dev
P03 lands → 0.5.0-dev
P04 lands → 0.5.0-dev
P05 lands:
  - bump 0.5.0-dev → 0.5.0
  - update CHANGELOG
  - update README capability matrix
  - merge to master via --no-ff
  - operator step (NOT in skill): tag v0.5.0, push
```

### 7.6 Backwards compatibility

**v0.4 configs continue to work unchanged.** New top-level blocks are
optional:

```yaml
# A v0.4 config still works in v0.5 — these blocks are absent → defaults apply:
admin:                   # absent → admin listener disabled (no /metrics, no /reload)
otel:                    # absent → no OTel layer; tracing still works
```

**No breaking changes to wire output.** Frontends are frozen. A v0.4
client gets byte-identical output from a v0.5 gateway running the same
routes/policies.

**No breaking changes to log format.** v0.4 tracing events under
`target = "agent_shim::resilience"` continue to fire with the same
field set. Phase 5 metrics + spans are *additional* — they don't
replace the existing structured logs.

### 7.7 Risks & mitigations

| Risk                                                | Mitigation                                                                                                            |
| --------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| OTel SDK pulls in heavy dependencies (~3MB binary growth) | Audit with `cargo bloat`; if >5MB total, gate OTel behind a feature flag (default-on)                                 |
| `governor::RateLimiter` reload semantics surprise users | Documented in §5.4, ADR-0005, and `docs/observability.md`; reload-summary response calls out reset buckets             |
| Admin port accidentally exposed to internet         | Default bind is loopback `127.0.0.1:9100`; absent `admin.bind` → admin listener disabled; documented prominently       |
| Hot-reload during high traffic causes contention    | arc-swap is wait-free for readers; one swap per reload; tests prove no read contention under 100-reader 1-writer load |
| `tracing-opentelemetry` version churn (pre-1.0)     | Pin exact version in Cargo.toml; ADR-0005 records the version chosen                                                  |

### 7.8 Definition of done

```
- [ ] All 5 plans (P01–P05) merged to feature branch
- [ ] All ~640 tests pass via cargo nextest run --workspace
- [ ] cargo clippy --workspace --all-targets -- -D warnings clean
- [ ] cargo deny check clean
- [ ] cargo fmt --all -- --check clean
- [ ] cargo bench passes baseline gates
- [ ] Frozen-core invariant: empty diff for crates/core, crates/frontends,
      crates/providers/src against v0.4.1 baseline
- [ ] docs/observability.md complete with copy-pasteable examples
- [ ] CHANGELOG [0.5.0] entry follows v0.4.0 structure
- [ ] README capability matrix v0.5 row present
- [ ] ADR-0005 committed and linked from docs/architecture.md
- [ ] manual smoke test: scrape /metrics, send traceparent through, reload a
      route via SIGHUP — all three work against the local binary
```

---

## 8. Appendix — Configuration reference (additions over v0.4)

### 8.1 New top-level `admin` block

```yaml
admin:
  bind: 127.0.0.1                # default; loopback recommended
  port: 9100                     # default
  # absent admin.* block → admin listener disabled (no /metrics, no /reload)
```

### 8.2 New top-level `otel` block

```yaml
otel:
  endpoint: http://otel-collector:4317   # absent → no export, spans local-only
  service_name: agent-shim               # default
  service_version: ${CARGO_PKG_VERSION}  # auto
  sample_ratio: 1.0                      # 0.0–1.0
  resource_attrs:
    deployment.environment: prod
    cloud.region: us-west-2
```

### 8.3 New `metrics` block (under existing `observability` ergonomics)

```yaml
# Optional; metrics auto-register when admin.bind is set.
metrics:
  histogram_buckets:
    request_duration_seconds: [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10]
    upstream_duration_seconds: [0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30]
```

Defaults match Prometheus conventions; operators can override per
histogram name.

### 8.4 Validation rules added

- **Rule 11** (reload only): unknown upstream reference
- **Rule 12** (reload only): upstreams.* set must match AppCore
- **Rule 13** (reload only): server.* / admin.* immutable
- **Rule 14** (reload only): otel.endpoint immutable
