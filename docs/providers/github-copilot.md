# Provider: GitHub Copilot

GitHub Copilot-as-backend lets paying Copilot subscribers route
Anthropic-frontend or OpenAI-frontend agents to Copilot's underlying
models (Claude, GPT-4o, GPT-5, etc.) through the gateway. The provider
reuses the OpenAI-compatible request encoder and SSE parser; what's
unique to Copilot is the **OAuth device-flow login**, the long-lived
**token manager** that handles proactive refresh, and the
Copilot-specific request headers.

| Inbound frontend  | Outbound path                                | Notes                                    |
|---|---|---|
| `anthropic_messages` | Canonical translation (OpenAI Chat shape) | Tools + vision + reasoning_effort        |
| `openai_chat`        | Canonical translation (OpenAI Chat shape) | Tools + vision + reasoning_effort        |
| `openai_responses`   | Canonical translation (OpenAI Chat shape) | Tools + vision + reasoning round-trip    |

## Config Example

```yaml
upstreams:
  copilot:
    type: github_copilot
    # No fields — credentials come from the path below.

copilot:
  credential_path: ~/.config/agent-shim/copilot-credentials.json   # optional, this is the default

routes:
  - frontend: anthropic_messages
    model: claude-sonnet-4-5
    upstream: copilot
    upstream_model: claude-sonnet-4-5
  - frontend: openai_chat
    model: gpt-4o
    upstream: copilot
    upstream_model: gpt-4o
```

Run `agent-shim copilot login` once before serving — the device flow
opens a browser, prompts for GitHub authorization, and writes the
refresh token to `credential_path`.

## Capability Flags

| Flag         | Value  | Meaning                                                     |
|---|---|---|
| `streaming`  | `true` | SSE streaming on `/chat/completions`                        |
| `tool_use`   | `true` | OpenAI-style `tool_calls` cross-encode                      |
| `vision`     | `true` | Copilot serves Claude / GPT-4o; both vision-capable         |
| `json_mode`  | `true` | `response_format: { type: "json_object" }` is honored       |

## Behavior

### Authentication

The `CopilotTokenManager` actor owns the long-lived GitHub OAuth
refresh token (persisted) and the short-lived Copilot API token (held
in memory with embedded expiry). It exposes a single
`get_token() -> Result<CopilotToken>` channel that serializes refresh,
avoids thundering herds, and serves cached tokens otherwise. The
provider attaches the API token as `Authorization: Bearer <token>`
plus the Copilot-specific request headers
(`Editor-Version`, `Editor-Plugin-Version`, `Copilot-Integration-Id`,
etc.). Tokens are never logged.

### Endpoint

The Copilot API endpoint URL is **dynamic** — it is returned by the
token-exchange call, not hardcoded — so the provider re-reads it on
every refresh. This matches Copilot's published behavior.

### Streaming, Tool-Call Translation, Reasoning

All three flow through the shared `oai_chat_wire` encoder/parser. See
[`openai-compatible.md`](openai-compatible.md) for the wire-level
details. Copilot's reasoning-capable models (`o1`, `gpt-5`, etc.)
honor `reasoning_effort`; the route-default
`reasoning_effort: "medium"` (or any of `minimal | low | medium | high
| xhigh`) is forwarded.

### Vision

Inbound `ContentBlock::Image` flows through the OpenAI Chat
`image_url` part shape. Both `BinarySource::Url` (https URLs) and
`BinarySource::Base64` (data URIs) are supported. Copilot's
underlying vision model (Claude or GPT-4o) determines what content
types are accepted upstream.

### Error Handling

HTTP non-2xx responses surface as `ProviderError::Upstream { status,
body }`. The token manager handles the **stale-token 401** case
internally — when it observes a 401 from the Copilot API endpoint, it
refreshes the API token via the GitHub OAuth refresh token and the
provider retries once before surfacing the error. A persistent 401
after refresh (revoked subscription, missing Copilot entitlement) is
terminal.

### Retries

The provider does **not** retry above the token-manager refresh path
described above. Higher-level retry logic belongs in the v0.4
`ResilientCaller` (see below).

## Resilience behavior

This provider participates in the v0.4 resilience subsystem. See
[`docs/resilience.md`](../resilience.md) for the operator-facing guide
and the [Phase 4 design spec](../superpowers/specs/2026-05-08-phase-4-resiliency-design.md)
for the layering details.

**Default fallback eligibility for this provider:**

| Error class                     | Eligible?              |
|---------------------------------|------------------------|
| Network errors (timeout, DNS)   | Eligible — falls back  |
| Upstream 5xx                    | Eligible — falls back  |
| Upstream 429 (rate limit)       | Eligible — falls back  |
| Upstream 4xx (auth, validation) | Terminal — no fallback |
| Decode/encode errors            | Terminal — no fallback |
| Capability mismatch             | Terminal — no fallback |

**Provider-specific notes:**

* Copilot's auth-refresh path returns 401 on stale tokens; AgentShim's
  existing token-refresh logic (the `CopilotTokenManager` actor)
  handles this **before** the resilience layer sees it. A genuine 401
  observed by the resilience layer (revoked Copilot subscription,
  missing entitlement) is therefore treated as terminal — no fallback,
  surface to client.

**Streaming caveat (D4):** Once `provider.complete()` returns
`Ok(stream)` and bytes flow to the client, fallback is no longer
possible. Mid-stream failures surface as stream-level errors. This
matches v0.3 behavior; v0.4 does not introduce buffering.
