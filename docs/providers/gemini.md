# Provider: Gemini (Google AI Studio)

Gemini-as-backend talks directly to
`generativelanguage.googleapis.com/v1beta` using the **Generate Content
API**. Unlike OpenAI-compatible upstreams, Gemini's wire format is
structurally distinct — there is no `messages` array of role-tagged
objects; instead the API expects a `contents` array of
`Content { role, parts }` entries with typed `Part` payloads (`text`,
`inlineData`, `fileData`, `functionCall`, `functionResponse`).

The provider does **not** share any code with the OpenAI-compatible
encoder/parser. It owns its own `request::build` (canonical → wire),
`stream` (JSON-array byte scanner), and `response::parse_*` (wire →
canonical) modules under `crates/providers/src/gemini/`.

| Inbound frontend | Outbound path | Reasoning blocks visible? |
|---|---|---|
| `anthropic_messages` | Canonical translation (Gemini Generate Content) | Yes — `thought:true` parts render as `thinking` events |
| `openai_chat` | Canonical translation (Gemini Generate Content) | No — reasoning is dropped by the OpenAI Chat encoder |
| `openai_responses` | Canonical translation (Gemini Generate Content) | Yes — `thought:true` parts render as `response.reasoning.{delta,done}` |

## Config Example

```yaml
upstreams:
  gemini:
    type: gemini
    api_key: "AIzaSy-your-ai-studio-api-key-here"
    # Optional overrides (defaults shown):
    # base_url: https://generativelanguage.googleapis.com/v1beta
    # request_timeout_secs: 30
    # default_headers: {}
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `type` | string | — | Must be `gemini` |
| `api_key` | secret | — | Sent as a `?key=...` query parameter — see "Authentication" below |
| `base_url` | string | `https://generativelanguage.googleapis.com/v1beta` | Override for proxies; `/models/{model}:generateContent` (or `:streamGenerateContent`) is appended |
| `default_headers` | map | `{}` | Operator-level header overrides |
| `request_timeout_secs` | u64 | `30` | Timeout for the upstream HTTP call |

## Capability Flags

The provider declares:

| Flag | Value | Meaning |
|---|---|---|
| `streaming` | `true` | `streamGenerateContent` returns a JSON array; the byte scanner emits one parsed object per server flush |
| `tool_use` | `true` | `functionDeclarations` / `functionCall` / `functionResponse` cross-encode through the canonical mapping |
| `vision` | `true` | Inline base64 (`inlineData`) and URL-form (`fileData`) image inputs both translate from `BinarySource` |
| `json_mode` | `true` | `response_format: json_object` and `json_schema` map onto `responseMimeType: application/json` (+ `responseSchema`) |

## Sample Routes

### OpenAI Chat frontend → Gemini Flash (general use)

```yaml
routes:
  - frontend: openai_chat
    model: gemini-2.0-flash
    upstream: gemini
    upstream_model: gemini-2.0-flash
```

A standard chat-completions setup. Tool definitions, tool choice, and
tool-call argument deltas all work through this seam — note that
Gemini emits **complete** tool-call args per chunk (not a streaming
JSON fragment, unlike OpenAI), so the canonical
`ToolCallArgumentsDelta` carries the entire stringified args in one
shot.

### Anthropic frontend → Gemini (visible thinking)

```yaml
routes:
  - frontend: anthropic_messages
    model: gemini-2.5-flash-thinking
    upstream: gemini
    upstream_model: gemini-2.5-flash-thinking
    reasoning_effort: medium
```

Recommended when you want the agent to see the model's reasoning
trace. Gemini's thinking-capable models emit `thought:true` parts
inside `candidates[].content.parts`; the parser routes them into a
canonical `Reasoning` block, and the Anthropic frontend encoder
renders that as `content_block_start type: thinking` events. Claude
Code displays them as a "thinking" indicator.

`reasoning_effort` is consulted by the request encoder — see "Thinking
Budget" below.

### OpenAI Responses frontend → Gemini (visible thinking)

```yaml
routes:
  - frontend: openai_responses
    model: gemini-2.5-flash-thinking
    upstream: gemini
    upstream_model: gemini-2.5-flash-thinking
    reasoning_effort: medium
```

The Responses event model has first-class `response.reasoning.delta` /
`response.reasoning.done` events, so `thought:true` parts surface to
the agent end-to-end. See "Responses frontend (v0.3)" below for the
full description of the surface.

### Responses frontend (v0.3)

As of v0.3.0, OpenAI Responses (`POST /v1/responses`) is wired as a
first-class inbound frontend for the Gemini backend, alongside the
existing `openai_chat` and `anthropic_messages` surfaces. The design
is captured in
`docs/superpowers/specs/2026-05-07-phase-3-responses-frontend-design.md`
and executed by Plan 03
(`docs/superpowers/plans/2026-05-07-03-responses-to-gemini.md`).

