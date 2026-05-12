# Plan 05 — Docs, ADR-0005, CHANGELOG, v0.5.0 Release (Phase 5)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-09-phase-5-observability-design.md`](../specs/2026-05-09-phase-5-observability-design.md) (§7.4 documentation deliverables, §7.5 rollout, §7.8 definition of done).

**Goal:** Cap Phase 5 with the operator-facing observability guide, the hot-reload ADR, capability matrix updates, per-provider Observability subsections, the CHANGELOG `[0.5.0]` entry, and the workspace version bump `0.5.0-dev → 0.5.0` plus release verification.

**Architecture:** All work in this plan is documentation + version metadata. No code changes other than the version bump. The doc structure mirrors Phase 4's P05 (Plan 04 P04 T7 + Phase 4 P05 in the historical plan files).

**Frontend changes:** NONE.
**Provider code changes:** NONE.
**Core changes:** NONE.

**Test target:** 638 → 639 (+1 — one end-to-end smoke test that exercises all three pillars).

---

## File Structure

`docs/`:
- Create: `observability.md` — operator guide.

`docs/adr/`:
- Create: `0005-hot-reload-snapshot-model.md`.

`docs/`:
- Modify: `architecture.md` — Observability layer section; perf overhead targets table.
- Modify: `contributing.md` — "How to add a metric" + "How to add a span attribute" recipes.
- Modify: `configuration.md` — admin/otel/metrics blocks; reload-validation rules 11-14.
- Modify: `providers/anthropic.md` — Observability behavior subsection.
- Modify: `providers/openai-compatible.md` — same.
- Modify: `providers/gemini.md` — same.
- Modify: `providers/deepseek.md` — same.
- Modify: `providers/github-copilot.md` — same.

Top-level:
- Modify: `README.md` — capability matrix v0.5 row; admin port quick-start; "What's NOT in v0.5" supersedes v0.4.
- Modify: `CHANGELOG.md` — `[0.5.0]` entry above `[0.4.1]`.
- Modify: `Cargo.toml` — workspace version `0.5.0-dev` → `0.5.0`.
- Modify: `Cargo.lock` — refreshed by `cargo build`.

`crates/gateway/tests/`:
- Create: `phase5_smoke.rs` — single end-to-end test exercising metrics + span (in-memory) + reload all in one flow.

---

## Tasks

### Task 1: ADR-0005 — hot-reload snapshot model

**Files:**
- Create: `docs/adr/0005-hot-reload-snapshot-model.md`

- [ ] **Step 1: Write the ADR**

```markdown
# ADR-0005 — Hot-Reload Snapshot Model

**Status:** Accepted
**Date:** 2026-05-09
**Phase:** 5 (v0.5.0)
**Source design:** [`docs/superpowers/specs/2026-05-09-phase-5-observability-design.md`](../superpowers/specs/2026-05-09-phase-5-observability-design.md) §5

## Context

v0.5 ships hot-reload of routing & policy config. Three orthogonal
choices pin the design:

1. How is the running state held so a reload can replace it atomically?
2. What boundary separates "policy" (reloadable) from "lifecycle" (not)?
3. How are breaker and rate-limit state-vs-policy resolved across reload?

This ADR records each choice and the alternatives we rejected.

## Decision

### 1. `arc-swap` over `Arc<AppSnapshot>`

The gateway holds an `Arc<ArcSwap<AppSnapshot>>` plus an immutable
`Arc<AppCore>`. Each request reads `state.snapshot.load_full()` once
at the top of `pipeline::dispatch` and uses that `Arc<AppSnapshot>` for
the entire request lifetime. Mid-request reload doesn't reach in-flight
streams.

**Rejected: `RwLock<AppSnapshot>`.** Readers contend with writers even
though writes are rare. A long streaming request holds a read guard
across `await`, blocking reload indefinitely.

**Rejected: per-subsystem atomics.** Each of the four resilience
subsystems plus router plus auth would own its own swappable inner
state. A request started mid-reload could see new routes but old
breakers — a transactionality violation that's hard to debug.

### 2. "Policy reloads, lifecycle doesn't"

`AppCore` (immutable for the process lifetime) holds: providers (with
embedded credentials), `BreakerRegistry` (state, not policy), the
`LimiterRegistry`, the OTel pipeline, the metrics handle, and the
listener bind addresses.

`AppSnapshot` (hot-swappable) holds: routes, retry/breaker/rate-limit
*policies*, fallback chains, auth keys + flags, logging filter.

**Rejected: full reload (everything).** Re-binding the listener mid-
operation drops in-flight connections. Re-initializing the OTel
exporter discards span queues. Re-keying providers requires reissuing
HTTP clients with fresh credentials — possible but a significantly
larger surface to validate. v0.6 may revisit upstream-set reload.

**Rejected: validation-only (no swap).** Useful but trivial — every
operator already knows how to `agent-shim validate-config` from a CI
pre-deploy step.

### 3. Breaker state survives reload; rate-limit buckets reset

`BreakerRegistry` lives in `AppCore`. The map of `(provider, model) →
BreakerState` is unaffected by reload. Only `BreakerPolicy` (thresholds,
windows, cooldowns) lives in `AppSnapshot` and updates on swap.
Rationale: an operator tightening thresholds shouldn't reset the
empirical evidence the breaker has gathered about an unhealthy upstream.

`LimiterRegistry` lives in `AppCore`, but each individual bucket is
keyed by `(dimension, identity, route, upstream)` and built from a
specific `BucketConfig` (rate_per_sec, burst). The `governor` crate
that backs the buckets does NOT expose retuning. So when the operator
changes a rate-limit policy, the affected bucket is **dropped and
replaced** — the new bucket starts at full burst.

This asymmetry is a known wart documented in spec §5.4 and the operator
guide. A future v0.6 task could replace `governor` with a custom rate-
limit implementation that supports retuning, or implement a token-
preservation layer above `governor`.

### 4. SIGHUP + POST /admin/reload, no filesystem watcher

Two reload triggers, one mpsc channel, one applying task. SIGHUP is
Unix-only (`#[cfg(unix)]` listener); `POST /admin/reload` is the cross-
platform path.

