# Phase 2 (v0.2) — Provider Breadth Design Spec

**Status:** Approved (grilling complete, ready for implementation planning)
**Date:** 2026-04-30
**Source:** [`2026-04-28-agent-shim-design.md`](./2026-04-28-agent-shim-design.md) §9 Phase 2

---

## 1. Scope

Phase 2 ships a **focused 3-provider expansion**, not the full v0.2 menu in the original roadmap. Discipline: ship them well rather than ship them all.

**In scope:**

- **DeepSeek native adapter** — composes the new `oai_chat_wire` shared lib; adds `reasoning_content` parsing, prompt-cache usage mapping, and the interleaved-reasoning state machine.
- **Gemini native adapter** (Generative-Language-API / "AI Studio") — separate code path, JSON-array streaming, native thinking-budget passthrough, safety-rating capture.
- **Anthropic-as-backend provider** — hybrid path: bytes-passthrough when the inbound frontend is `anthropic_messages`, full canonical translation otherwise.
- **Vision Tier-1** end-to-end — 7 active encoder cells + Anthropic-passthrough cell + capability gate; `BinarySource` already canonical.

**Deferred to v0.2.x point releases:**

- Native adapters for Qwen/DashScope, Kimi/Moonshot, Doubao/Volcengine, Grok/xAI (work via OpenAI-compat in the meantime).
- Vertex-AI Gemini and AWS Bedrock / Vertex Anthropic (cloud-hosted variants — different auth, different envelope).
- OpenAI Responses frontend × vision (Responses still settling; agents using it rarely send vision today).
- Response-side image emission from Gemini → Anthropic-frontend rendering.
- Anthropic OAuth ("Workbench" tokens) — API-key auth only in v0.2.

**Permanently out** (per parent spec §11): embeddings, moderation, audio I/O, admin UI, end-user identity, billing.

---

## 2. Locked Decisions

The eleven decisions that shape Phase 2. Each is referenced from plan files by number.

### D1. Provider scope: DeepSeek + Gemini + Anthropic-as-backend + Vision Tier-1

The other v0.2 candidates from the parent spec (Qwen, Kimi, Doubao, Grok) ride the existing OpenAI-compat shim until v0.2.x. Reasoning: DeepSeek showcases the "OpenAI-compat-with-quirks" pattern; Gemini is the only candidate that *cannot* ride the shim (different wire format); Anthropic-as-backend unlocks Claude API direct without Copilot indirection.

### D2. Anthropic provider uses a hybrid passthrough+canonical path

When `req.frontend.kind == FrontendKind::AnthropicMessages`, the provider proxies bytes through the existing `BackendProvider::proxy_raw` shape — round-trip is byte-for-byte lossless on Anthropic-only features (`cache_control`, `thinking`, server tools, beta headers). When the frontend is anything else, the provider takes the canonical path: encode `CanonicalRequest` → Anthropic Messages JSON, parse Anthropic SSE → `CanonicalStream`. One `BackendProvider` impl, two paths, decision in `complete()`.

ADR-0001 records the rationale. **The architectural invariant**: same prompt routed through both paths must produce semantically equivalent output, tested by golden fixtures.

### D3. Reasoning content is interleaved blocks, not collapsed

Provider parsers track an `in_reasoning: bool` state machine and emit `ContentBlockStart`/`ContentBlockStop` events around reasoning↔text transitions. DeepSeek's "all reasoning, then all content" is a degenerate case (one reasoning block followed by one text block). Gemini's `thoughts: bool` flag on parts uses the same machinery. Anthropic's interleaved `thinking → text → tool_use → thinking → text` pattern round-trips losslessly.

The state machine lives in `crates/providers/src/oai_chat_wire/interleaved_reasoning.rs` (introduced in Plan 02, consumed by Plan 03).

### D4. DeepSeek prompt-cache usage maps to canonical fields

DeepSeek's `prompt_cache_hit_tokens` → `Usage.cache_read_input_tokens`. DeepSeek's `prompt_cache_miss_tokens` → `Usage.input_tokens` (computed: `prompt_tokens − hit`). DeepSeek has no "cache creation" concept, so `cache_creation_input_tokens` stays `None`. Full DeepSeek breakdown preserved verbatim in `Usage.provider_raw`.

Inbound `cache_control` from an Anthropic-frontend client routed to DeepSeek is **dropped with a `debug!` log** — DeepSeek's "ignore unknown fields" is undocumented behavior; we don't bet on it.

### D5. Gemini provider targets Generative-Language-API (AI Studio); Vertex deferred to v0.2.x

`generativelanguage.googleapis.com/v1beta/...` with `?key=AIza...` API-key auth. Same UX as `DEEPSEEK_API_KEY`. JSON-array streaming (not SSE) — provider implements an incremental JSON-array parser (~150 lines + fuzz test). Vertex defers to v0.2.x because its 95% wire-format overlap means encoder + parser are reusable; only `auth.rs` and `endpoint.rs` swap.

### D6. Vision Tier-1 ships 7 active encoder cells + 1 passthrough + capability gate

