# Configuration

AgentShim is configured via a YAML file (default `gateway.yaml`).
Every field below is checked at startup with `serde(deny_unknown_fields)`,
so typos fail loudly instead of being silently ignored.

## Top-level shape

```yaml
server:
  bind: "127.0.0.1"           # listen address
  port: 8787                   # listen port
  keepalive_secs: 15           # SSE keepalive interval (0 = disabled)

logging:
  format: pretty               # "pretty" | "json"
  filter: "info,agent_shim=debug"  # RUST_LOG-style filter

upstreams:
  <name>:
    type: <kind>               # see "Upstream types" below
    tier: <economy|standard|premium>  # required, v0.6+
    # …kind-specific fields…

routes:
  - frontend: <kind>           # "anthropic_messages" | "openai_chat" | "openai_responses"
    model: <alias>             # what the agent requests
    upstream: <upstream-name>  # which entry under `upstreams:`
    upstream_model: <name>     # what the upstream sees
    # …optional per-route fields…
```

## Upstream types

### `open_ai_compatible`

Generic OpenAI Chat Completions client. Works with any provider that
exposes `/v1/chat/completions` (DeepSeek, Kimi, Qwen, Ollama, vLLM,
Azure OpenAI, llama.cpp, etc.).

```yaml
upstreams:
  example:
    type: open_ai_compatible
    base_url: "https://api.example.com/v1"
    api_key: "sk-..."
    tier: standard                     # required, v0.6+
    request_timeout_secs: 120          # default 30
    default_headers: {}                # operator-level overrides
```

### `github_copilot`

GitHub Copilot via OAuth device flow. Requires running
`agent-shim copilot login` once before serving.

```yaml
upstreams:
  copilot:
    type: github_copilot
    tier: standard                     # required, v0.6+
    # No other fields — credentials come from the path below.

copilot:
  credential_path: "~/.config/agent-shim/copilot.json"  # optional, this is the default
```

### `anthropic`

Direct talk to api.anthropic.com (or a proxy). Hybrid path: byte-passthrough
when inbound is also Anthropic Messages, canonical translation otherwise.

```yaml
upstreams:
  anthropic:
    type: anthropic
    api_key: "sk-ant-..."
    base_url: "https://api.anthropic.com"   # default
    anthropic_version: "2023-06-01"         # default
    tier: premium                           # required, v0.6+
    request_timeout_secs: 60                # default 30
    default_headers: {}
```

### `deepseek`

Native api.deepseek.com client with `reasoning_content` interleaving
(deepseek-reasoner emits visible thinking blocks via the
ReasoningInterleaver state machine) and `cache_hit_tokens`/
`cache_miss_tokens` mapping into the canonical Usage shape.

```yaml
upstreams:
  deepseek:
    type: deepseek
    api_key: "sk-..."
    base_url: "https://api.deepseek.com/v1"  # default
    tier: economy                             # required, v0.6+
    request_timeout_secs: 30                  # default
    default_headers: {}
```

### `gemini`

Google AI Studio Generate Content API. Native JSON-array streaming on
`:streamGenerateContent` (NOT SSE), `inlineData`/`fileData` images,
`thinkingConfig` (budget tokens) for reasoning-capable models, and
`safetyRatings` round-trip via the canonical extensions map.

```yaml
upstreams:
  gemini:
    type: gemini
    api_key: "AIzaSy-..."
    base_url: "https://generativelanguage.googleapis.com/v1beta"  # default
    tier: standard                                                 # required, v0.6+
    request_timeout_secs: 30                                       # default
    default_headers: {}
```

The API key is sent as a `?key=...` query parameter (AI Studio
convention), never as a header. See
[docs/providers/gemini.md](providers/gemini.md) for details.

## Routes