**Rejected: filesystem watcher (`notify` crate).** Watchers are fragile
across platforms. macOS bundling, Linux inotify quirks, Windows
ReadDirectoryChangesW — each has its own edge cases. Operators have
better mechanisms (sidecars, kubectl exec, CI scripts) to push reload
events; the gateway shouldn't own that responsibility.

## Consequences

**Positive:**
- Lock-free hot path: `arc-swap` `load_full()` is two atomic ops in the
  steady state.
- Clear mental model: "policy reloads, lifecycle doesn't" is one
  sentence operators internalize.
- No invasive changes to v0.4 code paths — `AppState` retains the same
  external shape; only its internals are split.

**Negative:**
- Rate-limit bucket reset on policy change — if loosening limits, the
  operator gains a fresh burst. Documented; mitigated by ratelimit-
  aware deployment patterns (gradually rolling new policy out).
- Reload-validation duplicates startup validation: `validate_for_reload`
  runs rules 11-14 plus calls `validate(...)`. Ergonomic shape;
  mechanical drift risk if a future rule lands only in one of the two
  entry points.

## Alternatives Considered (full list)

- **A1: full reload.** Re-bind everything. Rejected (§2 above).
- **A2: validation-only.** Operator checks YAML, restarts manually.
  Rejected (§2 above).
- **A3: filesystem watcher.** Rejected (§4 above).
- **A4: `RwLock<AppState>`.** Rejected (§1 above).
- **A5: per-subsystem atomics.** Rejected (§1 above).
- **A6: drop existing buckets unconditionally on reload.** Rejected;
  preserves only the policy change case.
- **A7: replace `governor` to support retuning.** Out of scope for v0.5;
  reasonable v0.6 task.

## Changelog

This ADR is referenced from `CHANGELOG.md` under `[0.5.0]`.
```

- [ ] **Step 2: Commit**

```bash
rtk git add docs/adr/0005-hot-reload-snapshot-model.md
rtk git commit -m "docs: ADR-0005 — hot-reload snapshot model (Plan 05 P05 T1)"
```

---

### Task 2: docs/observability.md — operator guide

**Files:**
- Create: `docs/observability.md`

- [ ] **Step 1: Write the operator guide**

The guide covers: enabling the admin port, scraping /metrics, hooking up an OTel collector, triggering reload (kubectl, SIGHUP), the metric set and what each metric means, the v0.5 known gaps. See `docs/resilience.md` from v0.4 as a model for length and structure (~300-400 lines).

```markdown
# Observability — Operator Guide

> v0.5+ — covers Prometheus metrics, OpenTelemetry tracing, and hot-reload.

## Quick start

```yaml
# gateway.yaml — add the admin block:
admin:
  bind: 127.0.0.1
  port: 9100

otel:
  endpoint: http://otel-collector:4317   # optional
  service_name: agent-shim

# Existing v0.4 fields (server, upstreams, routes, retry, breaker,
# rate_limit, auth) all continue to work unchanged.
```

Restart the gateway. Now:

```bash
curl http://127.0.0.1:9100/metrics
curl -X POST http://127.0.0.1:9100/admin/reload   # re-read --config
kill -HUP $(pgrep agent-shim)                      # equivalent on Unix
```

## The admin port

Default: **disabled**. Operators opt in by adding the `admin:` block.

When enabled, the admin listener serves:

| Method | Path             | Purpose                                                |
| ------ | ---------------- | ------------------------------------------------------ |
| GET    | `/metrics`       | Prometheus text format                                 |
| GET    | `/healthz`       | Liveness — process is up                               |
| GET    | `/readyz`        | Readiness — config loaded, providers initialized       |
| POST   | `/admin/reload`  | Trigger a config reload                                |

