# Plan 02 — Responses → Anthropic

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-07-phase-3-responses-frontend-design.md`](../specs/2026-05-07-phase-3-responses-frontend-design.md) (decision D2).

**Goal:** Wire the OpenAI Responses frontend to the Anthropic backend through the canonical translation path. Codex (and any Responses-native agent) can talk to `claude-opus-4-7` direct via `api.anthropic.com`. The hybrid passthrough path stays Anthropic↔Anthropic only.

**Architecture:**
- No new code in `crates/providers/src/anthropic/` — the canonical encode/decode path already works for any frontend.
- Verify `proxy_raw` correctly returns `Ok(None)` for `FrontendKind::OpenAiResponses`, falling back to canonical.
- Verify the gateway dispatch wires Responses frontend to Anthropic backend with no special-cases.
- Reasoning round-trip: Anthropic emits `thinking` blocks → canonical `Reasoning` → Responses encoder (Plan 01) emits `response.reasoning.{delta,done}`.

**Tech stack:** No new dependencies.

---

## File Structure

`crates/providers/src/anthropic/`:
- Verify (no expected change): `mod.rs::proxy_raw` — must `Ok(None)` for `OpenAiResponses` so canonical path runs.
- Verify (no expected change): `request.rs` — already encodes any `CanonicalRequest`.

`crates/protocol-tests/`:
- Create: `tests/responses_to_anthropic_text.rs`
- Create: `tests/responses_to_anthropic_tools.rs`
- Create: `tests/responses_to_anthropic_reasoning.rs`
- Create: `fixtures/responses_to_anthropic/` (text/tools/reasoning per-cell fixtures)

`docs/`:
- Modify: `docs/providers/anthropic.md` — add a "Responses frontend" subsection documenting the canonical-only path.

---

## Tasks

### Task 1: verify proxy_raw short-circuit

- [ ] Read `crates/providers/src/anthropic/mod.rs::proxy_raw` and confirm:
  - `OpenAiResponses` returns `Ok(None)` (canonical path).
  - `AnthropicMessages` is the only kind that gets passthrough.
- [ ] If the current code passes through Responses by mistake (it likely doesn't but verify), fix it. Add a unit test for the short-circuit behavior.

**Acceptance:** Inspection report or 1-line diff if needed.

### Task 2: text streaming smoke

- [ ] Write `tests/responses_to_anthropic_text.rs` using mockito:
  - Mockito serves a real Anthropic Messages SSE response (use a captured fixture).
  - Build a `CanonicalRequest` with `frontend.kind = OpenAiResponses` and a single text input.
  - Call `AnthropicProvider::complete()`.
  - Pipe the resulting `CanonicalStream` through `OpenAiResponses::encode_stream`.
  - Assert: the encoded SSE matches the expected Responses event sequence (use byte-level comparison against `expected.sse` fixture).

**Acceptance:** Test passes; both text streaming and `response.completed` payloads are correct.

### Task 3: tool round-trip smoke

- [ ] Write `tests/responses_to_anthropic_tools.rs`:
  - Multi-turn input with `function_call` and `function_call_output` items.
  - Anthropic backend returns `tool_use` block (mocked SSE).
  - Encoder surfaces `function_call` output_item with streaming arguments.
- [ ] Verify round-trip: Anthropic `tool_use_id` ↔ Responses `call_id`.

**Acceptance:** Test passes; tool call argument deltas appear in correct order.

### Task 4: reasoning round-trip smoke

- [ ] Write `tests/responses_to_anthropic_reasoning.rs`:
  - Anthropic backend returns `thinking` blocks followed by `text` (mocked SSE with real shape).
  - Encoder surfaces `response.reasoning.{delta,done}` followed by `response.output_text.{delta,done}`.
- [ ] Verify the canonical `ReasoningDelta` events flow correctly.
- [ ] If Anthropic's `thinking` shape carries a `signature`, verify it's dropped silently (canonical path is lossy on signatures by design — D2).

**Acceptance:** Test passes; reasoning text reaches the Responses output.

### Task 5: cross-protocol smoke (sanity)

- [ ] Add one cross-protocol test: send a Responses request through gateway → Anthropic backend (text only, no mocking other than the provider's HTTP layer). Use the existing `vision_capability_mismatch.rs` integration-test pattern to mount a real `Router`.

**Acceptance:** Test passes; the response.completed event arrives; usage is non-zero.

### Task 6: docs

- [ ] In `docs/providers/anthropic.md`, add a "Responses frontend" section explaining:
  - This is canonical-path only (no passthrough).
  - Anthropic-only fields like signatures on thinking blocks are dropped.
  - Use the Anthropic Messages frontend if you need passthrough fidelity.

**Acceptance:** Docs section reads cleanly.

---

## Definition of Done

- [ ] All tasks complete.
- [ ] `cargo nextest run --workspace` passes with new test count.
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] `cargo fmt --all` clean.
- [ ] `docs/providers/anthropic.md` updated.
- [ ] No changes to `agent-shim-core`.
