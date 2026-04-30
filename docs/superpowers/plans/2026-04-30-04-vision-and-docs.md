# Plan 04 — Vision Tier-1 + Documentation Refresh

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-04-30-phase-2-provider-breadth-design.md`](../specs/2026-04-30-phase-2-provider-breadth-design.md) (decisions D6, D10).

**Goal:** Wire end-to-end vision (Tier-1 matrix) across the v0.2 provider set and refresh project documentation. This is the transverse plan that depends on Plans 01–03 having landed; vision cells exercise every provider.

**Architecture:**
- 7 active encoder cells + 1 passthrough cell, all driven by the existing `BinarySource` enum and `ContentBlock::Image` variant — **no canonical model changes**, per ADR-0002.
- New capability gate at the routing/provider boundary: when `req` contains an `Image` block and target provider's `capabilities.vision == false`, raise `ProviderError::CapabilityMismatch` *before* any network call. Frontend renders a 400 in its dialect.
- Documentation refresh covers: (a) per-provider docs for Anthropic/DeepSeek/Gemini, (b) capability matrix in README, (c) updated architecture / contributing pages, (d) `scripts/regen-fixtures.sh` for ongoing fixture maintenance, (e) opt-in nightly live e2e workflow.

**Tech stack:** No new dependencies. Test image is a tiny PNG (~2KB) committed to fixtures.

**Core changes:** NONE.

---

## File Structure

`crates/providers/`:
- Modify: `crates/providers/src/openai_compatible/` (vision encoder for `image_url` parts; should be mostly there already from v0.1 — verify and add tests)
- Modify: `crates/providers/src/github_copilot/` (vision; same shape as OAI-compat)
- Modify: `crates/providers/src/anthropic/request.rs` (encoder for Anthropic image source format on canonical path)
- Modify: `crates/providers/src/anthropic/passthrough.rs` (no encoder change — passthrough is already lossless)
- Modify: `crates/providers/src/gemini/request.rs` (encoder for `inline_data` parts — likely already done in Plan 03 Task 4)
- Modify: `crates/providers/src/deepseek/mod.rs` (set `capabilities.vision = false` explicitly)

`crates/gateway/`:
- Modify: `crates/gateway/src/pipeline.rs` (or wherever provider dispatch lives) — add capability gate that scans canonical request for Image blocks and checks against target capabilities

`crates/frontends/`:
- Modify: `crates/frontends/src/anthropic_messages/decode.rs` — verify image source decode (URL + base64)
- Modify: `crates/frontends/src/openai_chat/decode.rs` — verify image_url decode (URL + data URI)
- Modify: `crates/frontends/src/{anthropic_messages,openai_chat}/encode_*.rs` — error rendering for `CapabilityMismatch`

`crates/protocol-tests/`:
- Create: `crates/protocol-tests/tests/vision_anthropic_to_openai_compat.rs`
- Create: `crates/protocol-tests/tests/vision_anthropic_to_anthropic_passthrough.rs`
- Create: `crates/protocol-tests/tests/vision_anthropic_to_copilot.rs`
- Create: `crates/protocol-tests/tests/vision_anthropic_to_gemini.rs`
- Create: `crates/protocol-tests/tests/vision_openai_chat_to_openai_compat.rs`
- Create: `crates/protocol-tests/tests/vision_openai_chat_to_copilot.rs`
- Create: `crates/protocol-tests/tests/vision_openai_chat_to_gemini.rs`
- Create: `crates/protocol-tests/tests/vision_capability_mismatch.rs`
- Create: `crates/protocol-tests/fixtures/vision/test_image.png` (~2KB tiny PNG)
- Create per-cell `request`/`upstream`/`expected` fixtures under `crates/protocol-tests/fixtures/vision/<provider>/<frontend>_image.*`

Root:
- Create: `scripts/regen-fixtures.sh`
- Create: `.github/workflows/nightly-live.yaml`
- Modify: `README.md` (capability matrix, vision support)
- Modify: `docs/architecture.md` (extension namespaces, hybrid Anthropic path, capability gate)
- Modify: `docs/contributing.md` (fixture regeneration, namespacing convention, frozen-core policy)
- Modify: `docs/configuration.md` (new upstream types)
- Create or modify: `docs/providers/anthropic.md`, `docs/providers/deepseek.md`, `docs/providers/gemini.md` (the per-provider pages, finalize after Plans 01–03)

---

## Tasks

### Task 1: Capability gate

- [ ] In `crates/gateway/src/pipeline.rs` (or equivalent), after route resolution and before provider dispatch, add a check:

```rust
fn check_capabilities(req: &CanonicalRequest, caps: &ProviderCapabilities) -> Result<(), ProviderError> {
    let has_image = req.messages.iter()
        .flat_map(|m| m.content.iter())
        .any(|b| matches!(b, ContentBlock::Image(_)));
    if has_image && !caps.vision {
        return Err(ProviderError::CapabilityMismatch(
            "target provider does not support vision".into(),
        ));
    }
    Ok(())
}
```

- [ ] Frontend response path: when `CapabilityMismatch` propagates back, render as a 400 with a documented error code (`capability_mismatch`) in the frontend's error envelope.
  - Anthropic: `{"type":"error","error":{"type":"invalid_request_error","message":"..."}}`
  - OpenAI Chat: `{"error":{"message":"...","type":"invalid_request_error","code":"capability_mismatch"}}`
- [ ] Unit tests for the gate function. Integration test in Task 8.

### Task 2: Anthropic frontend image decode/encode

- [ ] Verify `frontends/src/anthropic_messages/decode.rs` correctly decodes:
  - `{"type":"image","source":{"type":"base64","media_type":"image/png","data":"<base64>"}}` → `ContentBlock::Image(ImageBlock { source: BinarySource::Base64 { mime, data } })`
  - `{"type":"image","source":{"type":"url","url":"https://..."}}` → `ContentBlock::Image(ImageBlock { source: BinarySource::Url(...) })`
- [ ] Add unit test if missing.

### Task 3: OpenAI Chat frontend image decode/encode

- [ ] Verify `frontends/src/openai_chat/decode.rs` correctly decodes `{"type":"image_url","image_url":{"url":"data:image/png;base64,..."}}` and `{"type":"image_url","image_url":{"url":"https://..."}}`. Both forms.
- [ ] Add unit tests.

### Task 4: OpenAI-compat & Copilot provider image encoders

- [ ] Confirm `oai_chat_wire::canonical_to_chat::build` emits `image_url` parts when `ContentBlock::Image` is present:
  - `BinarySource::Base64 { mime, data }` → `{"type":"image_url","image_url":{"url":"data:{mime};base64,{data}"}}`
  - `BinarySource::Url(url)` → `{"type":"image_url","image_url":{"url":"{url}"}}`
  - `BinarySource::Bytes { mime, data }` → encode to base64, treat as Base64 case.
  - `BinarySource::ProviderFileId { .. }` → not supported in v0.2, log and skip with a warning.
- [ ] Set `capabilities.vision = true` for both providers (already true for OAI-compat per existing code; verify for Copilot).
- [ ] Tests covered by cells in Tasks 7–8.

### Task 5: Anthropic provider image encoder (canonical path)

- [ ] In `crates/providers/src/anthropic/request.rs`, emit Anthropic-style `image` blocks in the request body when `ContentBlock::Image` is present:
  - `BinarySource::Base64 { mime, data }` → `{"type":"image","source":{"type":"base64","media_type":mime,"data":data}}`
  - `BinarySource::Url(url)` → `{"type":"image","source":{"type":"url","url":url}}`
  - `BinarySource::Bytes` → encode to base64.
- [ ] Passthrough path already lossless — no change.
- [ ] Tests in Task 7.

### Task 6: Gemini provider image encoder

- [ ] In `crates/providers/src/gemini/request.rs`, emit `inline_data` parts (already scoped in Plan 03 Task 4 — confirm it's wired):
  - `BinarySource::Base64 { mime, data }` → `Part { inline_data: Some(InlineData { mime_type: mime, data }), .. }`
  - `BinarySource::Url(url)` → `Part { file_data: Some(FileData { mime_type, file_uri: url }), .. }` if Gemini supports `file_data` for HTTPS URLs in `streamGenerateContent`. **Verify against Gemini docs during implementation**; if not supported, fall back to fetching+inlining at the encoder boundary (warn at `info` about the latency impact) or reject with `UnsupportedFeature` error. Document the choice.
  - `BinarySource::Bytes` → base64 + InlineData.

### Task 7: Vision cell tests (Anthropic frontend)

- [ ] Capture or hand-craft fixtures for each cell:
  - `vision_anthropic_to_openai_compat`: Anthropic-style request with image → mocked OAI-compat upstream returning a vision-aware response.
  - `vision_anthropic_to_copilot`: same shape, Copilot upstream.
  - `vision_anthropic_to_anthropic_passthrough`: passthrough cell — assert byte-equal upstream pass-through.
  - `vision_anthropic_to_gemini`: Anthropic-style request with image → mocked Gemini upstream.
- [ ] Write each test using the per-pair fixture pattern from D10. Cross-protocol cells use inline `Vec<StreamEvent>` for canonical-event assertions where appropriate.
- [ ] All cells share `fixtures/vision/test_image.png` as the inbound image.

### Task 8: Vision cell tests (OpenAI Chat frontend)

- [ ] Mirror Task 7's three cells: `openai_compat`, `copilot`, `gemini`. (No `anthropic_passthrough` cell because the passthrough only applies when frontend == anthropic_messages.)
- [ ] OpenAI Chat → canonical-Anthropic (cross-protocol) is already covered by the Plan 01 cross test; Plan 04 doesn't need to re-test the cell with vision unless it surfaces a new bug.

### Task 9: Capability mismatch test

- [ ] `tests/vision_capability_mismatch.rs`: send an Anthropic-style request with an image, route at DeepSeek (which has `vision: false`). Assert:
  - The gateway never makes a network call (mockito's deepseek endpoint should record zero hits).
  - Response is HTTP 400.
  - Body matches Anthropic's error envelope shape with `type: invalid_request_error` and a message mentioning capability/vision.

### Task 10: Fixture regeneration script

- [ ] `scripts/regen-fixtures.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