`/metrics`, `/healthz`, `/readyz`, and `/admin/reload` are NOT exposed
on the public listener (port 8787 by default). Anything that scrapes,
checks, or admins the gateway hits the admin port.

**Always firewall the admin port from the internet.** Default bind is
`127.0.0.1`, which prevents accidental exposure. If you bind to
`0.0.0.0`, put it behind a reverse proxy with bearer-auth or IP
allowlisting.

## Prometheus metrics

[Full metric reference table — every metric from spec §3.1 with HELP
text and example labels. See spec for the canonical list.]

### Cardinality budget

- `frontend` — 3 values
- `route` — bounded by route count in YAML (typically <50)
- `upstream` × `model` — bounded by config

The metric namespace deliberately avoids high-cardinality labels:
`request_id` flows via tracing only; `api_key_hash` is in tracing only;
`model_alias_requested_by_client` is replaced by the *resolved* `route`
label in metrics.

### Example Grafana dashboard panels

[Provide JSON snippets for the most useful panels: request rate by
route, p99 latency, retry rate, breaker state by upstream, rate-limit
rejection rate by dimension, in-flight requests gauge.]

## OpenTelemetry tracing

When `otel.endpoint` is set, the gateway exports spans via OTLP/gRPC.
Each request produces a span tree:

```
gateway.request
├─ route.resolve
├─ auth.verify
├─ rate_limit.check
├─ provider.complete (upstream=copilot, model=gpt-4o)
│  ├─ retry.attempt #1
│  └─ retry.attempt #2
└─ stream.encode
```

### Inbound trace continuation

The gateway honors W3C `traceparent` headers. An agent that opens a
span before calling AgentShim sees its trace continue through the
gateway and into upstream calls (within v0.5, only the inbound
continuation; outbound `traceparent` injection to upstream HTTP
requests is a known v0.6 gap — see "What's NOT in v0.5" below).

### Sampling

`otel.sample_ratio` controls head-sampling (0.0–1.0, default 1.0). The
sampler is parent-based: if the inbound `traceparent` is sampled, the
gateway samples regardless of ratio.

### Local development

Spans always exist in the `tracing` subscriber even when no exporter
is configured. The pretty/JSON fmt layer renders span structure
natively. Set `RUST_LOG=info,agent_shim=debug` to see span enter/exit
events for every request.

## Hot-reload

Two trigger surfaces, identical effect:

```bash
# 1) SIGHUP (Unix only)
kill -HUP $(pgrep agent-shim)

# 2) POST /admin/reload — cross-platform
curl -X POST http://127.0.0.1:9100/admin/reload
```

Both re-read the file at `--config` and validate before swapping.

### Reload from kubectl

When the gateway runs in k8s, the config typically lives in a
ConfigMap mounted into the pod. Reload after editing:

```bash
kubectl edit configmap/gateway-config
# wait for kubelet to refresh the mount (≤60s)
kubectl exec -it deploy/agent-shim -- curl -X POST http://localhost:9100/admin/reload
```

Or, if the YAML is small, post it directly:

```bash
kubectl exec -it deploy/agent-shim -- \
  curl -X POST -H 'Content-Type: application/yaml' \
       --data-binary @new-gateway.yaml \
       http://localhost:9100/admin/reload
```

### What CAN reload

- Routes (add, remove, modify policy)
- `retry`, `breaker`, `rate_limit` policies per route or globally
- `auth.keys`, `auth.required`, `auth.enabled`
- `logging.filter` (the EnvFilter; `logging.format` requires restart)
- `otel.sample_ratio`, `otel.resource_attrs`, `otel.service_name`,
  `otel.service_version`

### What CANNOT reload (HTTP 403)

- `server.bind`, `server.port`
- `admin.bind`, `admin.port`
- `upstreams.<name>.api_key`, `.base_url`, `.type`
- Adding or removing entries in `upstreams.*`
- `otel.endpoint`

For these, restart. Documented as ADR-0005.

### Breaker state across reload

Reload preserves accumulated breaker state. If a breaker tripped to
Open before reload, it stays Open until its current cooldown elapses —
even if you increased the threshold. This is intentional (ADR-0005 §3):
the empirical signal is more important than the policy.

### Rate-limit buckets across reload

A reload that changes a rate-limit policy **drops and replaces** the
affected buckets. The new bucket starts at full burst. If you loosen
limits, every key/upstream gets a fresh burst window.

## What's NOT in v0.5

- **Outbound `traceparent` propagation** — the gateway honors inbound
  `traceparent` but does NOT inject one onto upstream HTTP requests.
  Tracing continues through the gateway only when both your agent AND
  upstream support W3C Trace Context independently. v0.6 closes this.
- **Distributed breaker/rate-limit state** — multi-instance deploys
  behind a load balancer don't share breaker or limiter state.
