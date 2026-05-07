# Phase 3 (v0.3) — OpenAI Responses Frontend Full Coverage

**Status:** Draft (ready for grilling)
**Date:** 2026-05-07
**Source:** [`2026-04-28-agent-shim-design.md`](./2026-04-28-agent-shim-design.md) §9 Phase 3

---

## 1. Scope

Phase 3 finishes what Phase 2 left as TODO on the OpenAI `/v1/responses`
frontend: route it through every backend (not just OAI-compat / Copilot /
DeepSeek), wire vision through it, and round-trip the richer item-based
event model accurately.

**In scope:**

- **Responses → Anthropic** (canonical translation path).
- **Responses → Gemini** (Generate Content API). Includes thinking and
  safety-ratings round-trip in the Responses event model.
- **Responses × Vision Tier-1** — 5 active cells:
  OAI-compat / Copilot / Anthropic / Gemini, plus the DeepSeek
  capability-gate negative.
- **Responses event model audit** — `response.output_item.{added,done}`,
  `response.content_part.{added,done}`,
  `response.output_text.{delta,done}`,
  `response.function_call_arguments.{delta,done}`,
  `response.reasoning.delta`, `response.reasoning.done`,
  `response.completed`, `response.error` — verified against real OpenAI
  SSE captures.
- **Responses tool round-trip** — `function_call` items as both inbound
  (multi-turn `input` history) and outbound (assistant-emitted) round-trip
  to canonical `ToolCallBlock` and back. `function_call_output` items map
  to canonical `ToolResultBlock`.
- **Responses reasoning round-trip** — OpenAI Responses' `reasoning` item
  ↔ DeepSeek `reasoning_content` ↔ Gemini `thoughts: true` parts ↔
  Anthropic `thinking` blocks. Frontend surfaces a `reasoning` content
  part on the way out and accepts `reasoning` items on the way in for
  multi-turn continuity.
- **Promote `extensions["gemini.safety_ratings"]` to a typed canonical
  field** — `Usage.safety_ratings: Vec<SafetyRating>` (or similar). This
  is the v0.3 frozen-core exception described in ADR-0002. Migration is
  one-line per consumer (mostly tests).

**Deferred to v0.3.x point releases:**

- Responses background mode (`stream: false`, `store: true` polling) — we
  remain a stateless gateway; agents wanting persistence go to OpenAI
  directly.
- Responses `previous_response_id` server-side resume — we accept and
  ignore (already today). v0.3.x might wire client-side replay.
- Responses `include` field (logprobs, hidden reasoning) — accepted and
  ignored today.
- Responses computer-use / web-search / file-search built-in tools — out
  indefinitely (those are OpenAI hosted-tool features; we neither pass
  them through nor implement equivalents).

**Permanently out** (per parent spec §11): embeddings, moderation,
audio I/O, admin UI, end-user identity, billing, v0.1/v0.2 retro:
multi-account Copilot, OAuth Anthropic.

---

## 2. Locked Decisions

The eight decisions that shape Phase 3.

### D1. Responses-as-frontend stays stateless

We accept `previous_response_id`, `store`, `truncation`, `include` in the
request and silently ignore them. Agents using `previous_response_id` for
multi-turn must instead replay the full conversation in `input` (which
they already do for OpenAI Chat). Responses-mode "first-class server
state" is an OpenAI feature, not a gateway feature. This is the same
posture as v0.2.

### D2. Anthropic backend Responses path = canonical translation only

The hybrid passthrough+canonical pattern from v0.2 only applies when
inbound and outbound are both Anthropic Messages. When the inbound is
OpenAI Responses, the request flows through the canonical path
(`anthropic::request::build` → `anthropic::response::parse`). The hybrid
code in `anthropic/mod.rs::proxy_raw` short-circuits with `Ok(None)` for
`FrontendKind::OpenAiResponses`.

This is lossy for Anthropic-only fields (signatures, redacted thinking),
which is the cost of cross-protocol routing — same trade-off Phase 2
documented for OpenAI Chat → Anthropic.

### D3. Gemini backend Responses path = same encoder, same parser

Gemini's `BackendProvider::complete()` already takes a `CanonicalRequest`
and returns a `CanonicalStream`. The frontend kind doesn't affect the
provider's wire shape — only the *encoding* of the response stream
(which is the frontend's responsibility, not the provider's).

So enabling Responses → Gemini is mostly a matter of removing the "n/a in
v0.2" status. The work is verifying that:

1. Gemini's `thoughts: true` reasoning parts surface as Responses
   `reasoning` items on the encode side.
2. Gemini's `safetyRatings` reach the Responses `response.completed` payload.
3. Multi-turn input history (Responses `function_call` /
   `function_call_output` items in `input`) round-trips to Gemini's
   `functionCall` / `functionResponse` parts and back.

### D4. Vision: 4 active cells + 1 capability-gate cell

```
                    OAI-compat | Copilot | Anthropic | Gemini | DeepSeek
OpenAI Responses        ✅      |   ✅    |    ✅     |   ✅   | ❌ gate
```

