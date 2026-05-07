# Plan 04 — Responses Vision Tier-1 + Safety Ratings Promotion + v0.3 Docs

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-07-phase-3-responses-frontend-design.md`](../specs/2026-05-07-phase-3-responses-frontend-design.md) (decisions D4, D7).

**Goal:** Close the Phase 3 vision matrix: 4 active Responses → backend cells + 1 capability-gate negative. Promote `extensions["gemini.safety_ratings"]` to a typed canonical `Usage.safety_ratings` field per ADR-0003. Refresh project documentation for v0.3.

This is the closing plan: every cell in the (frontend × provider × modality) matrix is exercised; the canonical model gains its first new field since v0.1; docs land at v0.3 reality.

**Architecture:**
- 4 vision cells reuse the `BinarySource` plumbing from Phase 2 — no encoder changes needed in providers.
- Capability gate already covers DeepSeek (Phase 2 work); we add a Responses-specific test cell for completeness.
- ADR-0003 promotes `safety_ratings` to typed: `Usage.safety_ratings: Option<Vec<SafetyRating>>` with `SafetyRating { category, probability }` open-enum fields. Gemini provider double-writes during transition.
- Documentation: README capability matrix bumps to v0.3, "What's NOT in" updates, CHANGELOG.md gets a v0.3.0 entry, per-provider docs cross-link.

**Tech stack:** No new dependencies.

---

## File Structure

`crates/core/src/`:
- Modify: `usage.rs` (add `safety_ratings: Option<Vec<SafetyRating>>`)
- Create: `safety.rs` (~40 lines: `SafetyRating`, `SafetyCategory`, `SafetyLevel` open enums)
- Modify: `lib.rs` (re-export `SafetyRating` etc.)

`crates/providers/src/gemini/`:
- Modify: `response.rs` (populate both the typed field and the legacy extension key for one minor)

`crates/frontends/src/openai_responses/`:
- Verify (no expected change): vision input parts already decode (`input_image` / `image_url`).

`crates/protocol-tests/`:
- Create: `tests/responses_vision_oai_compat.rs`
- Create: `tests/responses_vision_copilot.rs`
- Create: `tests/responses_vision_anthropic.rs`
- Create: `tests/responses_vision_gemini.rs`
- Create: `tests/responses_capability_gate_deepseek.rs`
- Create: `fixtures/vision/responses/` per-cell fixtures

`docs/`:
- Create: `docs/adr/0003-promote-safety-ratings.md`
- Modify: `docs/adr/0002-frozen-core-v0.2.md` (add a "Status: superseded by ADR-0003 for safety_ratings" note at the top of the safety_ratings section)
- Modify: `docs/providers/gemini.md` (note the typed field; deprecate the extension key reference to v0.3.1)
- Modify: `README.md` (capability matrix v0.3, "What's NOT in v0.3", releases section bumps to v0.3.0)
- Modify: `docs/architecture.md` (capability matrix mention, frozen-core caveat updated)
- Modify: `docs/contributing.md` (canonical-model promotion process)

Root:
- Modify: `CHANGELOG.md` (new v0.3.0 entry)
- Modify: `Cargo.toml` (workspace `version` from `0.3.0-dev` → `0.3.0`)

---

## Tasks

### Task 1: SafetyRating canonical type

- [ ] Create `crates/core/src/safety.rs` with:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(tag = "category", content = "probability")]
  pub struct SafetyRating {
      pub category: SafetyCategory,
      pub probability: SafetyLevel,
  }

  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(rename_all = "SCREAMING_SNAKE_CASE")]
  pub enum SafetyCategory {
      HateSpeech,
      Harassment,
      DangerousContent,
      SexuallyExplicit,
      // Open-enum: unknown values round-trip via Other.
      #[serde(other)]
      Other,
  }

  // Same shape for SafetyLevel: Negligible, Low, Medium, High, Other.
  ```
- [ ] Add `safety_ratings: Option<Vec<SafetyRating>>` to `Usage` struct in `usage.rs`. Default `None`.
- [ ] Re-export from `lib.rs`.
- [ ] Add a `proptest_roundtrip` case to verify `SafetyRating` serde round-trips (including `Other` variants).

**Acceptance:** `cargo build -p agent-shim-core` succeeds. `cargo nextest run -p agent-shim-core` passes with new test.

### Task 2: Gemini provider double-writes safety ratings

- [ ] In `crates/providers/src/gemini/response.rs`, when extracting `safetyRatings`:
  - Populate `Usage.safety_ratings = Some(parsed_vec)`.
  - Keep populating `extensions["gemini.safety_ratings"]` with the same data (one minor of overlap).
  - Add a `// REMOVE in v0.3.1` comment at the legacy write site.
