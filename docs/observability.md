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

### Outbound `traceparent` propagation — deferred to v0.6

The gateway does NOT currently inject `traceparent` onto upstream HTTP
requests. Spec §4.3 and the spec deferral note both call this out as a
known v0.5 gap. Tracing continues through the gateway only when both
your agent AND the upstream support W3C Trace Context independently. A
provider-side hook the v0.4 frozen surface doesn't expose is required
to close this; v0.6 will land it.

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

### Rate-limit buckets across reload — deferred to v0.6

The matching half of spec §5.4 is **not yet implemented in v0.5**. The
reload-applying task only swaps `AppSnapshot`; `LimiterRegistry` lives
on the immutable `AppCore`. A reload that changes `rate_limit.*` is
therefore inert against running buckets — existing buckets keep
enforcing the old policy until the process restarts. Operators needing
immediate effect today must restart. The plan for v0.6 is either to
replace `governor` with a custom limiter that supports retuning, or to
add a token-preservation rebuild path so the reload-applying task can
drop and recreate affected buckets safely. See ADR-0005 §3 and spec
§5.4 for the rationale.

## What's NOT in v0.5

- **Outbound `traceparent` propagation** — the gateway honors inbound
  `traceparent` but does NOT inject one onto upstream HTTP requests.
  Tracing continues through the gateway only when both your agent AND
  upstream support W3C Trace Context independently. v0.6 closes this.
- **Rate-limit bucket effect on reload** — see "Rate-limit buckets
  across reload" above. v0.6 candidate.
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

### Why doesn't tightening rate limits take effect after reload?

See "Rate-limit buckets across reload" above — the v0.5 reload-
applying task swaps `AppSnapshot` only, while `LimiterRegistry` lives
on the immutable `AppCore`. Restart to apply new rate-limit policy in
v0.5. v0.6 candidate.