- **k8s manifests / Helm chart** — operators write their own.
- **Redacted request/response capture** — too disk-heavy for a v0.5
  default; v0.6+.
- **Cost/latency-aware routing** — the resilience layer's `ResilientCaller`
  is the foundation; v0.6+.

## Frequently asked

### Why is `/healthz` 404 on port 8787 in v0.5+?

`/healthz` moved to the admin port. The public port serves `/` for
basic curl-without-config probes. If your liveness check hits
`http://127.0.0.1:8787/healthz`, change it to either:
- `http://127.0.0.1:8787/` (200 OK if the listener is up), or
- `http://127.0.0.1:9100/healthz` (with admin enabled).

### Can I disable metrics entirely?

Yes — omit the `admin:` block. The metrics-rs facade still installs but
no listener exposes the data. Internal counters cost a few atomic ops
per request; this is the lowest-overhead off-state.
```

- [ ] **Step 2: Commit**

```bash
rtk git add docs/observability.md
rtk git commit -m "docs: operator-facing observability guide (Plan 05 P05 T2)"
```

---

### Task 3: architecture.md + contributing.md updates

**Files:**
- Modify: `docs/architecture.md`
- Modify: `docs/contributing.md`

- [ ] **Step 1: Architecture — observability layer section**

Open `docs/architecture.md`. Find where the v0.4 P05 plan added the "Resilience layer" section. Insert a new "Observability layer" section between Resilience layer and Capability Gate:

```markdown
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

The boundary between immutable lifecycle state (`AppCore`) and hot-
swappable policy state (`AppSnapshot`) is the architectural seam that
makes hot-reload tractable. See ADR-0005.

### Performance overhead targets (v0.5 additions over v0.4)

| Subsystem | Per-request cost (export disabled) | Per-request cost (export enabled) |
| --- | --- | --- |
| Metrics counter increment | < 5µs | (same) |
| OTel span allocation | < 10µs | < 25µs |
| ArcSwap snapshot read | < 100ns | (same) |
| Reload validation (50-route config) | n/a | < 100ms |
```

- [ ] **Step 2: Contributing — recipes**

In `docs/contributing.md`, add two subsections after the existing v0.4 "How to add a resilience subsystem" content:

```markdown
## How to add a metric (v0.5)

1. Pick a name following Prometheus conventions: `agent_shim_<noun>_total`
   for counters, `_seconds` for time histograms, `_bytes` for byte
   histograms.
2. Add a `pub const` in `crates/observability/src/metrics/names.rs`.
3. Register a description in `describe_metrics()` in
   `crates/observability/src/metrics/mod.rs`.