- [ ] Update existing Gemini tests that read the extension key to also assert the typed field is populated identically.

**Acceptance:** `cargo nextest run -p agent-shim-providers gemini` passes; both fields populated.

### Task 3: ADR-0003

- [ ] Create `docs/adr/0003-promote-safety-ratings.md`:
  - Context: Phase 2 ADR-0002 explicitly listed safety_ratings as the v0.3 promotion candidate.
  - Decision: typed canonical field as defined in Task 1.
  - Migration: one minor of double-write (v0.3.0 writes both; v0.3.1 drops extension key).
  - Open enum: unknown categories/levels round-trip via `Other(String)` so future Gemini changes don't break consumers.
- [ ] Update ADR-0002's safety_ratings section with a "Superseded by ADR-0003" note.

**Acceptance:** Both ADR docs are consistent.

### Task 4: 4 active vision cells

- [ ] `responses_vision_oai_compat.rs`: Responses request with `input_image` (data URI) → mockito serves OAI-compat `chat.completions` → assert outbound body contains the `image_url` part shape; assert response.completed reaches encoder.
- [ ] `responses_vision_copilot.rs`: same pattern with Copilot upstream (uses Copilot Responses-API path or canonical chat path; verify which based on `use_responses_api` logic in `github_copilot/mod.rs:188`).
- [ ] `responses_vision_anthropic.rs`: Responses request with image → AnthropicProvider canonical path → mockito serves Anthropic Messages SSE → encoder produces correct Responses output.
- [ ] `responses_vision_gemini.rs`: Responses request with image → GeminiProvider → mockito serves Gemini response → encoder produces correct Responses output.

Each test asserts BOTH:
- The outbound mocked body contains the expected provider-specific image shape.
- The inbound encoded response contains usage and the response.completed event.

**Acceptance:** All 4 tests pass.

### Task 5: Responses capability gate (DeepSeek)

- [ ] Write `tests/responses_capability_gate_deepseek.rs` using the same `TextOnlyStubProvider`-style pattern as `crates/gateway/tests/vision_capability_mismatch.rs`:
  - Mount a router with a Responses → DeepSeek-style stub route.
  - Send a Responses request with `input_image` content part.
  - Assert HTTP 400 response.
  - Assert error envelope shape matches the OpenAI Responses error contract:
    `{"error":{"message":"…","type":"invalid_request_error","code":"capability_mismatch"}}`.
  - Assert provider's `complete()` was never called (via panicking stub).

**Acceptance:** Test passes.

### Task 6: docs refresh

- [ ] **README.md**:
  - Capability matrix table bumps to v0.3 — Responses row gets ✅ for OAI-compat / Copilot / Anthropic / Gemini and ❌ gate for DeepSeek.
  - "What's NOT in v0.3" replaces "What's NOT in v0.2"; updates entries (Phase 4: fallback / circuit / retry / rate-limit; Phase 5: metrics / hot-reload / OTel; Phase 6: audio / multi-account Copilot / OAuth Anthropic).
  - Releases section bullet updates to **v0.3.0** with one-line summary.
- [ ] **docs/architecture.md**: capability gate section already covers DeepSeek; add a brief note that Responses cells join the matrix in v0.3. Frozen-core paragraph references ADR-0003 promotion of safety_ratings.
- [ ] **docs/contributing.md**: add a small "Promoting an extension key to a typed canonical field" subsection documenting the ADR-0002 → ADR-0003 process as the template for future promotions.
- [ ] **CHANGELOG.md**: new `## [0.3.0] — 2026-MM-DD` entry summarizing Phase 3 deliverables.

**Acceptance:** All docs read cleanly; matrix is correct.

### Task 7: version bump to 0.3.0

- [ ] Bump workspace `version` in `Cargo.toml` from `0.3.0-dev` to `0.3.0`.
- [ ] Verify `EDITOR_VERSION` / `EDITOR_PLUGIN_VERSION` in `crates/providers/src/github_copilot/headers.rs` are `AgentShim/0.3.0`.
- [ ] Run `cargo build --release -p agent-shim` to verify clean build.

**Acceptance:** Release build succeeds.

---

## Definition of Done

- [ ] All tasks complete.
- [ ] `cargo nextest run --workspace` passes with new test count.
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] `cargo fmt --all` clean.
- [ ] All docs updated, ADR-0003 published.
- [ ] CHANGELOG has v0.3.0 entry.
- [ ] Release build succeeds with workspace version `0.3.0`.