The OpenAI Responses `input_image` content part already decodes
correctly today (Phase 2 verified) — it just hasn't been exercised
end-to-end through Anthropic/Gemini. Vision tests exercise both data-URI
and https URL `image_url` strings.

### D5. Reasoning round-trip = `response.reasoning.delta` + `reasoning` items

Inbound (multi-turn): when the request `input` array contains
`{"type": "reasoning", ...}` items (real shape verified against the
o-series), we decode them into `ContentBlock::Reasoning` on the
preceding assistant message. They round-trip to whatever the target
provider supports.

Outbound (assistant-emitted): when the canonical stream emits
`StreamEvent::ReasoningDelta`, we surface
`response.reasoning.delta` + `response.reasoning.done` SSE events
(today these are silently dropped — see `encode_stream.rs` line 408).
The done event contains the accumulated reasoning text.

When the target backend doesn't expose reasoning (OAI-compat, Copilot
chat models without `reasoning_effort`), the frontend simply emits no
reasoning events — the content blocks were never produced by the
provider. No fakery.

### D6. Tool round-trip via canonical `ToolCallBlock` + `ToolResultBlock`

Responses `function_call` items in `input` already decode to
`ContentBlock::ToolCall` (Phase 2 work, line 86 of `wire.rs`). Phase 3
adds:

