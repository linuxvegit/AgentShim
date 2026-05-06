# `/v1/messages/count_tokens` — Design

**Status:** Proposed
**Date:** 2026-05-06
**Owner:** AgentShim gateway

## Problem

Claude Desktop (`claude-cli/2.1.121`, `agent-sdk/0.2.121`) preflights every
turn with `POST /v1/messages/count_tokens?beta=true`. AgentShim does not
implement that route, so axum returns the built-in 404 and the SDK aborts
before issuing any real `/v1/messages` request. The captured loopback
traffic in `claudeDesktop.pcapng` shows exactly one packet — the failed
preflight — with no follow-up.

A working `/v1/messages/count_tokens` is therefore a hard prerequisite for
Claude Desktop to talk to AgentShim at all. The Agent SDK uses the
returned `input_tokens` integer as a budgeting heuristic for context
compaction; it does not need exact parity with Anthropic's official count.

## Goals

- Make Claude Desktop work end-to-end against AgentShim.
- Implement `POST /v1/messages/count_tokens` with the documented Anthropic
  response shape: `{ "input_tokens": <number> }`.
- Stay backend-agnostic — produce the same answer regardless of which
  upstream the routed model would land on.
- Keep the change small: one frontend module, one gateway handler, one
  new dependency.

## Non-goals

- Exact parity with Anthropic's official count. The SDK uses the value as
  a threshold input (e.g. 75% / 90% of context window); ±10–20% accuracy
  is well within tolerance.
- Per-backend tokenizers. We deliberately use a single tokenizer for all
  routes.
- Forwarding to the upstream's own `count_tokens` endpoint. Adds latency
  and a second code path for marginal accuracy benefit.
- Authentication. The endpoint computes a local heuristic; no credentials
  are required and any provided are ignored.
- Rate limiting or caching. Not warranted for the local-loopback
  deployment AgentShim targets.

## Design decisions

### Local approximation everywhere

A single code path counts every request locally. No upstream call. No
`proxy_raw`. No new method on `BackendProvider`. Counting is a pure
function of the decoded `CanonicalRequest`.

### Single tokenizer: `tiktoken-rs` (cl100k_base)

One `tiktoken_rs::CoreBPE` instance, lazily initialized into a
`OnceCell` so the ~5MB encoder data loads once for the lifetime of the
process. Used via `encode_ordinary` so `<|...|>` literals in user
content do not blow up.

`cl100k_base` is OpenAI's BPE; it is not Anthropic's tokenizer. Empirical
spot-checks show it lands within ~10–15% of Anthropic counts on typical
Claude Desktop turns (system prompt + 5–20 messages + 5–10 tools). That
is well inside the SDK's compaction tolerance.

### Frontend-shaped, not provider-shaped

The new logic lives in
`crates/frontends/src/anthropic_messages/count_tokens.rs`. The
`anthropic_messages` decoder already turns Anthropic-flavored bodies
into `CanonicalRequest`; counting is a pure function over that type. The
boundary rule (frontends never import providers) is preserved — the new
module imports only from `core` and `tiktoken-rs`.

## HTTP contract

### Route

`POST /v1/messages/count_tokens`

The `?beta=true` query string Claude Desktop sends is ignored. axum
routes ignore query strings by default; we do not validate or read them.

### Request body

A subset of the Anthropic Messages request schema. The official
`count_tokens` API does not require `max_tokens`; our existing
`MessagesRequest` does. To reuse the existing `decode()` path
unchanged, the handler decodes a thin wrapper
`CountTokensRequest` that mirrors `MessagesRequest` but makes
`max_tokens: Option<u32>` and omits `stream`. Before delegating to
`decode::decode`, the handler injects `max_tokens: 0`.

Decoded fields the counter consumes:

- `model` — recorded for logging only; never sent anywhere.
- `system` (string or array-of-blocks form).
- `messages[]` with text, tool_use, tool_result, thinking,
  redacted_thinking content blocks.
- `tools[]` — each tool's `name`, `description`, `input_schema`.
- `tool_choice` if non-`auto`.

Fields explicitly ignored:

`max_tokens`, `temperature`, `top_p`, `top_k`, `stop_sequences`,
`stream`, `metadata`, `service_tier`.

### Headers

