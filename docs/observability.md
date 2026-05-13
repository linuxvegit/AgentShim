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

The full set of v0.5 metrics, with HELP text and example label sets:

### Request lifecycle

| Metric | Type | Labels | Description |
| --- | --- | --- | --- |
| `agent_shim_requests_total` | counter | `frontend`, `route`, `status_class` | Inbound requests grouped by frontend dialect, resolved route, and HTTP status class (`2xx`/`4xx`/`5xx`). |
| `agent_shim_request_duration_seconds` | histogram | `frontend`, `route`, `status_class` | End-to-end request latency. |
| `agent_shim_request_body_bytes` | histogram | `frontend`, `route` | Inbound request body size. |
| `agent_shim_in_flight_requests` | gauge | (none) | Current in-flight request count. |

### Resilience layer

These mirror the v0.4 tracing taxonomy 1:1.

| Metric | Type | Labels | Description |
| --- | --- | --- | --- |
| `agent_shim_retry_attempts_total` | counter | `route`, `upstream`, `model`, `outcome` | Retry attempts. `outcome` is `success` / `terminal` / `retried`. |
| `agent_shim_retry_exhausted_total` | counter | `route`, `upstream`, `model`, `reason` | Retry budget exhausted. `reason` is `max_attempts` / `total_budget`. |
| `agent_shim_fallback_transitions_total` | counter | `route`, `from_upstream`, `to_upstream` | Transitions between upstreams in a fallback chain. |
| `agent_shim_breaker_state_changes_total` | counter | `upstream`, `model`, `from_state`, `to_state` | Circuit breaker state transitions. |
| `agent_shim_rate_limit_rejected_total` | counter | `dimension`, `route`, `upstream` | Rate-limit rejections. `dimension` is `api_key` / `route` / `upstream` / `source_ip`. |

### Upstream calls

| Metric | Type | Labels | Description |
| --- | --- | --- | --- |
| `agent_shim_upstream_duration_seconds` | histogram | `upstream`, `model`, `status_class` | Time spent in the upstream HTTP call. |
| `agent_shim_upstream_errors_total` | counter | `upstream`, `model`, `error_class` | Upstream errors. `error_class` is `network` / `upstream_5xx` / `upstream_4xx` / `upstream_429` / `decode`. |

### Token accounting

| Metric | Type | Labels | Description |
| --- | --- | --- | --- |
| `agent_shim_tokens_input_total` | counter | `upstream`, `model` | Sum of input tokens reported by upstream usage. |
| `agent_shim_tokens_output_total` | counter | `upstream`, `model` | Sum of output tokens reported by upstream usage. |

### Reload

| Metric | Type | Labels | Description |
| --- | --- | --- | --- |
| `agent_shim_config_reloads_total` | counter | `result` | Reload attempts. `result` is `success` / `validation_failed` / `io_failed`. |

### Cardinality budget

- `frontend` — 3 values
- `route` — bounded by route count in YAML (typically <50)
- `upstream` × `model` — bounded by config

The metric namespace deliberately avoids high-cardinality labels:
`request_id` flows via tracing only; `api_key_hash` is in tracing only;
`model_alias_requested_by_client` is replaced by the *resolved* `route`
label in metrics.

### Adding a new metric (v0.6.1+)

Every metric is declared once in `crates/observability/src/metrics/catalog.rs`
as a zero-sized marker struct with `#[derive(Metric)]`. Adding a new
metric is a one-site edit:

```rust
#[derive(Metric)]
#[metric(
    name = "agent_shim_my_new_total",
    kind = "counter",
    help = "Per-route count of my-new-thing"
)]
pub struct MyNewTotal;
```

The derive emits `MyNewTotal::NAME`, registers a Prometheus description
via `linkme`-collected slice, and surfaces the metric in the
[structural-parity test](../crates/observability/tests/derive_metric_parity.rs)
on the next release.

