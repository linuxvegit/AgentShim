# Provider: Anthropic

Anthropic-as-backend talks directly to `api.anthropic.com` (or any
API-compatible host configured by the operator) using the `/v1/messages`
Messages API.

This provider implements a **hybrid path**: when the inbound frontend is
`anthropic_messages` the request bytes pass through unchanged; when the
inbound frontend is `openai_chat` or `openai_responses` the canonical
request is encoded into Messages JSON and the upstream response is parsed
back into a `CanonicalStream`. See ADR-0001 for the design rationale.

| Inbound frontend | Outbound path | Anthropic-only fields preserved? |
|---|---|---|
| `anthropic_messages` | Passthrough (`proxy_raw`) | Yes — byte-for-byte |
| `openai_chat` | Canonical translation | No (see "Lossy fields") |
| `openai_responses` | Canonical translation | No (see "Lossy fields") |

## Config Example

```yaml
upstreams:
  anthropic:
    type: anthropic
    api_key: "sk-ant-your-key-here"
    # Optional overrides:
    # base_url: https://api.anthropic.com
    # anthropic_version: "2023-06-01"
    # default_headers:
    #   x-operator-tag: prod
    # request_timeout_secs: 60
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `type` | string | — | Must be `anthropic` |
| `api_key` | secret | — | Sent as the `x-api-key` header |
| `base_url` | string | `https://api.anthropic.com` | Override for proxies / region endpoints |
| `anthropic_version` | string | `2023-06-01` | Sent as the `anthropic-version` header |
| `default_headers` | map | `{}` | Operator-level header overrides |
| `request_timeout_secs` | u64 | `30` | Timeout for the upstream HTTP call |

## Capability Flags

The provider declares:

| Flag | Value | Meaning |
|---|---|---|
| `streaming` | `true` | SSE streaming is supported on both passthrough and canonical paths |
| `tool_use` | `true` | Anthropic tool definitions and tool calls cross-encode through the canonical mapping tables |
| `vision` | `true` | Image content blocks are supported when the frontend supplies them |
| `json_mode` | `false` | Anthropic does not implement OpenAI-style strict JSON mode in v0.2; use a `tool_use` block as the structured-output workaround |

## Sample Routes

### Anthropic frontend → Anthropic backend (passthrough fast path)

```yaml
routes:
  - frontend: anthropic_messages
    model: claude-opus-4-7
    upstream: anthropic
    upstream_model: claude-opus-4-7
```

Request bytes are forwarded through `proxy_raw`. `cache_control` markers,
`thinking` blocks (including `signature` deltas), and any
`anthropic-beta` headers the agent sent round-trip byte-for-byte.

### OpenAI Chat frontend → Anthropic backend (canonical translation)

```yaml
routes:
  - frontend: openai_chat
    model: claude-via-oai
    upstream: anthropic
    upstream_model: claude-opus-4-7
```

Tool definitions, tool choice, and tool-call argument deltas all work
across this seam. `cache_control`, `thinking.signature`, and other
Anthropic-only fields are dropped (see "Lossy fields" below).

### Per-route `anthropic-beta` default

```yaml
routes:
  - frontend: anthropic_messages
    model: claude-opus-4-7
    upstream: anthropic
    upstream_model: claude-opus-4-7
    anthropic_beta: context-1m-2025-08-07
```

The route default applies when the inbound request didn't supply its own
`anthropic-beta` header. Inbound always wins.

## Behavior

### Authentication

Requests are authenticated with the `x-api-key` header set to the
configured `api_key`. The `anthropic-version` header is set from the
upstream config (default `2023-06-01`). The key is never logged.

### Hybrid path (passthrough vs canonical)

The provider exposes two surfaces on the `BackendProvider` trait:

* `proxy_raw` — short-circuits when the inbound `FrontendKind` is
  `AnthropicMessages`. The inbound body is forwarded to
  `<base_url>/v1/messages` unchanged; the upstream response stream is
  returned as raw bytes for the frontend's passthrough encoder.
* `complete` — the canonical path. `request::build` encodes the
  `CanonicalRequest` into Messages-shaped JSON; `response::parse_stream`
  / `response::parse_unary` decode the upstream SSE or unary body back
  into `CanonicalStream` events. Tool-call argument deltas via
  `input_json_delta` become canonical `ToolCallArgumentsDelta` events.

This split is described in ADR-0001 (the hybrid passthrough+canonical
provider design).

### Streaming

Full SSE streaming is supported on both paths. On the canonical path the
SSE event types Anthropic emits (`message_start`, `content_block_start`,
`content_block_delta`, etc.) are translated to canonical
`StreamEvent`s using the shared mapping tables in
`agent-shim-core::mapping::anthropic_wire`.

### Tool-Call Translation

Tool definitions and `tool_choice` cross-encode through the mapping
tables in `agent-shim-core::mapping::anthropic_wire`. On the canonical
path, the provider emits canonical `ToolCallStart`,
`ToolCallArgumentsDelta`, and `ToolCallStop` events as the upstream
streams `tool_use` content blocks and `input_json_delta` chunks.

### Lossy fields

The following fields do **not** round-trip when the request crosses
protocols (i.e. inbound frontend is not `anthropic_messages`):

* `cache_control` markers — preserved on the passthrough path only;
  dropped on the canonical path because OpenAI Chat / Responses have no
  equivalent surface.
* `thinking.signature` deltas — preserved on the passthrough path;
  dropped with a debug log on the canonical path.
* Anthropic-specific beta headers (`anthropic-beta`,
  `anthropic-version`, etc.) — preserved on the passthrough path. On
  the canonical path only the per-route `anthropic_beta` default is
  applied via `RoutePolicy::resolve`; the inbound request has no
  Anthropic-specific header surface to forward.

If you need byte-perfect fidelity on these fields, route Anthropic
inbound to the Anthropic backend.

### `anthropic-beta` header forwarding

`RoutePolicy::resolve` merges the inbound `anthropic-beta` header (if
present) with the per-route `anthropic_beta` default. **Inbound wins**;
the route default is the fallback. Comma-separated values are passed
through unchanged. See the README "Anthropic beta features" section for
the end-to-end picture.

### Error Handling

HTTP non-2xx responses from the upstream surface as
`ProviderError::Upstream { status, body }`, carrying the original status
code and response body for the frontend to format into a protocol-shaped
error.

### Retries

The provider does **not** retry automatically. Retry logic belongs in
the caller or at the infrastructure layer.
