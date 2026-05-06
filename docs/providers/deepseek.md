# Provider: DeepSeek

DeepSeek-as-backend talks directly to `api.deepseek.com` (or any
API-compatible host configured by the operator) using the OpenAI-style
`/chat/completions` endpoint, with two DeepSeek-specific quirks layered
on: interleaved `reasoning_content` deltas (R1 family) and
`prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` usage fields.

Unlike the Anthropic provider, DeepSeek has **no passthrough fast
path** — every inbound frontend goes through canonical encode/decode.
Both `anthropic_messages` and `openai_chat` requests are translated
into the OAI-Chat wire shape, and the upstream stream is parsed back
into a `CanonicalStream`.

| Inbound frontend | Outbound path | Reasoning blocks visible? |
|---|---|---|
| `anthropic_messages` | Canonical translation (OAI-Chat shape) | Yes — rendered as `thinking` events |
| `openai_chat` | Canonical translation (OAI-Chat shape) | No — reasoning is dropped by the OpenAI Chat encoder |

## Config Example

```yaml
upstreams:
  deepseek:
    type: deepseek
    api_key: "sk-your-deepseek-api-key-here"
    # Optional overrides (defaults shown):
    # base_url: https://api.deepseek.com/v1
    # request_timeout_secs: 30
    # default_headers: {}
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `type` | string | — | Must be `deepseek` |
| `api_key` | secret | — | Sent as the `Authorization: Bearer <key>` header |
| `base_url` | string | `https://api.deepseek.com/v1` | Override for proxies / region endpoints; `/chat/completions` is appended |
| `default_headers` | map | `{}` | Operator-level header overrides |
| `request_timeout_secs` | u64 | `30` | Timeout for the upstream HTTP call |

## Capability Flags

The provider declares:

| Flag | Value | Meaning |
|---|---|---|
| `streaming` | `true` | DeepSeek emits SSE for `/chat/completions` on both `deepseek-chat` and `deepseek-reasoner` |
| `tool_use` | `true` | OpenAI-style `tool_calls` cross-encode through the canonical mapping tables |
| `vision` | `false` | DeepSeek does not accept image content blocks in v0.2 |
| `json_mode` | `true` | DeepSeek supports `response_format: { type: "json_object" }` |

## Sample Routes

### OpenAI Chat frontend → DeepSeek (general use)

```yaml
routes:
  - frontend: openai_chat
    model: deepseek-chat
    upstream: deepseek
    upstream_model: deepseek-chat
```

A standard chat-completions setup. Tool definitions, tool choice, and
tool-call argument deltas all work through this seam via the shared
OAI-Chat wire encoder.

### Anthropic frontend → DeepSeek-Reasoner (visible thinking)

```yaml
routes:
  - frontend: anthropic_messages
    model: deepseek-reasoner
    upstream: deepseek
    upstream_model: deepseek-reasoner
```

Recommended when you want the agent to see the model's reasoning
trace. DeepSeek-R1 emits `delta.reasoning_content` chunks alongside
the regular `delta.content` chunks; the parser routes both through the
`ReasoningInterleaver` state machine, and the Anthropic frontend
encoder renders the reasoning blocks as `content_block_start
type: thinking` events. Claude Code displays them as a "thinking"
indicator.

### Per-route `reasoning_effort` default

```yaml
routes:
  - frontend: openai_chat
    model: deepseek-reasoner
    upstream: deepseek
    upstream_model: deepseek-reasoner
    reasoning_effort: medium
```

Note: DeepSeek does **not** consult `reasoning_effort` today —
reasoning emergence on `deepseek-reasoner` is automatic and the field
is silently dropped at the provider boundary. The route-level default
is still accepted for forward compatibility and consistency with
upstreams that do honor it (Copilot/GPT-5/o-series).

## Behavior

### Authentication

Requests are authenticated with `Authorization: Bearer <api_key>`
(OpenAI-style), **not** Anthropic-style `x-api-key`. The key is never
logged.

### Endpoint

`<base_url>/chat/completions` for chat, `<base_url>/models` for the
model list. With the default `base_url` of `https://api.deepseek.com/v1`,
the resolved chat URL is `https://api.deepseek.com/v1/chat/completions`.
A trailing slash on `base_url` is tolerated.

### Streaming

Full SSE streaming is supported on both `deepseek-chat` and
`deepseek-reasoner`. The parser is a clone-and-modify of the shared
`oai_chat_wire::chat_sse_parser` with two DeepSeek-specific changes
(see "Reasoning Interleaving" and "Cache Usage Mapping" below).

### Tool-Call Translation

