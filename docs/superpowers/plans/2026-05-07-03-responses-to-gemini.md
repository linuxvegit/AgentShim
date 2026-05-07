# Plan 03 — Responses → Gemini

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-07-phase-3-responses-frontend-design.md`](../specs/2026-05-07-phase-3-responses-frontend-design.md) (decision D3).

**Goal:** Wire the OpenAI Responses frontend to the Gemini backend (AI Studio Generate Content API). Codex can talk to `gemini-2.5-flash-thinking` etc. via Responses. Thinking and `safetyRatings` round-trip through the Responses event model.

**Architecture:**
- No new code in `crates/providers/src/gemini/` — the provider is frontend-agnostic.
- Verify Gemini's `thoughts: true` reasoning parts surface as `response.reasoning.{delta,done}` (relies on Plan 01's encoder work).
- Verify Gemini's `safetyRatings` reach `response.completed` via canonical `Usage` (today via the extension key; see Plan 04 for typed promotion).
- Verify multi-turn `function_call` / `function_call_output` items in Responses `input` round-trip to Gemini `functionCall` / `functionResponse` parts.

**Tech stack:** No new dependencies.

---

## File Structure

`crates/providers/src/gemini/`:
- Verify (no expected change): `request.rs`, `response.rs`, `mod.rs` — should already work.

`crates/protocol-tests/`:
- Create: `tests/responses_to_gemini_text.rs`
- Create: `tests/responses_to_gemini_tools.rs`
- Create: `tests/responses_to_gemini_thinking.rs`
- Create: `tests/responses_to_gemini_safety_ratings.rs`
- Create: `fixtures/responses_to_gemini/` (per-cell fixtures)

`docs/`:
- Modify: `docs/providers/gemini.md` — add a "Responses frontend" subsection.

---

## Tasks

### Task 1: text streaming smoke

- [ ] Write `tests/responses_to_gemini_text.rs` using mockito:
  - Mockito serves a Gemini `:streamGenerateContent` JSON-array response (use captured fixture).
  - `CanonicalRequest` with `frontend.kind = OpenAiResponses`.
  - Call `GeminiProvider::complete()`, pipe through `OpenAiResponses::encode_stream`.
  - Assert encoded SSE matches expected Responses event sequence.

**Acceptance:** Test passes; verify usage tokens reach `response.completed`.

### Task 2: tool round-trip smoke

- [ ] Write `tests/responses_to_gemini_tools.rs`:
  - Multi-turn input: `function_call` + `function_call_output` items.
  - Decoder converts those to canonical `ToolCallBlock` / `ToolResultBlock`.
  - Gemini provider's request encoder produces `functionCall` and `functionResponse` parts.
  - Mockito returns a Gemini response containing a new `functionCall` part.
  - Encoder surfaces the new function_call as a Responses output_item.

**Acceptance:** Test passes; verify the outbound request to Gemini contains the expected `functionResponse` shape; verify the inbound new tool call has the correct `call_id`.

### Task 3: thinking round-trip

- [ ] Write `tests/responses_to_gemini_thinking.rs`:
  - Use `gemini-2.5-flash-thinking` model (capability `vision: true`, thinking-capable).
  - Request includes `reasoning: { effort: "medium" }` — verify it maps to `thinkingConfig.thinkingBudget` per the documented table.
  - Mockito returns a streaming response with `thoughts: true` parts mixed with regular content.
  - Encoder surfaces `response.reasoning.{delta,done}` followed by `response.output_text.{delta,done}`.

**Acceptance:** Test passes; reasoning interleaving works correctly.

### Task 4: safety ratings smoke

- [ ] Write `tests/responses_to_gemini_safety_ratings.rs`:
  - Mockito returns a response with `safetyRatings` array on the `candidates[0]`.
  - Verify the canonical `extensions["gemini.safety_ratings"]` is populated.
  - Verify the Responses encoder includes the safety ratings in `response.completed` extensions or in a `metadata` field (TBD by Plan 04 promotion — for now, just verify the data flows through canonical and isn't silently dropped).

**Acceptance:** Test passes; data is observable in the encoded response.

### Task 5: docs

- [ ] In `docs/providers/gemini.md`, add a "Responses frontend" subsection explaining:
  - Same encoder + parser as OpenAI Chat / Anthropic frontends.
  - Thinking budget mapping reused.
  - SafetyRatings flow through; v0.3 promotes to a typed canonical field (link to Plan 04 or ADR-0003).

**Acceptance:** Docs section reads cleanly.

---

## Definition of Done

- [ ] All tasks complete.
- [ ] `cargo nextest run --workspace` passes with new test count.
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] `cargo fmt --all` clean.
- [ ] `docs/providers/gemini.md` updated.
- [ ] No changes to `agent-shim-core` (yet — that comes in Plan 04).