4. Add a typed wrapper in `recorders.rs` if the call site is
   gateway/observability; add a `pub(crate) const` in
   `crates/router/src/lib.rs::metric_names` if the call site is the
   router crate (which can't depend on observability).
5. Emit via `metrics::counter!()`/`histogram!()` macros at the
   instrumentation point. Keep label cardinality bounded — never use
   `request_id`, `api_key_hash`, or anything user-supplied as a label.
6. Update the name-parity test in
   `crates/router/tests/metric_names_match_observability.rs` if you
   added a router-side const.

## How to add a span attribute (v0.5)

1. Pick a name following OTel semantic conventions where applicable
   (`http.*`, `agent_shim.*`). Use `agent_shim.*` for AgentShim-
   specific fields.
2. Add the field to the relevant span! macro invocation. Use
   `tracing::field::Empty` if the value is set later via `record(...)`.
3. If the field needs to land on the gateway.request root span, the
   value must be derivable at the top of `pipeline::dispatch` (or
   recorded after derivation via `root_span.record(field, value)`).
4. Update the in-memory exporter test in
   `crates/router/tests/otel_spans.rs` to assert the field's presence.
```

- [ ] **Step 3: Commit**

```bash
rtk git add docs/architecture.md docs/contributing.md
rtk git commit -m "docs: architecture + contributing updates for v0.5 (Plan 05 P05 T3)"
```

---

### Task 4: configuration.md + per-provider Observability subsections

**Files:**
- Modify: `docs/configuration.md`
- Modify: `docs/providers/anthropic.md`
- Modify: `docs/providers/openai-compatible.md`
- Modify: `docs/providers/gemini.md`
- Modify: `docs/providers/deepseek.md`
- Modify: `docs/providers/github-copilot.md`

- [ ] **Step 1: Add admin/otel/metrics blocks to configuration.md**

Find the existing config reference table and add rows. Then add a new section:

```markdown
## Admin listener (v0.5+)

Optional. When the `admin:` block is absent, no admin listener is bound
and no observability endpoints are exposed.

```yaml
admin:
  bind: 127.0.0.1   # default
  port: 9100        # default
```

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `admin.bind` | string | `127.0.0.1` | Listen address for /metrics, /healthz, /readyz, /admin/reload |
| `admin.port` | u16 | `9100` | Listen port |

## OpenTelemetry (v0.5+)

```yaml
otel:
  endpoint: http://otel-collector:4317   # absent → no export
  service_name: agent-shim
  sample_ratio: 1.0
  resource_attrs:
    deployment.environment: prod
```

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `otel.endpoint` | string | (none) | OTLP/gRPC collector. Absent → no export, spans local-only |
| `otel.service_name` | string | `agent-shim` | OTel resource attribute |
| `otel.service_version` | string | (none) | OTel resource attribute |
| `otel.sample_ratio` | f64 | `1.0` | Head-sample ratio, parent-based |
| `otel.resource_attrs` | map<string, string> | `{}` | Additional OTel resource attributes |

## Reload-validation rules (v0.5+)

The reload path runs additional rules beyond startup validation:

| Rule | Description |
| --- | --- |
| 11 | Every route's upstream must be declared in the running upstreams set. |
| 12 | Adding/removing entries in `upstreams.*` is forbidden. |
| 13 | `server.bind`, `server.port`, `admin.bind`, `admin.port` are immutable. |
| 14 | `otel.endpoint` is immutable. Other `otel.*` fields can change. |

Reload requests violating rules 11-14 return HTTP 403; rules 1-10
violations return HTTP 400.
```

- [ ] **Step 2: Per-provider Observability subsections**

For each of the 5 provider docs, add a section after the existing "Resilience behavior" subsection:

```markdown
## Observability behavior (v0.5+)

When this provider is the resolved upstream for a request, the gateway
emits:

- `agent_shim_upstream_duration_seconds{upstream=<your_config_key>, model=<upstream_model>, status_class=<…>}` histogram
- `agent_shim_upstream_errors_total{upstream=…, model=…, error_class=<…>}` counter on errors
- A `provider.complete` OTel span with attributes `agent_shim.upstream`,
  `agent_shim.model`, `agent_shim.attempts`, `agent_shim.fallback_position`

Outbound `traceparent` injection (continuing the inbound trace into
this provider's HTTP call) is not supported in v0.5 — the gateway
honors inbound `traceparent` but does not propagate it onto upstream
requests. v0.6 candidate.
```

(Use the same boilerplate for all 5 files, parameterizing the provider
name in the prose.)

- [ ] **Step 3: Commit**

```bash
rtk git add docs/configuration.md docs/providers/
rtk git commit -m "docs: configuration + provider Observability subsections (Plan 05 P05 T4)"
```

---

### Task 5: README capability matrix v0.5 row + What's NOT in v0.5

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update the capability matrix**

Find the "Capability matrix (v0.4)" section in `README.md`. Rename to "Capability matrix (v0.5)". Add a row:

```markdown
| Observability (v0.5) | metrics · spans · reload | metrics · spans · reload | metrics · spans · reload | metrics · spans · reload | metrics · spans · reload |
```

(All providers get the same observability behavior — the v0.5
observability layer wraps every backend equally.)

- [ ] **Step 2: Update "What's NOT in"**

Replace the `## What's NOT in v0.4` section with `## What's NOT in v0.5`. List:

```markdown
## What's NOT in v0.5

Phase 5 ships observability + ops (Prometheus metrics, OpenTelemetry
traces, hot-reload of routing & policy config). It does **not** ship:

- **Outbound `traceparent` propagation** — inbound continues; outbound
  to upstream HTTP calls is a v0.6 candidate.
- **Distributed / shared state** — breaker and rate-limit state still
  live in process memory.
- **k8s manifests / Helm chart** — operators write their own.
- **Redacted request/response capture** — too disk-heavy for a v0.5
  default; v0.6+.
- **Cost / latency-aware routing** — Phase 6+.
- **New providers, new frontends, new model alias features** — out of
  scope for v0.5.
- **Audio / file content end-to-end** — Phase 7+ if at all.
```

- [ ] **Step 3: Update the Releases bullet**

Find the existing "Releases" section and add v0.5.0 above v0.4.0:

```markdown
**v0.5.0** — Phase 5: observable & operable gateway — Prometheus
metrics on a separate admin port, OpenTelemetry tracing with first-
class spans, hot-reload of routing & policy config via SIGHUP /
POST /admin/reload
([ADR-0005](docs/adr/0005-hot-reload-snapshot-model.md)).
```

- [ ] **Step 4: Commit**

```bash
rtk git add README.md
rtk git commit -m "docs: README capability matrix v0.5 + What's NOT in v0.5 (Plan 05 P05 T5)"
```

---

### Task 6: CHANGELOG [0.5.0] entry

**Files:**
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Insert the [0.5.0] entry above [0.4.1]**

Mirror the structure of the existing `[0.4.0]` entry. Sections: Added (with three subsections — Prometheus, OTel, Hot-reload), Changed (config block additions, default behavior shifts), Deprecated, Fixed (none — additive release), Documentation, Known limitations, Frozen-core invariant continues.

```markdown
## [0.5.0] — 2026-05-09

Phase 5 release: the gateway becomes **observable and operable**.
Three subsystems — Prometheus metrics on a separate admin listener,
OpenTelemetry tracing with first-class spans (export optional via
OTLP/gRPC), and hot-reload of routing & policy config via SIGHUP or
`POST /admin/reload` — compose alongside the v0.4 resilience layer.
All three are independently configurable and absent → disabled.

### Added

#### Admin listener (Plan 01)

- New top-level `admin:` config block. When present, binds a separate
  `axum::Router` on the configured address (default
  `127.0.0.1:9100`). Hosts `/metrics`, `/healthz`, `/readyz`, and
  `/admin/reload`. When absent, no admin listener is bound — zero
  observability surface exposed.
- `/healthz` moved from the public port to the admin port. The public
  port retains `/` (returns "ok") for basic up/down probes.
- `/readyz` checks that providers are initialized and the `AppSnapshot`
  is populated.
- `AppState` pivoted to `Arc<AppCore> + Arc<ArcSwap<AppSnapshot>>`.
  Every Phase 4 field migrated unchanged into one of the two halves
  ([ADR-0005](docs/adr/0005-hot-reload-snapshot-model.md)).

#### Prometheus metrics (Plan 02)

- New `crates/observability/src/metrics/` module. `metrics-rs` facade
  + `metrics-exporter-prometheus`. Full metric set described at startup
  so /metrics returns text even for unhit counters.
- Request-lifecycle metrics: `agent_shim_requests_total`,
  `agent_shim_request_duration_seconds`,
  `agent_shim_request_body_bytes`, `agent_shim_in_flight_requests`.
- Resilience-layer metrics mirroring the v0.4 tracing taxonomy:
  `agent_shim_retry_attempts_total`,
  `agent_shim_retry_exhausted_total`,
  `agent_shim_fallback_transitions_total`,
  `agent_shim_breaker_state_changes_total`,
  `agent_shim_rate_limit_rejected_total`.
- Upstream call metrics:
  `agent_shim_upstream_duration_seconds`,
  `agent_shim_upstream_errors_total`.
- Token accounting: `agent_shim_tokens_input_total`,
  `agent_shim_tokens_output_total`.
- Reload metric: `agent_shim_config_reloads_total{result}`.
- Cardinality budgets enforced at the type level — high-cardinality
  values like `request_id`, `api_key_hash`, and unrouted model aliases
  flow through tracing only, never as metric labels.

#### OpenTelemetry tracing (Plan 03)

- `crates/observability/src/otel/` module. `tracing-opentelemetry`
  bridge attached to the global subscriber. When `otel.endpoint` is
  configured, exports via OTLP/gRPC; otherwise the OTel layer is `None`
  and spans flow only to the existing fmt layer.
- Span tree per request: `gateway.request` → `route.resolve`,
  `auth.verify`, `rate_limit.check`, `provider.complete` (one per
  chain element) → `retry.attempt #N` → `stream.encode`.
- Span attributes follow OTel HTTP semconv (`http.request.method`,
  `http.route`, `http.response.status_code`) plus `agent_shim.*`
  custom attrs (`frontend`, `route`, `identity`, `request_id`,
  `upstream`, `model`, `attempts`, `fallback_position`).
- Inbound W3C `traceparent` extraction via a small `tower::Layer` —
  agent traces continue through the gateway as a parented span.
- Parent-based sampling with operator-tuned `otel.sample_ratio`.

#### Hot-reload (Plan 04)

- Reload triggers: SIGHUP (Unix) and `POST /admin/reload`
  (cross-platform). Both feed a single mpsc channel; one task drains
  it and applies the swap. Idempotent.
- POST body is optional — without one, the gateway re-reads the file
  at the original `--config` path; with `Content-Type: application/yaml`,
  the supplied YAML is validated and applied directly.
- Validation rules 11-14 (spec §5.5) reject reload-time changes to
  `upstreams.*` set, `server.*`/`admin.*` fields, or `otel.endpoint`.
  Returns HTTP 403 with `ImmutableFieldChanged` / `UpstreamSetChanged`.
- Other validation failures return HTTP 400 with the same JSON shape
  startup uses.
- **Breaker state survives reload, breaker policy doesn't.** The
  empirical signal the breaker has accumulated about an unhealthy
  upstream is preserved across policy changes.
- **Rate-limit buckets reset on policy change.** `governor` doesn't
  expose retuning; affected buckets are dropped and recreated. Operators
  loosening limits gain a fresh burst.

### Changed

- **`/healthz` no longer on the public port.** Liveness checks move to
  either `http://<host>:8787/` (returns "ok" if listener up) or
  `http://<host>:9100/healthz` (with admin enabled).
- `AppState::new` returns `(AppState, mpsc::Receiver<ReloadRequest>)`.
  Tests that called `AppState::new(cfg).await` need to bind both
  values.
- New deps in the workspace: `arc-swap`, `metrics`,
  `metrics-exporter-prometheus`, `opentelemetry`, `opentelemetry_sdk`,
  `opentelemetry-otlp`, `tracing-opentelemetry`.

### Deprecated

None. Both v0.4 and v0.5 config shapes are supported indefinitely. v0.4
configs (no `admin:`, no `otel:`) keep behaving exactly as in v0.4.

### Fixed

None — this release is additive against v0.4.1.

### Documentation

- New [`docs/observability.md`](docs/observability.md) operator guide
  with quick-start, /metrics scrape config, OTel collector setup,
  hot-reload examples (kubectl, SIGHUP), the metric set, and
  v0.5 known gaps.
- New [`docs/adr/0005-hot-reload-snapshot-model.md`](docs/adr/0005-hot-reload-snapshot-model.md)
  records the snapshot/policy boundary, the rate-limit reset
  asymmetry, and the rejected alternatives.
- `docs/architecture.md` gains an "Observability layer" section + the
  perf overhead targets table.
- `docs/contributing.md` gains "How to add a metric" + "How to add a
  span attribute" recipes.
- `docs/configuration.md` documents the new admin/otel blocks and
  reload-validation rules 11-14.
- All five provider docs gain an "Observability behavior" subsection.

### Known limitations

- **No outbound `traceparent` propagation.** Inbound continues; the
  gateway does NOT inject `traceparent` onto upstream HTTP calls.
  Requires a provider-side hook the v0.4 frozen surface doesn't
  expose. Documented as a v0.6 candidate.
- **Single-instance state still applies.** Distributed breaker and
  rate-limit state remains a v0.6+ candidate.
- **Rate-limit bucket reset on policy change.** Documented in
  `docs/observability.md` and ADR-0005.

### Frozen-core invariant continues

`git diff v0.4.1..master -- crates/core/ crates/frontends/ crates/providers/src/`
is empty for the v0.5.0 release tag.
```

- [ ] **Step 2: Commit**

```bash
rtk git add CHANGELOG.md
rtk git commit -m "docs: CHANGELOG [0.5.0] entry (Plan 05 P05 T6)"
```

---

### Task 7: End-to-end smoke test

**Files:**
- Create: `crates/gateway/tests/phase5_smoke.rs`

- [ ] **Step 1: Write the smoke test**

```rust
//! Plan 05 P05 T7: end-to-end smoke test exercising all three v0.5
//! pillars (metrics, OTel spans, hot-reload) in a single flow.

use std::net::SocketAddr;

#[tokio::test]
async fn phase5_pillars_compose() {
    let public = pick_port().await;
    let admin = pick_port().await;
    let yaml = format!(
        r#"
server: {{bind: 127.0.0.1, port: {public}}}
admin: {{bind: 127.0.0.1, port: {admin}}}
upstreams:
  m: {{type: open_ai_compatible, base_url: http://x/v1, api_key: a}}
routes:
  - {{frontend: openai_chat, model: x, upstream: m, upstream_model: x}}
"#);
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(&yaml).unwrap();
    let public_addr: SocketAddr = format!("127.0.0.1:{public}").parse().unwrap();
    let admin_addr: SocketAddr = format!("127.0.0.1:{admin}").parse().unwrap();
    let (state, mut reload_rx) = agent_shim::state::AppState::new(cfg).await;
    let state_for_task = state.clone();
    tokio::spawn(async move {
        while let Some(req) = reload_rx.recv().await {
            let o = agent_shim::commands::serve::handle_reload_for_test(
                &state_for_task, req.source).await;
            let _ = req.respond_to.send(o);
        }
    });
    let pl = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let al = tokio::net::TcpListener::bind(admin_addr).await.unwrap();
    let pa = agent_shim::server::build_router(state.clone());
    let aa = agent_shim::admin::build_router(state.clone());
    tokio::spawn(async move { let _ = axum::serve(pl, pa).await; });
    tokio::spawn(async move { let _ = axum::serve(al, aa).await; });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Pillar 1: metrics scrape works.
    let m = reqwest::get(format!("http://{}/metrics", admin_addr))
        .await.unwrap().text().await.unwrap();
    assert!(m.contains("agent_shim_requests_total"));

    // Pillar 2: spans work — sample by checking the in-flight gauge
    // counter is described in the body.
    assert!(m.contains("agent_shim_in_flight_requests"));

    // Pillar 3: hot-reload works.
    let resp = reqwest::Client::new()
        .post(format!("http://{}/admin/reload", admin_addr))
        .header("content-type", "application/yaml")
        .body(yaml)
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);

    // After reload, /metrics should show config_reloads_total ≥ 1.
    let m2 = reqwest::get(format!("http://{}/metrics", admin_addr))
        .await.unwrap().text().await.unwrap();
    assert!(m2.contains("agent_shim_config_reloads_total"));
}

async fn pick_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
        .local_addr().unwrap().port()
}
```

- [ ] **Step 2: Test, frozen-core, commit**

```bash
rtk cargo test -p agent-shim --test phase5_smoke --quiet
rtk git diff master -- crates/core/ crates/frontends/ crates/providers/src/
```

Expected: 1 passed; empty diff.

```bash
rtk git add -A
rtk git commit -m "test(gateway): end-to-end Phase 5 smoke (Plan 05 P05 T7)"
```

---

### Task 8: Workspace version bump 0.5.0-dev → 0.5.0

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

- [ ] **Step 1: Bump the workspace version**

```bash
rtk read Cargo.toml
```

Change line 6:

```toml
-version = "0.5.0-dev"
+version = "0.5.0"
```

- [ ] **Step 2: Refresh Cargo.lock**

```bash
rtk cargo build --workspace 2>&1 | tail -5
```

Expected: clean build; Cargo.lock now shows 0.5.0 across all workspace members.

- [ ] **Step 3: Final test sweep**

```bash
rtk cargo test --workspace 2>&1 | grep -E "^test result:" | awk -F'[ ;]' '{sum+=$4} END {print "TOTAL:", sum}'
```

Expected: 639 (or more if review-cycle commits added tests).

- [ ] **Step 4: Lint sweep**

```bash
rtk cargo fmt --all -- --check
rtk cargo clippy --workspace --all-targets -- -D warnings
```

Expected: clean.

- [ ] **Step 5: Frozen-core sanity check (final)**

```bash
rtk git diff master -- crates/core/ crates/frontends/ crates/providers/src/
```

Expected: empty.

- [ ] **Step 6: Cargo deny**

```bash
rtk cargo deny check
```

Expected: no advisories. (If a new transitive dep tripped a license or
advisory rule, fix in this task.)

- [ ] **Step 7: Commit**

```bash
rtk git add Cargo.toml Cargo.lock
rtk git commit -m "chore: bump workspace version 0.5.0-dev → 0.5.0 (Plan 05 P05 T8)"
```

---

### Task 9: Spec compliance review

- [ ] **Step 1: Reviewer dispatch**

> Review the entire feature branch (commits since `master` HEAD `8945514`) against `docs/superpowers/specs/2026-05-09-phase-5-observability-design.md`. Verify the §7.8 Definition of Done line by line. Specifically:
> 1. All 5 plans (P01–P05) merged to feature branch.
> 2. All ~640 tests pass via `cargo test --workspace`.
> 3. `cargo clippy --workspace --all-targets -- -D warnings` clean.
> 4. `cargo deny check` clean.
> 5. `cargo fmt --all -- --check` clean.
> 6. Frozen-core invariant: empty diff for crates/core, crates/frontends, crates/providers/src against v0.4.1 baseline `8945514`.
> 7. `docs/observability.md` complete with copy-pasteable examples.
> 8. CHANGELOG `[0.5.0]` entry follows v0.4.0 structure.
> 9. README capability matrix v0.5 row present.
> 10. ADR-0005 committed and linked from `docs/architecture.md`.
> 11. Manual smoke test: scrape `/metrics`, send `traceparent` through, reload a route via SIGHUP — all three work.

- [ ] **Step 2: Apply FAIL findings**

Each fix is a small commit: `fix(...): <reason> (Plan 05 P05 T9 followup)`.

---

### Task 10: Code quality review

- [ ] **Step 1: Reviewer dispatch**

> Review the v0.5 feature branch holistically (don't re-check spec compliance). Specifically:
> 1. The shape of the `AppCore` / `AppSnapshot` split — does it survive the additions from P02/P03/P04? Is anything in `AppSnapshot` that should be in `AppCore` (e.g. fields that never change but happen to be derived from config)?
> 2. The metrics-rs name-parity test — is duplicating consts between observability and router actually less error-prone than introducing a tiny `agent-shim-metric-names` crate? Argue both sides.
> 3. The `CHANGELOG.md` entry — does it accurately capture every config schema addition? Walk through the `Cargo.toml` diff to verify no transitive dep changed without note.
> 4. Documentation cross-links — does every new doc link to every other related new doc, and to the prior-art ADR-0004?

- [ ] **Step 2: Apply CRITICAL/HIGH findings**

---

## Done when

- [ ] Workspace test count ≥ 639.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] `cargo fmt --all -- --check` clean.
- [ ] `cargo deny check` clean.
- [ ] Frozen-core diff empty.
- [ ] Workspace version 0.5.0.
- [ ] All docs deliverables present (observability.md, ADR-0005, architecture.md, contributing.md, configuration.md, 5 provider Observability subsections, README capability matrix v0.5, CHANGELOG [0.5.0]).
- [ ] T9 + T10 reviews clear of CRITICAL/HIGH.
- [ ] **Operator step (NOT in this plan):** `git tag -a v0.5.0 <tip-commit> -m "v0.5.0 — Phase 5: observable & operable gateway"` and `git push origin master v0.5.0` after merging the feature branch via `--no-ff` to master.