The legacy `pub const NAME: &str = "..."` constants in
`crates/observability/src/metrics/names.rs` are preserved for source-
backward compatibility; new code should reference the marker struct
directly (`catalog::MyNewTotal::NAME`).

If a metric needs a `recorders.rs` helper (typed wrappers around
`metrics::counter!` / `metrics::histogram!` macros for label-set
enforcement), add it alongside the existing helpers in
`crates/observability/src/metrics/recorders.rs`. Recorders are
intentionally not auto-generated — they encode per-metric label
semantics that the catalog can't.

### Example Grafana dashboard panels

Useful starting panels (PromQL):

```promql
# Request rate by route
sum by (route) (rate(agent_shim_requests_total[5m]))

# p99 latency by route
histogram_quantile(0.99,
  sum by (route, le) (rate(agent_shim_request_duration_seconds_bucket[5m])))

# Retry rate (attempts / requests) by upstream
sum by (upstream) (rate(agent_shim_retry_attempts_total[5m]))
  /
sum by (upstream) (rate(agent_shim_requests_total[5m]))

# Breaker state changes — sparse, alert when nonzero
sum by (upstream, model, to_state) (
  increase(agent_shim_breaker_state_changes_total[5m]))

# Rate-limit rejection rate by dimension
sum by (dimension) (rate(agent_shim_rate_limit_rejected_total[5m]))

# In-flight requests gauge
agent_shim_in_flight_requests
```

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
gateway as a parented span.

### Outbound `traceparent` propagation

Outbound `traceparent` injection is implemented in v0.6 (Plan 01 / see
[ADR-0006](adr/0006-cost-aware-routing.md) for the wider Phase 6
context). Every provider's `reqwest::Client` is wrapped in
`agent_shim_providers::ProviderHttpClient`, which injects W3C
`traceparent` (and `tracestate` when present) from the current
`Span`'s OTel context onto every outgoing request. End-to-end
distributed traces through the gateway work without operator action.

When OTel export is disabled (`otel.endpoint` absent), no
`traceparent` is injected — the upstream sees the same request bytes
v0.5 would have produced.

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

Both re-read the file at `--config` and validate before swapping. The
reload-applying task (`commands::serve::handle_reload`) drains the
internal mpsc channel both triggers feed and replies on a one-shot
channel so the caller observes the outcome (HTTP 200 vs 400/403/500).

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
the empirical signal is more important than the policy. Covered by the
integration test in `crates/gateway/tests/reload_in_flight.rs`.

### Rate-limit buckets across reload

Rate-limit policy reload is implemented in v0.6 (Plan 02). The
reload-applying task rebuilds `LimiterRegistry` from the new
`rate_limit.*` policy and atomically swaps the `Arc<ArcSwap<LimiterRegistry>>`
that `AppCore` now holds. A reload that changes any `rate_limit.*`
field takes effect on the very next request — no process restart
required.

**Caveat:** existing in-flight buckets are **replaced**, not migrated.
The `governor` crate that backs the buckets does not expose retuning,
so the operator-visible behaviour is "swap to new policy with a fresh
token allowance" rather than "preserve token counts across the swap".
For typical operations (relaxing a limit, tightening a limit) this is
the expected semantic. See spec §5.4 (Phase 5) and the Phase 6 spec §3
for the rationale.

Covered by `crates/gateway/tests/reload_rate_limit_buckets_reset.rs`.

## Cost-aware routing (v0.6)

Phase 6 adds four orthogonal axes the operator can apply per route:

1. **Tier label** — `tier: economy | standard | premium` on every
   upstream. Routes can declare `min_tier`. Upstreams below the
   route's `min_tier` are filtered out.