```yaml
routes:
  - frontend: anthropic_messages   # the inbound API dialect
    model: claude-opus-4-7         # what the agent requests
    upstream: copilot              # which upstream serves it
    upstream_model: claude-opus-4-7  # what the upstream sees
    reasoning_effort: high         # optional: minimal | low | medium | high | xhigh
    anthropic_beta: context-1m-2025-08-07  # optional: default anthropic-beta header
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `frontend` | enum | — | `anthropic_messages`, `openai_chat`, or `openai_responses` |
| `model` | string | — | The alias the agent asks for. `*` is a wildcard catch-all. |
| `upstream` | string | — | A key under `upstreams:` |
| `upstream_model` | string | — | Model name sent to the upstream. `*` (in a wildcard route) means pass-through. |
| `reasoning_effort` | enum | — | Default thinking effort applied when the request omits one |
| `anthropic_beta` | string | — | Default `anthropic-beta` header value applied when the request omits one |

The route table is consulted by `agent_shim_router::StaticRouter`. Wildcards
(`model: "*"`) are matched only when no exact route exists.

## Environment overlay

Every config field can be overridden via environment variables with the
`AGENT_SHIM__` prefix and double-underscore as the path separator:

```bash
AGENT_SHIM__SERVER__PORT=9090
AGENT_SHIM__UPSTREAMS__DEEPSEEK__API_KEY=sk-...
AGENT_SHIM__LOGGING__FORMAT=json
```

Figment merges sources in priority order: defaults < YAML file < env vars.

## Secret handling

API keys must be supplied via environment variables (preferred) or via
the YAML file. The `Secret<String>` wrapper in `agent-shim-config`
prevents the value from appearing in `Debug` output or structured logs.

## Validate before running

```bash
agent-shim validate-config --config gateway.yaml
# OK: 4 routes, 2 upstreams
```

A non-zero exit code with a printed error means the config will not
start. Common errors:

* Unknown field (typo) — fix the spelling.
* Route references an upstream that isn't declared — add the upstream
  entry or remove the route.
* `reasoning_effort` value outside the documented enum — only the five
  named values are accepted.

## Example: full multi-provider config

See [`config/gateway.example.yaml`](../config/gateway.example.yaml) for
a complete annotated example covering DeepSeek, Anthropic, Gemini, and
Copilot upstreams plus their respective Tier-1 routes.

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

The admin listener serves `/metrics` (Prometheus text format),
`/healthz` (liveness), `/readyz` (readiness — providers initialized,
`AppSnapshot` populated), and `POST /admin/reload` (config reload). See
[`docs/observability.md`](observability.md) for the operator guide.

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

When `otel.endpoint` is absent the OTel layer is omitted; spans still
flow through the existing fmt subscriber so `RUST_LOG` enter/exit
events keep working in development. Inbound W3C `traceparent` headers
are honored regardless of whether export is configured.

**Outbound `traceparent` propagation** is implemented in v0.6. Every
provider's HTTP client is wrapped in
`agent_shim_providers::ProviderHttpClient`, which injects the current
span's W3C context onto every upstream request. See
[`docs/observability.md`](observability.md#outbound-traceparent-propagation)
and [ADR-0006](adr/0006-cost-aware-routing.md).

## Reload-validation rules (v0.5+)

The reload path runs additional rules beyond startup validation:

| Rule | Description |
| --- | --- |
| 11 | Every route's upstream must be declared in the running upstreams set. |
| 12 | Adding/removing entries in `upstreams.*` is forbidden. |
| 13 | `server.bind`, `server.port`, `admin.bind`, `admin.port` are immutable. |
| 14 | `otel.endpoint` is immutable. Other `otel.*` fields can change. |

Reload requests violating rules 11-14 return HTTP 403; rules 1-10
violations return HTTP 400. See ADR-0005 for the layering rationale and
`docs/observability.md` for the operator-facing reload guide.

### Observability behavior on reload

- **Routes, retry/breaker/rate-limit policies, auth keys + flags,
  logging filter, and most `otel.*` fields** swap atomically via
  `arc-swap`.
- **Breaker state is preserved across reload** — only policy
  (thresholds, windows, cooldowns) updates. Empirical signal about
  unhealthy upstreams survives.
- **Rate-limit policy is reloadable in v0.6.** `LimiterRegistry` now
  lives behind `Arc<ArcSwap<LimiterRegistry>>` on `AppCore`. The
  reload-applying task rebuilds the registry from the new policy and
  atomically swaps it; the change takes effect on the next request.
  Existing in-flight buckets are replaced (not migrated) — see
  [`docs/observability.md`](observability.md#rate-limit-buckets-across-reload)
  and ADR-0005 §3 for the caveat.

## Cost-aware routing fields (v0.6)

Five new schema fields land in v0.6 across upstreams and routes. They
compose declaratively to filter the v0.4 fallback chain — see
[ADR-0006](adr/0006-cost-aware-routing.md) and
[`docs/observability.md`](observability.md#cost-aware-routing-v06).

### Upstream-level

| Field | Type | Required | Description |
|---|---|---|---|
| `tier` | `economy \| standard \| premium` | **yes** | Service tier label. Routes can require `min_tier`. No default; absent → startup error. |
| `cost.input_per_million_usd` | `f64` (≥ 0) | no | USD per million input tokens. Combined with the request's `tiktoken-rs` count to drive the cap axis. |
| `cost.output_per_million_usd` | `f64` (≥ 0) | no | USD per million output tokens. Combined with the request's `max_tokens` ceiling (default 4096) to drive the cap axis. |
| `p95_latency_budget_ms` | `u64` | no | Maximum allowed recent p95 latency, in milliseconds. Compared against `agent_shim_upstream_duration_seconds`. |

Example:

```yaml
upstreams:
  premium_anthropic:
    type: anthropic
    api_key: sk-ant-...
    tier: premium
    cost:
      input_per_million_usd: 15.0
      output_per_million_usd: 75.0
    p95_latency_budget_ms: 2000