1. `function_call_output` item decoding to `ContentBlock::ToolResult`
   (already in `wire.rs` line 92, but verify the message-attachment
   logic — the Responses contract is "function_call_output items belong
   to the user role's contribution to the conversation").
2. Outbound encoding: when the canonical stream emits a
   `ContentBlockStart { kind: ToolCall }` followed by `ToolCallStart`
   and arg deltas, the existing encoder emits a `function_call`
   output_item with streaming `function_call_arguments.delta`. This
   mostly works today; Phase 3 verifies it against multi-tool sequences
   (parallel calls, interleaved with text).

### D7. Promote `safety_ratings` to typed canonical field

ADR-0002 stated `extensions["gemini.safety_ratings"]` is provisional in
v0.2 and a typed canonical field arrives in v0.3 once empirical
cross-provider read patterns are clear. They are: every frontend that
exposes filter information needs to read the same field with the same
shape.

The field name is `Usage.safety_ratings: Option<Vec<SafetyRating>>`
where `SafetyRating { category: SafetyCategory, probability: SafetyLevel }`.
Both nested enums are open enums that round-trip unknown values via
`Other(String)`. ADR-0003 records the migration:

- New typed field added on `Usage`.
- Gemini provider populates BOTH the typed field AND
  `extensions["gemini.safety_ratings"]` for one minor version; v0.3.0
  drops the extension key.
- Tests reading the extension key get a one-line refactor.

### D8. Phase 3 = 4 plan files, frontend-major sequencing

```
2026-05-07-01-responses-event-model-audit.md     (R1: encode_stream + tests)
2026-05-07-02-responses-to-anthropic.md          (R2: canonical path through anthropic provider)
2026-05-07-03-responses-to-gemini.md             (R3: canonical path through gemini provider)
2026-05-07-04-responses-vision-and-safety-promotion.md  (R4: vision Tier-1 + ADR-0003 + docs)
```

Each plan is independently shippable. After Plan 01, agents see crisper
SSE events from existing routes. After Plan 02, Codex can run on Claude
direct. After Plan 03, Codex can run on Gemini. After Plan 04, vision
and the typed-safety-rating promotion ship.

---

## 3. Module Layout

No new top-level crates. Phase 3 is mostly verification + small encoder
additions.

```
crates/frontends/src/openai_responses/
  decode.rs           # MODIFY: reasoning items inbound
  encode_stream.rs    # MODIFY: response.reasoning.{delta,done} events
  encode_unary.rs     # MODIFY: include reasoning in output array
  mapping.rs          # (no change expected)
  wire.rs             # MODIFY: ReasoningItem in InputItem; ResponseReasoning in OutputContent

crates/providers/src/
  anthropic/
    mod.rs            # MODIFY: proxy_raw still returns None for OpenAiResponses (verify)
    request.rs        # (no change expected — already canonical)
    response.rs       # (no change expected)
  gemini/
    mod.rs            # (no change expected — provider is frontend-agnostic)
  deepseek/
    mod.rs            # (no change expected — capability gate handles vision)

crates/core/src/
  usage.rs            # MODIFY: add safety_ratings: Option<Vec<SafetyRating>>
  safety.rs           # NEW (~40 lines): SafetyRating, SafetyCategory, SafetyLevel enums

crates/protocol-tests/tests/
  responses_to_anthropic_text.rs            # NEW
  responses_to_anthropic_tools.rs           # NEW
  responses_to_anthropic_reasoning.rs       # NEW
  responses_to_gemini_text.rs               # NEW
  responses_to_gemini_tools.rs              # NEW
  responses_to_gemini_thinking.rs           # NEW
  responses_to_gemini_safety_ratings.rs     # NEW
  responses_vision_oai_compat.rs            # NEW
  responses_vision_copilot.rs               # NEW
  responses_vision_anthropic.rs             # NEW
  responses_vision_gemini.rs                # NEW
  responses_capability_gate_deepseek.rs     # NEW
  responses_event_model_audit.rs            # NEW (golden SSE captures)

docs/
  adr/
    0003-promote-safety-ratings.md          # NEW
  providers/
    (no new files; existing per-provider docs gain a "Responses frontend" section)
```

---

## 4. Test Strategy

Same three-rules pattern as Phase 2:

1. Fixture filenames follow `<scenario>.<role>.<ext>`.
2. `scripts/regen-fixtures.sh` is extended with a `responses` provider
   group capturing OpenAI's real `/v1/responses` SSE stream.
3. Cross-protocol tests assert canonical events at the boundary, not
   byte equality. The Responses encode_stream tests are the exception:
   those compare against golden SSE captures because the *shape* of the
   Responses event sequence is the contract under test.

Vision smoke tests follow the same pattern as Phase 2's
`vision_smoke.rs`:

- Mockito serves the upstream wire shape.
- Test sends a Responses request with `input_image` content part.
- Asserts: outbound mockito body contains the expected image part shape,
  inbound canonical events look right, response.completed has correct
  usage.

The capability-gate test follows the same `TextOnlyStubProvider` panic
pattern as `crates/gateway/tests/vision_capability_mismatch.rs`.

Live e2e: nightly workflow extended with `responses_*_live` cases when
`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY` are present.

---

## 5. Risks and Mitigations

**R1. OpenAI changes the Responses event names mid-flight.**
The Responses API is younger than Chat Completions and OpenAI has revised
event names twice already. Mitigation: golden-fixture tests pin the
contract per release; CHANGELOG documents the OpenAI API version we
target; live nightly catches drift early.

**R2. Multi-turn `input` array with mixed item types is brittle.**
Responses `input` accepts strings, message arrays, and typed-item arrays
(per `wire.rs::InputField`). Mixed sequences (message → function_call →
function_call_output → message → reasoning) are the hard case.
Mitigation: build a small property test that generates random
`Vec<InputItem>` shapes and asserts decode → encode roundtrip preserves
canonical-event ordering.

**R3. ADR-0003 promotion creates a transitional double-write.**
For one v0.3 minor, Gemini provider populates both the typed field and
the legacy extension key. Risk: consumers read both, see the same data,
no problem; but if a future Phase 3 patch flips writes off without
updating consumers, silent data loss. Mitigation: deprecation comment
in `gemini/response.rs` says "remove on v0.3.1 release"; CI grep gate
fails if `gemini.safety_ratings` extension write survives past that
tag.

**R4. Reasoning round-trip when the agent sends reasoning items it
can't author.**
Agents rarely emit `reasoning` items in `input` themselves — those come
from previous turns. But if a third-party tool synthesizes them, the
content might violate provider expectations (e.g., Anthropic rejects
`thinking` blocks without valid signatures from the same model).
Mitigation: when forwarding `reasoning` items in Anthropic-bound
requests, wrap them as `<reasoning>...</reasoning>` text within a
`text` content block rather than as `thinking` blocks. Matches the
behavior OpenAI uses when targeting non-reasoning models.

**R5. Vision + Responses interaction with OpenAI's image part shape
versions.**
Responses uses `input_image: { image_url: "..." }` whereas Chat uses
`image_url: { url: "..." }`. Wire-shape mismatch is already handled in
`decode.rs::decode_input`. Mitigation: explicit test for both shapes;
fixture captures lock the contract.

---

## 6. Out of Scope for v0.3

**Frontends:** No new frontends. Responses is the focus.

**Providers:** No new providers in v0.3. Qwen/Kimi/Doubao/Grok continue
to ride OpenAI-compat. Native adapters defer to v0.4 if at all.

**Cross-cutting features deferred to v0.4+:**
- Fallback chains, circuit breakers, per-route retries — Phase 4
- Per-key rate limiting, request budget caps, per-agent API keys — Phase 4
- Cost/latency-aware routing — Phase 4 (optional)
- Prometheus metrics, hot-reload config, OpenTelemetry — Phase 5
- Audio / file content end-to-end — Phase 6+ if at all
- Multi-account Copilot — Phase 6
- OAuth Anthropic ("Workbench" tokens) — Phase 6
- Vertex Anthropic / Bedrock Anthropic — Phase 4 candidate

**Definition of done (v0.3.0):**
- All Phase 3 plan tasks complete.
- 439 + new tests all pass.
- Capability matrix in README updated to v0.3 (Responses row fully
  populated).
- `extensions["gemini.safety_ratings"]` write retained for one
  transitional minor; deprecation tracked.
- Per-provider docs each gain a "Responses frontend" section.
- ADR-0003 published.