Standard OAI-Chat `tool_calls` flow through
`agent-shim-core::mapping::oai_chat_wire`. On the canonical path, the
provider emits `ToolCallStart`, `ToolCallArgumentsDelta`, and
`ToolCallStop` events as the upstream streams `tool_calls` deltas.
Tool-call block indices are allocated **after** the interleaver's
text/reasoning blocks, rather than at a fixed offset, because reasoning
content can appear at index 0 ahead of the first text block.

### Reasoning Interleaving (deepseek-reasoner)

DeepSeek-R1 (`deepseek-reasoner`) emits `delta.reasoning_content`
deltas in addition to the regular `delta.content` stream. AgentShim
parses these through the `ReasoningInterleaver` state machine, which:

* Opens a `Reasoning` content block when a `reasoning_content` delta
  arrives, emitting `ContentBlockStart` / `ReasoningDelta` events.
* Closes the reasoning block and opens a `Text` block (or vice
  versa) when the upstream switches kinds, emitting a `ContentBlockStop`
  for the prior block before the new `ContentBlockStart`.
* Flushes any open block on `[DONE]` so a malformed stream that skips
  `finish_reason` still terminates cleanly.

Cross-protocol behavior:

* **Anthropic frontend** — reasoning blocks render as
  `content_block_start type: thinking` events. Claude Code displays
  them as a "thinking" indicator alongside the visible response.
* **OpenAI Chat frontend** — the OpenAI Chat dialect has no canonical
  reasoning-delta field, so `ReasoningDelta` events are silently
  dropped by the encoder. Clients see only the regular `content`
  deltas. This matches the behavior of every other reasoning-capable
  upstream when the inbound is OpenAI Chat.

### Cache Usage Mapping

DeepSeek's `usage` block reports prompt-cache hit and miss counts as
two separate fields:

* `prompt_cache_hit_tokens`  — tokens served from the prompt cache.
* `prompt_cache_miss_tokens` — *new* prompt tokens (the not-cached
  portion).

These map onto the canonical `Usage` shape as follows:

| DeepSeek field | Canonical field |
|---|---|
| `prompt_cache_hit_tokens` | `cache_read_input_tokens` |
| `prompt_cache_miss_tokens` | `input_tokens` (with fallback to `prompt_tokens` when miss is absent) |
| — | `cache_creation_input_tokens` is always `None` (DeepSeek has no concept of cache creation) |
| `completion_tokens` | `output_tokens` |
| (full `usage` object) | preserved verbatim in `provider_raw` |

DeepSeek's prompt cache is **implicit and server-managed** — there
is no inbound `cache_control` knob for users to set. The cache hit
count is observed-only signal.

### `cache_control` drop (v0.2 limitation)

The DeepSeek provider applies a defense-in-depth `strip_cache_control`
step to the outbound body, removing any Anthropic-style
`cache_control` keys from `messages[].cache_control` and
`messages[].content[].cache_control`. When one or more keys are
stripped, a single `tracing::debug!` event is emitted per request
with the strip count.

Today the OAI-Chat encoder does not produce `cache_control` keys —
that field is an Anthropic wire concept and the encoder ignores
per-block extensions entirely. The strip is therefore a guard against
a future encoder change leaking the field, which would otherwise
cause DeepSeek's API to reject the request with a 400 (its
`/chat/completions` rejects unknown fields).

This is a **v0.2 limitation; revisit if DeepSeek ships explicit cache
markers.** When and if DeepSeek exposes a user-facing cache-control
surface, we'll add a translation layer that maps Anthropic-style
markers onto DeepSeek's native shape.

### Lossy fields

The following Anthropic-frontend concepts do **not** round-trip when
the inbound frontend is `anthropic_messages` and the outbound is
DeepSeek:

* `cache_control` markers — silently dropped (with a debug log if any
  were present in the encoded body). DeepSeek's prompt cache is
  server-managed and has no explicit user surface.
* `thinking` config (`thinking: { type: "enabled", budget_tokens: ...
  }`) — DeepSeek does not honor this. Reasoning emergence on
  `deepseek-reasoner` is automatic; the provider does not consult
  `GenerationOptions.thinking`.
* `signature` deltas — DeepSeek does not emit Anthropic-style
  thinking-block signatures. Reasoning is delivered via
  `reasoning_content` deltas only.

### Error Handling

HTTP non-2xx responses from the upstream surface as
`ProviderError::Upstream { status, body }`, carrying the original
status code and response body for the frontend to format into a
protocol-shaped error. The body is also logged at `warn!` level with
the upstream status.

### Retries

The provider does **not** retry automatically. Retry logic belongs in
the caller or at the infrastructure layer.