```

When `cost` is absent (e.g. Copilot subscription product), the cap
axis is effectively no-op for that upstream — there is no per-request
cost to compare against `max_cost_usd`. The same applies to
`p95_latency_budget_ms`: absent → that upstream is not gated on
latency.

### Route-level

| Field | Type | Required | Description |
|---|---|---|---|
| `min_tier` | `economy \| standard \| premium` | no | Minimum upstream tier accepted by this route. Upstreams with `tier < min_tier` are filtered out. |
| `max_cost_usd` | `f64` (≥ 0) | no | Per-request cost cap. Estimated cost > cap → upstream filtered out. |

Example:

```yaml
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstreams:
      - {name: premium_anthropic, model: claude-opus-4-7}
      - {name: copilot, model: gpt-4o}
    min_tier: standard
    max_cost_usd: 0.05
```

When every upstream in a route's chain is filtered, the gateway
returns **HTTP 503 `NoEligibleUpstream`** with a body listing each
skipped upstream and the per-axis reason. The 503 is rendered through
the inbound frontend's existing error envelope (Anthropic, OpenAI
Chat, OpenAI Responses).

### Image-aware cost estimation (v0.6.1+)

As of v0.6.1, image content blocks contribute to the per-request
input-token estimate that `max_cost_usd` is evaluated against. The
estimator dispatches by inbound `FrontendKind`:

- **Anthropic Messages frontend** → uses Anthropic's documented
  token-per-pixel divisor, falling back to a 1600-token worst-case
  for images of unknown dimensions.
- **OpenAI Chat frontend** → uses OpenAI's documented tile math
  (85 + 170 × ⌈w/512⌉ × ⌈h/512⌉ for high-detail), falling back to
  1105 tokens for unknown dimensions.
- **OpenAI Responses frontend** → mirrors OpenAI's math.

The canonical request type doesn't record image dimensions today —
the gateway always passes `ImageSizeHint::Unknown` to the estimator,
which means image cost contribution is always the vendor's published
worst-case figure. This makes the cap **conservative** (rejects more
requests than the realised cost will actually be), which matches the
v0.6.0 design intent for `max_cost_usd` ("reject if it COULD cost
more than $X").

Accuracy bar: each estimator's output is within ±5% of the
vendor-published token figures for fixture inputs verified in
[the impl's `#[cfg(test)]` tests](../crates/router/src/image_estimators/).

Realised-cost feedback (rolling EWMA over observed token usage) is
still v0.7+ scope — see [ADR-0006](adr/0006-cost-aware-routing.md)'s
Open Questions section.

### Validation rules 15-18

The v0.6 schema adds four new validation rules on top of the v0.5
reload set (rules 11-14):

| Rule | Description | Failure HTTP code (reload) |
| --- | --- | --- |
| 15 | `cost.input_per_million_usd` and `cost.output_per_million_usd` must be non-negative when present. | 400 |
| 16 | `tier` must be one of `economy`, `standard`, `premium`. Unknown values fail with the enum-list message. | 400 |
| 17 | Every route declaring `min_tier` must have at least one upstream in its chain that meets `min_tier`. Checked at startup AND on reload. | 400 |
| 18 | `tier`, `cost.*`, `p95_latency_budget_ms`, `min_tier`, `max_cost_usd` are all reloadable. Changing any of them via `/admin/reload` takes effect on the next request — no restart required. | n/a (positive rule) |

Rule 17 has an important corollary: removing the last `min_tier`-
satisfying upstream from a route's chain via reload is rejected.
Operators relaxing tier requirements should lower `min_tier` first,
then trim the chain in a second reload.

See [ADR-0006](adr/0006-cost-aware-routing.md) §2 for the design
decision behind keeping the four axes as filters (not re-sorters).
