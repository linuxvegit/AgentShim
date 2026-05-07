# Plan 01 — Responses Event Model Audit

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-07-phase-3-responses-frontend-design.md`](../specs/2026-05-07-phase-3-responses-frontend-design.md) (decisions D1, D5, D6).

**Goal:** Tighten the OpenAI `/v1/responses` frontend's event model against real OpenAI SSE captures. Add reasoning round-trip on encode (the v0.2 implementation drops `ReasoningDelta` silently — see `encode_stream.rs:407-409`). Add reasoning items on decode for multi-turn input. Add tool round-trip verification with golden fixtures.

This is the foundation plan: every later Phase 3 plan tests vision/Anthropic/Gemini through the encoder this plan hardens.

**Architecture:**
- No canonical model changes (canonical already has `ContentBlock::Reasoning` and `StreamEvent::ReasoningDelta`).
- New wire types in `frontends/src/openai_responses/wire.rs`: `ReasoningItem` (inbound), `ReasoningDelta` / `ReasoningDone` SSE payloads (outbound), `OutputContent::Reasoning` variant.
- `encode_stream.rs` ReasoningDelta arm emits `response.reasoning.delta` / `response.reasoning.done`. ContentBlockStop for reasoning kind closes the part.
- `decode.rs` accepts `{"type": "reasoning", ...}` items in input arrays, decoding to `ContentBlock::Reasoning` attached to the preceding assistant message.

**Tech stack:** No new dependencies.

---

## File Structure

`crates/frontends/src/openai_responses/`:
- Modify: `wire.rs` (add `InputItem::Reasoning`, `OutputContent::Reasoning`, `ReasoningDeltaPayload`, `ReasoningDonePayload`)
- Modify: `decode.rs` (handle `InputItem::Reasoning` in `decode_input`)
- Modify: `encode_stream.rs` (emit `response.reasoning.{delta,done}`; new `output_item` for reasoning kind)
- Modify: `encode_unary.rs` (include reasoning content parts in output array)

`crates/protocol-tests/`:
- Create: `tests/responses_event_model_audit.rs` (golden SSE comparisons against committed captures)
- Create: `fixtures/responses/text_simple.{request,upstream,expected}.{json,sse}`
- Create: `fixtures/responses/tools_parallel.{request,upstream,expected}.{json,sse}`
- Create: `fixtures/responses/reasoning_o1.{request,upstream,expected}.{json,sse}`

---

## Tasks

### Task 1: wire types for reasoning round-trip

- [ ] Add `InputItem::Reasoning { id: Option<String>, content: String, summary: Option<String> }` variant in `wire.rs`. Match the real OpenAI shape per their docs (https://platform.openai.com/docs/api-reference/responses/object).
- [ ] Add `OutputContent::Reasoning { text: String, summary: Option<String> }` variant.
- [ ] Add `ReasoningDeltaPayload { item_id: String, output_index: u32, delta: String }`.
- [ ] Add `ReasoningDonePayload { item_id: String, output_index: u32, text: String }`.
- [ ] Add `OutputItem::Reasoning { id: String, status: &'static str, content: Vec<OutputContent> }`.

**Acceptance:** `cargo build -p agent-shim-frontends` succeeds.

### Task 2: decode reasoning items from input

- [ ] In `decode.rs::decode_input`, when iterating `Vec<InputItem>`, on `InputItem::Reasoning` push a `ContentBlock::Reasoning` to the preceding assistant message's content array. If no preceding assistant message exists (i.e., reasoning appears first), create a new assistant message with the reasoning block.
- [ ] Round-trip: `decode → encode` of a reasoning-bearing input must surface the reasoning content correctly.

**Acceptance:** Add unit test in `decode.rs::tests`: `reasoning_items_attach_to_preceding_assistant_message`.

### Task 3: encode reasoning deltas as Responses SSE events

- [ ] In `encode_stream.rs`, replace the `StreamEvent::ReasoningDelta { .. }` arm in the catch-all (line 408) with full handling.
- [ ] On `ContentBlockStart { kind: Reasoning, index }`: assign next `output_index`, create `OutputItem::Reasoning` with `status: "in_progress"`, emit `response.output_item.added`.
- [ ] On `StreamEvent::ReasoningDelta { index, text }`: emit `response.reasoning.delta` event with the new payload struct.
- [ ] On `ContentBlockStop { index }` for a reasoning block: emit `response.reasoning.done` with accumulated text, then `response.output_item.done`.
- [ ] Buffer accumulated reasoning text per output_index, parallel to existing `text_buf` and `tool_args_buf`.

**Acceptance:** Add unit test in `encode_stream.rs::tests`: `reasoning_delta_emits_responses_events`.

### Task 4: encode_unary support for reasoning

- [ ] In `encode_unary.rs`, when iterating canonical content blocks to build the `output` array, emit `OutputContent::Reasoning` parts for `ContentBlock::Reasoning` blocks.

**Acceptance:** Add unit test: unary response with reasoning block produces output array containing reasoning part.

### Task 5: golden fixtures + audit test

- [ ] Capture three OpenAI Responses SSE streams (or hand-craft them from real captures the user provides):
  - `text_simple` — plain text response, single message, usage at end.
  - `tools_parallel` — two parallel function calls, no text content.
  - `reasoning_o1` — o1 model output: reasoning items + text + usage.
- [ ] Write `tests/responses_event_model_audit.rs` that:
  - Loads a `request` fixture, decodes via `OpenAiResponses::decode_request`.
  - Asserts canonical structure looks right.
  - Builds a `CanonicalStream` from the matching `upstream` fixture (canonical event sequence).
  - Encodes via `OpenAiResponses::encode_stream`.
  - Compares output bytes against `expected.sse` (line-by-line; SSE event order matters).

**Acceptance:** All three audit tests pass; fixtures committed.

### Task 6: review existing test coverage gaps

- [ ] Run `rg "encode_stream\|encode_unary" crates/frontends/src/openai_responses/` and audit existing tests for coverage of:
  - Multi-turn `input` with both message-array and item-array formats.
  - `tool_choice: { type: "function", name: "..." }` decoding.
  - `instructions` field becoming a `SystemSource::OpenAiSystem` system block.
  - Empty `output` array on `response.completed` for tool-only responses.
- [ ] File any coverage gaps as new tests.

**Acceptance:** Coverage gaps either tested or documented as TODO with a tracking note.

---

## Definition of Done

- [ ] All tasks complete.
- [ ] `cargo nextest run -p agent-shim-frontends` passes (target: existing count + 4–6 new tests).
- [ ] `cargo nextest run -p agent-shim-protocol-tests` passes (target: existing count + 1 new responses_event_model_audit test).
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] `cargo fmt --all` clean.
- [ ] No changes to `agent-shim-core`.