No auth required. We accept and ignore `x-api-key`, `Authorization`,
`anthropic-version`, `anthropic-beta`, all `x-stainless-*` headers. We
do not read, validate, or reflect them.

### Success response

`HTTP 200`, `Content-Type: application/json`:

```json
{ "input_tokens": 1234 }
```

Single field. No usage envelope, no model echo, no other keys — the SDK
only consumes `input_tokens`.

### Error responses

Reuses the existing `HandlerError → IntoResponse` mapping in
`crates/gateway/src/handlers/mod.rs`.

| Condition                                      | Status | Body                                              |
| ---------------------------------------------- | ------ | ------------------------------------------------- |
| Body is not valid JSON / missing fields        | 400    | `{"error":{"message":"invalid request body: ..."}}` |
| Unknown role / malformed content block         | 400    | same shape                                        |
| Tokenizer init failure or other internal error | 500    | `{"error":{"message":"..."}}`                     |

No streaming. Always unary.

## Counting algorithm

The counter is a pure function `count(req: &CanonicalRequest) -> u32`.
It walks the canonical request, tokenizes each piece of text using
`cl100k_base`, and adds per-piece structural overhead to approximate
Anthropic's framing.

### Per-piece structural overhead

Anthropic's actual count includes message framing tokens (role markers,
boundary tokens, etc.). We approximate with these constants. They live
in `count_tokens.rs` as `const` items so future tuning is one edit.

| Piece                                  | Tokens added (besides text content) |
| -------------------------------------- | ----------------------------------- |
| Each message                           | +4                                  |
| Each system instruction                | +4                                  |
| Each text block                        | 0                                   |
| Each tool_use block                    | +8 on top of name + serialized input |
| Each tool_result block                 | +6 on top of content                |
| Each reasoning / redacted_reasoning    | +4 on top of content                |
| Each tool definition                   | +10 on top of name + description + serialized schema |
| `tool_choice` (if non-`auto`)          | +6 (plus tokenized tool name for the `tool` form) |
| Image block                            | +200 (fixed; deliberate over-count) |

The image overhead is intentionally a flat over-approximation. We do
not have image dimensions in the canonical block, and under-counting
vision turns would let the SDK believe it has more context window
headroom than it does.

### What text gets tokenized per block type

| Block                | Counted text                                              |
| -------------------- | --------------------------------------------------------- |
| `Text`               | the text                                                  |
| `ToolCall`           | `name` + `serde_json::to_string(arguments)`               |
| `ToolResult`         | `serde_json::to_string(content)` (string, blocks, or null)|
| `Reasoning`          | the reasoning text                                        |
| `RedactedReasoning`  | `data.len() / 4` (opaque blob; estimated)                 |
| `Unsupported` (image)| flat +200 from the table; raw JSON is not tokenized       |

### Tools

For each `ToolDefinition`, count
`name + description.unwrap_or("") + serde_json::to_string(&input_schema)`,
plus the +10 structural overhead.

### Final number

A `u32` accumulator using `saturating_add` for every addition.
`u32::MAX` is unreachable in practice (~16GB request body required) but
saturating arithmetic guarantees no overflow panic and no silent wrap.

### Naming bridge

The "Decoded fields" list uses Anthropic wire names (`thinking`,
`redacted_thinking`); the algorithm operates on `CanonicalRequest`,
which uses `Reasoning` / `RedactedReasoning`. Same blocks, two names.
The decoder maps wire → canonical; the counter only sees canonical.

## File-level changes

### New files

- **`crates/frontends/src/anthropic_messages/count_tokens.rs`** (~150 lines)
  - `pub fn count(req: &CanonicalRequest) -> u32`
  - Module-private `count_text(s: &str) -> u32` using `OnceCell<CoreBPE>`.
  - Module-private `count_block(block: &ContentBlock) -> u32`.
  - Module-private `count_tool(t: &ToolDefinition) -> u32`.
  - The structural-overhead constants from the table above.
  - Inline `#[cfg(test)] mod tests` covering the unit cases listed below.

- **`crates/frontends/src/anthropic_messages/count_tokens_wire.rs`** (~30 lines)
  - `CountTokensRequest` mirroring `MessagesRequest` but with
    `max_tokens: Option<u32>` and no `stream`.
  - `into_messages_request(self) -> MessagesRequest` — injects
    `max_tokens: 0` so the existing `decode::decode` path is reused.

  *Alternative considered:* make `MessagesRequest::max_tokens` optional.
  Rejected — it would touch every existing test fixture and serializer
  in the Anthropic frontend.

