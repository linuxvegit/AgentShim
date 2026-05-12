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
    # No fields — credentials come from the path below.

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

**Outbound `traceparent` propagation onto upstream HTTP calls is
deferred to v0.6.** See `docs/observability.md` "What's NOT in v0.5"
and spec §4.3 for context.

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
- **Rate-limit bucket changes are inert in v0.5.** `LimiterRegistry`
  lives on the immutable `AppCore`; a reload that changes
  `rate_limit.*` has no effect on running buckets until the process
  restarts. v0.6 candidate. See ADR-0005 §3 and
  `docs/observability.md` for details.