mode="${1:-all}"

case "$mode" in
  anthropic|all)
    : "${ANTHROPIC_API_KEY:?ANTHROPIC_API_KEY required}"
    # curl invocations against api.anthropic.com saving SSE bodies into
    # crates/protocol-tests/fixtures/anthropic/*.upstream.sse, then re-run the
    # gateway against those captures to refresh expected/* fixtures.
    ;;
  deepseek|all)
    : "${DEEPSEEK_API_KEY:?DEEPSEEK_API_KEY required}"
    ;;
  gemini|all)
    : "${GEMINI_API_KEY:?GEMINI_API_KEY required}"
    ;;
  *) echo "unknown mode: $mode"; exit 1;;
esac
```

- [ ] Document usage in `docs/contributing.md`. The script is not part of CI; it's a developer tool.

### Task 11: Nightly live e2e workflow

- [ ] `.github/workflows/nightly-live.yaml`:

```yaml
name: nightly-live
on:
  schedule:
    - cron: "17 7 * * *"   # off-peak, off-the-hour
  workflow_dispatch:
jobs:
  live-e2e:
    runs-on: ubuntu-latest
    env:
      AGENT_SHIM_E2E: "1"
      ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
      DEEPSEEK_API_KEY: ${{ secrets.DEEPSEEK_API_KEY }}
      GEMINI_API_KEY: ${{ secrets.GEMINI_API_KEY }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo nextest run --features live --test '*_live'
        if: ${{ env.ANTHROPIC_API_KEY != '' }}
```

- [ ] Live tests live in `crates/protocol-tests/tests/{anthropic,deepseek,gemini}_live.rs`. Each: streaming text + tool call. Skipped at runtime if env var missing.

### Task 12: Documentation refresh

- [ ] `docs/providers/anthropic.md`: full setup, route examples, the hybrid path's user-visible behavior, `cache_control` round-trip rules.
- [ ] `docs/providers/deepseek.md`: setup, capability matrix, reasoning passthrough, cache usage reporting, the `cache_control` drop-with-debug-log behavior.
- [ ] `docs/providers/gemini.md`: setup, thinking budget mapping table, **the `extensions["gemini.safety_ratings"]` schema as a documented first-class behavior** (per ADR-0002).
- [ ] `docs/architecture.md`: explain extension namespaces, the hybrid Anthropic path, the capability gate, the oai_chat_wire/ shared lib, the JSON-array streaming parser.
- [ ] `docs/contributing.md`: fixture regeneration workflow, namespacing convention for `extensions` keys, frozen-core policy, how to add a new provider.
- [ ] `docs/configuration.md`: document all four upstream types with full field references.
- [ ] `README.md`: capability matrix table:

| Frontend → Provider | OAI-compat | Copilot | Anthropic | DeepSeek | Gemini |
|---|---|---|---|---|---|
| Anthropic Messages | text + tools + vision | text + tools + vision | text + tools + vision (passthrough lossless) | text + tools + reasoning | text + tools + vision + thinking |
| OpenAI Chat | text + tools + vision | text + tools + vision | text + tools (canonical path) | text + tools + reasoning | text + tools + vision + thinking |
| OpenAI Responses | text + tools | text + tools | n/a v0.2 | text + tools | n/a v0.2 |

### Task 13: Verification gate

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo nextest run --workspace`
- [ ] `cargo deny check`
- [ ] `agent-shim validate-config --config config/gateway.example.yaml` succeeds.
- [ ] Manual: spin up a test gateway, send a real image through Anthropic frontend → Gemini route, confirm a vision-aware response. (Soft gate — only run if API keys available.)

**Success criterion:** v0.2 ship-ready. Vision Tier-1 matrix tests all green; capability gate enforces correctly; documentation has every new upstream/route shape covered; nightly live workflow committed (even if unused initially).