- **`crates/gateway/src/handlers/anthropic_count_tokens.rs`** (~40 lines)
  - Thin axum handler. Decode body via `count_tokens_wire`, run
    `count_tokens::count`, return `Json({"input_tokens": n})`.
  - One info log line:
    `→ /v1/messages/count_tokens | model: <alias> | tokens: <n> | bodyBytes: <m> | <duration>`.
  - No call into the route resolver. No call into providers.

- **`crates/gateway/tests/count_tokens_smoke.rs`** (~80 lines)
  - Integration tests against an ephemeral port. See the test plan below.

### Modified files

- **`crates/frontends/src/anthropic_messages/mod.rs`** — add
  `pub mod count_tokens;` and `pub mod count_tokens_wire;`.
- **`crates/frontends/Cargo.toml`** — add `tiktoken-rs = "0.5"`.
- **`crates/gateway/src/handlers/mod.rs`** — add
  `pub mod anthropic_count_tokens;`.
- **`crates/gateway/src/server.rs`** — one new `.route(...)` line.

### Untouched

`core`, `providers`, `router`, `config`, `observability` — no changes.

### Total surface

- 3 new source files (~220 lines).
- 1 new test file (~80 lines).
- 4 modified files (~6 lines of actual change).
- 1 new dependency (`tiktoken-rs`).

## Testing

### Unit tests (in `count_tokens.rs`)

1. **Empty request** — `{ messages: [] }` → equals the system+message
   base overhead constants.
2. **Plain text message** — single `"hello world"` user message → equals
   `tokenizer.encode_ordinary("hello world").len() + PER_MESSAGE`. The
   expected value is computed at runtime against the same tokenizer to
   avoid baking in a magic number.
3. **System + 3 messages** — sum of parts equals whole.
4. **Tool definitions** — array of 2 tools with realistic schemas.
   Asserts schema JSON is included in the count.
5. **Tool-use + tool-result round-trip** — both contribute; ID strings
   are not double-counted.
6. **Image block** — exactly +200 overhead applied; raw JSON is not
   tokenized on top.
7. **Reasoning block** — text counted; signature extension not counted.
8. **Determinism** — same input twice → same output.

### Integration tests (in `crates/gateway/tests/count_tokens_smoke.rs`)

1. **Happy path** — POST minimal valid body → 200,
   `{"input_tokens": <number>}`, correct `Content-Type`.
2. **Claude Desktop replay** — POST the exact body from
   `claudeDesktop.pcapng` (the build-perf agent description). Returns
   200. Regression guard for the original bug.
3. **`?beta=true` ignored** — `/v1/messages/count_tokens?beta=true`
   behaves identically to no query string.
4. **`max_tokens` absent** — POST without `max_tokens` → 200.
5. **Malformed JSON** — POST `{` → 400 with the standard error envelope.
6. **Unknown role** — POST a message with `role: "system"` → 400.
   Confirms we share the `decode()` validation.
7. **Auth headers ignored** — POST with no `x-api-key` → 200.
   POST with the literal `x-api-key: agent shim` Claude Desktop sends → 200.

### Manual verification

After tests pass:

1. Run `agent-shim serve --config config/gateway.example.yaml`.
2. Replay the captured request:
   ```bash
   curl -X POST 'http://127.0.0.1:8787/v1/messages/count_tokens?beta=true' \
     -H 'Content-Type: application/json' \
     -d '{"model":"claude-opus-4-7","messages":[{"role":"user","content":"hello"}],"tools":[]}'
   ```
   Expect `{"input_tokens": <small number>}` and a 200, plus a log line.
3. Launch Claude Desktop pointed at AgentShim. Capture loopback traffic.
   Expect: count_tokens returns 200, a `/v1/messages` POST follows, a
   real conversation works.

### Out of scope

- Accuracy benchmarking against real Anthropic — useful but not
  blocking. Could live in a dev-only test gated on `ANTHROPIC_API_KEY`.
- Rate limiting and caching — not warranted for local-loopback usage.

### CI

Tests run under the existing `cargo nextest run --workspace` command.
No new CI configuration.