2. **Per-token cost** — `cost: {input_per_million_usd, output_per_million_usd}`
   on an upstream. Combined with the request's input-token count
   estimate (via `tiktoken-rs`'s `cl100k_base` encoder) and the
   request's `max_tokens` ceiling (default 4096), this produces a
   per-request cost estimate.
3. **Latency budget** — `p95_latency_budget_ms` on an upstream. The
   cost filter compares the budget against the recent p95 read from
   `agent_shim_upstream_duration_seconds`.
4. **Per-route cost cap** — `max_cost_usd` on a route. Upstreams
   whose estimated cost exceeds this cap are filtered out.

When every upstream in a route's chain is filtered, the gateway
returns **HTTP 503 `NoEligibleUpstream`** with a JSON body listing
each skipped upstream and the per-axis reason. The response is
rendered through the inbound frontend's existing error envelope
(Anthropic, OpenAI Chat, OpenAI Responses) so agents see the failure
in their native shape.

The filter pass sits **between** the rate-limit gate and the v0.4
`ResilientCaller` chain walk. Filtered chains shrink before retry /
breaker / fallback get to see them; an upstream the cost filter
rejected never receives a request.

### Metrics

`agent_shim_cost_filtered_total{reason, upstream, route}` — per-axis
counter.

| reason | meaning | upstream skipped? |
|---|---|---|
| `tier` | upstream.tier < route.min_tier | yes |
| `latency` | recent p95 > upstream.p95_latency_budget_ms | yes |
| `cap` | estimated cost > route.max_cost_usd | yes |
| `latency_unknown` | probe has no samples yet for this upstream | **no** (passes through) |
| `tiktoken_fallback` | reserved for a future stricter encode-failure mode | **no** (passes through) |

The `latency_unknown` reason is a **note**, not a **skip** — operators
see warm-up periods in the counter without those buckets affecting
survival decisions. The `tiktoken_fallback` reason is defined for
metric-label parity with the spec but is not produced by the v0.6
filter; `cl100k_base` + `encode_ordinary` handle arbitrary user input
gracefully.

### Tuning

- `tier` is **required** on every upstream. There is no default. A
  config without `tier:` on every upstream fails to parse at startup.
- Cost estimates are intentionally pessimistic (upper bound). A
  filter rate that surprises you is usually a sign the cap is set
  too tightly for the request shape, not that the gateway is
  miscalibrated.
- The latency probe parses the `/metrics` scrape on each query. For
  sub-millisecond latency-sensitive deployments, watch for the parse
  overhead in `agent_shim_request_duration_seconds`. The ADR-0006
  "Negative consequences" section flags this as a v0.6.1 candidate.

See [ADR-0006](adr/0006-cost-aware-routing.md) for the design context
and rejected alternatives.

## What's NOT in v0.6

- **Realised-cost tracking (rolling EWMA).** The cost filter uses an
  upper-bound estimate; observed token counts don't feed back into
  the filter. v0.7+ candidate.
- **Distributed cost-filter state.** Each gateway instance applies
  the filter independently; multi-instance deployments don't share
  filter counts. v0.7+ candidate.
- **Distributed breaker/rate-limit state** — multi-instance deploys
  behind a load balancer don't share breaker or limiter state.
  v0.7+ candidate.
- **Agent-driven routing hints** — the policy decision belongs to the
  operator, not the agent. Explicitly out of scope; rationale in
  ADR-0006.
- **k8s manifests / Helm chart** — operators write their own.
- **Redacted request/response capture** — too disk-heavy for a default;
  v0.7+.

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

### How do I know whether a request was rejected by the cost filter?

Two surfaces:

- The HTTP response is **503** with a frontend-shaped error envelope
  whose body lists each skipped upstream and the per-axis reason
  (`NoEligibleUpstream`).
- The `agent_shim_cost_filtered_total{reason, upstream, route}`
  counter increments per axis. Dashboard with
  `sum by (reason) (rate(agent_shim_cost_filtered_total[5m]))`.

### Tightening rate limits should take effect immediately — does it?

Yes, as of v0.6. The reload-applying task rebuilds `LimiterRegistry`
and atomically swaps the `ArcSwap` slot on `AppCore`. Existing in-
flight buckets are replaced with fresh ones built from the new policy.
Covered by `crates/gateway/tests/reload_rate_limit_buckets_reset.rs`.