| | OAI-compat | Copilot | Anthropic | Gemini | DeepSeek |
|---|---|---|---|---|---|
| Anthropic frontend | ✅ | ✅ | ✅ passthrough | ✅ | ❌ capability gate |
| OpenAI Chat frontend | ✅ | ✅ | (covered by canonical Anthropic) | ✅ | ❌ capability gate |
| OpenAI Responses frontend | deferred (v0.2.x) | deferred | deferred | deferred | ❌ capability gate |

Capability gate raises `ProviderError::CapabilityMismatch` *before* the upstream call when the frontend sent images and the target provider's `vision: false`. Frontend renders a 400 in its dialect.

Response-side image emission from Gemini in v0.2: parsed (we don't crash on `inline_data` parts in responses) but not encoded through Anthropic-frontend image-output blocks. Dropped with debug log.

### D7. Module reuse via `oai_chat_wire/` extraction; sibling provider modules

```
crates/providers/src/
  oai_chat_wire/                  # NEW: shared crate-internal lib
    mod.rs
    canonical_to_chat.rs          # CanonicalRequest → OpenAI Chat JSON body
    chat_sse_parser.rs            # OpenAI SSE chunk → CanonicalStream events
    chat_unary_parser.rs          # OpenAI JSON response → CanonicalStream
    interleaved_reasoning.rs      # in_reasoning state machine (D3)
  openai_compatible/              # composes oai_chat_wire
  github_copilot/                 # composes oai_chat_wire (after migration)
  deepseek/                       # NEW: composes oai_chat_wire
  gemini/                         # NEW: separate (different wire format)
  anthropic/                      # NEW: passthrough + canonical paths
```

Mapping tables shared between Anthropic frontend and provider live in `agent-shim-core::mapping::anthropic_wire` (stop-reason translation, role mapping, tool_use ↔ tool_call). Wire-shape DTOs stay duplicated (~30 lines each, cheap).

The `oai_chat_wire/` extraction lands as a non-feature-bearing prep PR inside Plan 01, before any new provider code.

### D8. Anthropic provider auth: API-key only

`x-api-key: <api_key>` + `anthropic-version: <configurable, default 2023-06-01>`. New config variant:

```yaml
upstreams:
  anthropic:
    type: anthropic
    api_key: sk-ant-...
    base_url: https://api.anthropic.com    # default
    anthropic_version: "2023-06-01"        # default
    request_timeout_secs: 120
```

OAuth (Console "Workbench" tokens), Bedrock SigV4, GCP-Vertex-Anthropic envelope all deferred to v0.3+.

### D9. Phase 2 = 4 plan files (provider-major sequencing)

```
2026-04-30-01-oai-chat-wire-and-anthropic.md   (W1 + W2)
2026-04-30-02-deepseek.md                       (W3)
2026-04-30-03-gemini.md                         (W4)
2026-04-30-04-vision-and-docs.md                (W5 + W6)
```

Each plan is independently shippable: a user upgrading "after plan 02" gets DeepSeek native; "after plan 03" gets Gemini. Plan 02 ships `interleaved_reasoning.rs` so Plan 03 can import it.

### D10. Testing strategy: per-pair fixtures + 3 discipline rules + opt-in nightly live e2e

**Fixture organization:**

```
crates/protocol-tests/fixtures/<provider>/<scenario>.{request,upstream,expected}.{json,sse,jsonl}
```

The triple-suffix marks role: `request` = frontend input, `upstream` = mocked provider wire response, `expected` = frontend-encoded output bytes (or canonical-event JSON for cross-protocol).

**Three discipline rules:**

1. Fixture filenames follow the `<scenario>.<role>.<ext>` convention so regeneration is scriptable.
2. `scripts/regen-fixtures.sh` runs each provider's "capture mode" against live APIs (gated by `--features live` + env vars) so canonical-model field additions are mechanical.
3. **Cross-protocol tests assert canonical events at the boundary**, not byte equality. Each `cross_<frontend>_to_<provider>.rs` uses `collect_canonical_events(stream) → Vec<StreamEvent>` and asserts against an inline `Vec<StreamEvent>`. Saves a fixture file per cross-cell.

**Live-API e2e:** behind `--features live` + `AGENT_SHIM_E2E=1` + provider-specific env vars. Nightly workflow `.github/workflows/nightly-live.yaml` runs streaming text + tool call per provider. Failures surface as issues, not release blockers.

### D11. Frozen `agent-shim-core` for v0.2

**No new fields, no new variants, no new types in `agent-shim-core` during Phase 2.** Provider-specific data lives in `extensions: HashMap<String, serde_json::Value>` with documented namespace prefixes:

| Concern | Lives where |
|---|---|
| DeepSeek `reasoning_content` deltas | `ContentBlock::Reasoning` + `StreamEvent::ReasoningDelta` (existing) |
| DeepSeek cache hit/miss tokens | `Usage.cache_read_input_tokens` + `Usage.provider_raw` (existing) |
| Gemini `thoughts: bool` parts | `ContentBlock::Reasoning` interleaving (D3) |
| Gemini safety ratings (per-category probabilities) | `extensions["gemini.safety_ratings"]` on `CanonicalResponse` |
| Gemini `finishReason: "SAFETY"` | `StopReason::ContentFilter` (existing) |
| Anthropic `cache_creation` markers on response blocks | `extensions["anthropic.cache_creation"]` on relevant `ContentBlock` |

ADR-0002 records the rationale and the v0.3 promotion plan. **Documented exception**: Gemini safety ratings are first-class behavior (`docs/providers/gemini.md` documents stable contract), even though storage is untyped. v0.3 promotes to a typed field.

---

## 3. Module Layout

```
crates/
  core/                                 # FROZEN in v0.2 (D11)
    src/
      mapping/
        anthropic_wire.rs               # NEW: shared between frontend + provider
  providers/
    src/
      oai_chat_wire/                    # NEW (D7)
        mod.rs
        canonical_to_chat.rs
        chat_sse_parser.rs
        chat_unary_parser.rs
        interleaved_reasoning.rs        # D3 state machine
      openai_compatible/                # composes oai_chat_wire
      github_copilot/                   # composes oai_chat_wire (post-migration)
      deepseek/                         # NEW
        mod.rs
        request.rs                      # delegates to oai_chat_wire::canonical_to_chat
        response.rs                     # wraps chat_sse_parser with reasoning_content extension
        usage.rs                        # cache field mapping (D4)
      gemini/                           # NEW
        mod.rs
        auth.rs                         # AI-Studio API-key URL builder
        endpoint.rs
        request.rs                      # CanonicalRequest → Gemini Content/parts
        response.rs                     # Gemini JSON-array → CanonicalStream
        streaming_json.rs               # incremental JSON-array parser
      anthropic/                        # NEW
        mod.rs                          # passthrough vs canonical decision
        passthrough.rs                  # uses BackendProvider::proxy_raw shape
        request.rs                      # CanonicalRequest → Anthropic Messages JSON
        response.rs                     # Anthropic SSE → CanonicalStream
  protocol-tests/
    tests/
      anthropic_passthrough.rs          # Plan 01
      anthropic_canonical.rs            # Plan 01
      cross_openai_chat_to_anthropic.rs # Plan 01
      deepseek_text_stream.rs           # Plan 02
      deepseek_reasoning.rs             # Plan 02
      deepseek_cache_usage.rs           # Plan 02
      cross_anthropic_to_deepseek_reasoning.rs   # Plan 02
      gemini_text_stream.rs             # Plan 03
      gemini_tool_call.rs               # Plan 03
      gemini_thinking.rs                # Plan 03
      json_array_streaming_fuzz.rs      # Plan 03
      cross_anthropic_to_gemini.rs      # Plan 03
      vision_*.rs                       # Plan 04 (8 cells)
    fixtures/
      anthropic/
      deepseek/
      gemini/
      vision/
        test_image.png                  # ~2KB shared PNG
config/
  gateway.example.yaml                  # add anthropic, deepseek, gemini upstream examples
docs/
  providers/
    anthropic.md                        # NEW
    deepseek.md                         # NEW
    gemini.md                           # NEW
  superpowers/
    specs/
      2026-04-30-phase-2-provider-breadth-design.md   # this doc
    plans/
      2026-04-30-01-oai-chat-wire-and-anthropic.md
      2026-04-30-02-deepseek.md
      2026-04-30-03-gemini.md
      2026-04-30-04-vision-and-docs.md
  adr/
    0001-anthropic-hybrid-path.md
    0002-frozen-core-v0.2.md
.github/
  workflows/
    nightly-live.yaml                   # opt-in live e2e
scripts/
  regen-fixtures.sh                     # opt-in fixture capture
```

---

## 4. Definition of Done for Phase 2

- All four plans land green: `cargo nextest run --workspace` passes, `cargo clippy --workspace -- -D warnings` clean, `cargo deny check` clean.
- README updated with the v0.2 capability matrix.
- `docs/providers/{anthropic,deepseek,gemini}.md` exist with: setup walkthrough, capability flags, route examples, known limitations.
- `gateway.example.yaml` includes commented examples for all three new upstreams.
- ADR-0001 (Anthropic hybrid path) and ADR-0002 (frozen core) committed.
- Nightly-live workflow exists with secrets wired (or marked TODO if secrets unavailable).
- Vision capability gate verified: Anthropic-frontend image → DeepSeek route returns a 400 with a documented error code.

---

## 5. Out of Scope (Explicit)

To prevent scope creep during plan execution. If any of these come up mid-plan, they trigger a separate plan, not a same-plan addition.

- Vertex-AI Gemini, Bedrock Anthropic, Vertex-Anthropic envelopes
- Anthropic OAuth (Workbench tokens), refresh flow
- Qwen/DashScope, Kimi/Moonshot, Doubao/Volcengine, Grok/xAI native adapters
- OpenAI Responses × vision cells
- Response-side Gemini image emission rendered through Anthropic frontend
- ProviderFileId variant of `BinarySource` (defer until a backend widely uses file-upload-then-reference)
- Promoting any `extensions["<provider>.*"]` key to a typed canonical field
- Multi-account anything
- Parallel tool-call hints, logprobs (extension-only per parent spec)