**Same encoder + parser.** Phase 3 is a verification milestone, not
new provider code. The Gemini provider is frontend-agnostic by
construction — the same `request::build` (canonical → wire) and
`response::parse_*` (wire → canonical) modules under
`crates/providers/src/gemini/` serve all three frontends. The
Responses frontend simply exercises a different encoder/decoder pair
on the inbound side; the gateway wiring point is identical.

**Thinking budget.** Responses requests carrying
`reasoning: { effort: "medium" }` flow through the canonical
`generation.reasoning.effort` field and onto Gemini's
`generationConfig.thinkingConfig.thinkingBudget` via the same effort
table documented in "Thinking Budget" below. The three precedence
sources (explicit `budget_tokens`, route-resolved effort, request
effort) all apply unchanged. When a budget is emitted,
`includeThoughts: true` is set so the response carries `thought:true`
parts.

**Reasoning round-trip.** Gemini's `thought:true` parts → canonical
`ContentBlock::Reasoning` → the Responses encoder emits
`response.reasoning.delta` for each text fragment and
`response.reasoning.done` when the block closes. Unlike the OpenAI
Chat encoder (which has no reasoning event today and drops the block),
the Responses event model surfaces reasoning to the agent.

**Tool round-trip.** `function_call` and `function_call_output` items
in a Responses `input` array decode onto canonical `ToolCall` /
`ToolResult` blocks, which then encode to Gemini's `functionCall` /
`functionResponse` parts. Gemini's `functionResponse` requires a
function `name` (not just a call id) — the existing
`HashMap<id → name>` lookup over prior `ToolCall` blocks in the same
request handles this transparently for Responses inputs as well, so
the same tool-name-recovery path documented in "Tool-Call Translation"
below applies.

**Safety ratings.** For v0.3.0, Gemini's `candidates[].safetyRatings`
continue to travel as `extensions["gemini.safety_ratings"]` on the
first content block (per ADR-0002, "Provider-Specific Data
(Frozen-Core)" below), and are **not** yet surfaced on the encoded
Responses SSE — there is no `response.completed.safety_ratings` field
emitted today. v0.3.1 (Plan 04 / ADR-0003) will promote safety ratings
to a typed canonical `Usage.safety_ratings` field, at which point the
Responses encoder will land them on `response.completed`. Until then,
the regression test at
`crates/protocol-tests/tests/responses_to_gemini_safety_ratings.rs`
verifies the data is preserved at the parser level and not silently
dropped.

## Behavior

### Authentication

AI Studio authenticates with an API key supplied as a `?key=...`
**query parameter** on every request — there is no
`Authorization` header and no `x-api-key` header. The provider's
`AiStudioAuth::apply` helper attaches the key via
`reqwest::RequestBuilder::query`, which URL-encodes the value
correctly even when the key contains reserved characters. The key is
never logged.

### Endpoint

Two URLs, picked at request time based on `req.stream`:

* **Streaming**: `<base_url>/models/{model}:streamGenerateContent`
* **Unary**:     `<base_url>/models/{model}:generateContent`

With the default `base_url` of
`https://generativelanguage.googleapis.com/v1beta`, the resolved
streaming URL for `gemini-2.0-flash` is
`https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:streamGenerateContent`.
A trailing slash on `base_url` is tolerated.

> **Note:** AI Studio supports SSE on the streaming endpoint via
> `?alt=sse`, but the provider deliberately keeps the default
> JSON-array framing. Adding `?alt=sse` would change the wire format
> and require a different parser. If a future ADR opts into SSE, the
> choice belongs on `BackendTarget` / `RoutePolicy`, not silently in
> the URL helper.

### Streaming

The streaming endpoint returns a **single JSON array** of
`GenerateContentResponse` objects, with the server flushing each one
as the model produces it. This is *not* SSE — `eventsource-stream`
(used by every other parser in this crate) doesn't apply.

The provider ships a custom byte-level scanner
(`crates/providers/src/gemini/stream.rs`) that handles chunk splits
at *any* byte boundary: mid-object, mid-string, mid-escape sequence,
between objects, and immediately before/after `[` or `]`. The scanner
is exhaustively tested by splitting a sample body at every possible
position and confirming identical output.

### Tool-Call Translation

Standard tool definitions (`name`, `description`, `parameters`) flow
through `request::build` as a single `tools[0].functionDeclarations`
array. Tool calls in the response (`functionCall { name, args }`)
become `ContentBlock::ToolCall` with
`ToolCallArguments::Complete { value }` — Gemini ships args as a real
JSON object, **not** a stringified one (unlike OpenAI), so no extra
JSON parse round-trip is needed.

Tool *results* (`role: "tool"` messages on inbound canonical) require
a function name in Gemini's `functionResponse`. The encoder builds a
`HashMap<id → name>` over prior `ToolCall` blocks in the same request
to look it up. If the originating call can't be found (e.g. an orphan
tool result), the encoder falls back to the `tool_call_id` string —
the request will likely fail upstream, but it's preferable to dropping
the block silently.

### Thinking Budget

Gemini's "thinking" feature is controlled by
`generationConfig.thinkingConfig.thinkingBudget` (token count) and
`generationConfig.thinkingConfig.includeThoughts` (boolean). The
encoder picks a budget from three possible sources, in this order
(highest precedence first):

1. `req.generation.reasoning.budget_tokens` — Anthropic-style
   explicit token count (e.g. when the inbound is Anthropic Messages
   with `thinking: { budget_tokens: 1024 }`).
2. `req.resolved_policy.reasoning_effort` — route default, populated
   by `RoutePolicy::resolve` when an inbound effort wasn't supplied.
3. `req.generation.reasoning.effort` — request-level qualitative
   effort.

Sources (2) and (3) map effort levels to concrete token budgets:

| Canonical effort | Gemini `thinkingBudget` |
|---|---|
| `minimal` | `128` |
| `low` | `256` |
| `medium` | `1024` |
| `high` | `4096` |
| `xhigh` | `16384` |

Whenever a budget is emitted, `includeThoughts` is set to `true` so
the response carries `thought:true` parts that the parser can route
into `ContentBlock::Reasoning`. When no source supplies a budget,
`thinkingConfig` is omitted and the upstream uses its model-specific
default.

### Vision

Inbound canonical `ContentBlock::Image` (`Audio`, `File`) maps onto
two Gemini Part shapes depending on the source:

| Canonical `BinarySource` | Gemini Part |
|---|---|
| `Base64 { media_type, data }` | `inlineData { mimeType, data }` (base64-encoded) |
| `Bytes { media_type, data }` | `inlineData` (encoded as base64) |
| `Url { url }` | `fileData { fileUri }` (no media type — Gemini infers it) |
| `ProviderFileId { file_id }` | `fileData { fileUri: file_id }` |

On the response side, `inlineData` round-trips back through base64
decode into `BinarySource::Base64` so the canonical bytes match.

### Provider-Specific Data (Frozen-Core)

Per ADR-0002, the canonical model never grows new variants for
provider-specific fields. Gemini's data without a canonical home lands
on the **first content block's** `extensions` map under `gemini.*`
keys:

| Wire field | Extension key |
|---|---|
| `candidates[].safetyRatings` | `gemini.safety_ratings` |
| `candidates[].citationMetadata` | `gemini.citation_metadata` |

`promptFeedback` is currently dropped on the unary path; on the
streaming path it could surface as a `RawProviderEvent` in a future
revision (today it's not exposed).

### Stop-Reason Mapping

| Gemini `finishReason` | Canonical `StopReason` |
|---|---|
| `STOP` | `EndTurn` (or `ToolUse` if any function call was emitted) |
| `MAX_TOKENS` | `MaxTokens` |
| `SAFETY` | `ContentFilter` |
| `RECITATION` | `ContentFilter` |
| (other) | `Other(string)` |
| (missing) | `EndTurn` |

### Lossy fields

The following Anthropic-frontend concepts do **not** round-trip when
the inbound frontend is `anthropic_messages` and the outbound is
Gemini:

* `cache_control` markers — silently dropped. Gemini does not expose
  a user-facing prompt-cache surface today.
* Anthropic-style `signature` deltas on thinking blocks — Gemini does
  not emit signatures; reasoning is delivered via `thought:true`
  parts only.
* `RedactedReasoning` blocks — no Gemini representation; dropped by
  the encoder.

### Error Handling

HTTP non-2xx responses from the upstream surface as
`ProviderError::Upstream { status, body }`, carrying the original
status code and response body for the frontend to format into a
protocol-shaped error. The body is also logged at `warn!` level with
the upstream status.

### Retries

The provider does **not** retry automatically. Retry logic belongs in
the caller or at the infrastructure layer.

### Resilience behavior (v0.4+)

The Phase 4 retry/fallback layer treats this provider's errors as
follows:

| Error pattern | Default eligibility |
|---|---|
| Connect/DNS/TLS failures (`Network`) | **Eligible** (retry / fall back) |
| HTTP 5xx (`Upstream{status>=500}`) | **Eligible** |
| HTTP 429 (`Upstream{status=429}`) | **Eligible** |
| HTTP 401/403/422 etc. | **Terminal** (return to client) |
| Decode errors (malformed bytes) | **Terminal** |

Provider-specific notes:

* AI Studio occasionally returns HTTP 400 with a `SAFETY` block reason
  on prompts that fail Gemini's safety filter. These are **terminal**
  under the default classifier — the request itself violated policy,
  and the next upstream would likely reject it the same way.
