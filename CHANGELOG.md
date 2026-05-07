# Changelog

All notable changes to AgentShim are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] — 2026-05-07

Phase 2 release: provider breadth + vision Tier-1 + a token-count endpoint.
The canonical core is unchanged from v0.1 (frozen per
[ADR-0002](docs/adr/0002-frozen-core-v0.2.md)); all v0.2 work lands as new
providers, encoders, capability flags, and extension-map keys.

### Added

#### Native providers

- **Anthropic-as-backend** (`upstreams.<name>.type: anthropic`). Hybrid
  outbound path: byte-passthrough when the inbound frontend is also
  Anthropic Messages (round-trips `cache_control`, signature deltas on
  thinking blocks, and Anthropic-only metadata losslessly), canonical
  translation otherwise.
- **DeepSeek native** (`type: deepseek`). Adds:
  - `reasoning_content` interleaving for `deepseek-reasoner` (R1) — visible
    thinking blocks driven by the `ReasoningInterleaver` state machine.
  - `cache_hit_tokens` / `cache_miss_tokens` mapping into the canonical
    `Usage` shape.
  - Defense-in-depth `cache_control` strip on outbound bodies.
- **Gemini native** (`type: gemini`, AI Studio /
  generativelanguage.googleapis.com). Adds:
  - JSON-array streaming on `:streamGenerateContent` (NOT SSE) via a
    custom byte-level scanner that handles chunk boundaries at any byte
    position (mid-string, mid-escape, between objects). Exhaustively
    chunk-fuzzed.
  - `inlineData` (base64) and `fileData` (URL) image inputs.
  - `functionDeclarations` / `functionCall` / `functionResponse` for tools.
  - `thinkingConfig` (budget tokens) for reasoning-capable models.
  - `safetyRatings` / `citationMetadata` round-trip via the canonical
    extensions map (`gemini.safety_ratings`, `gemini.citation_metadata`).

#### Vision (Tier-1)

End-to-end image support across the (frontend × provider) matrix:

| Frontend → Provider | OAI-compat | Copilot | Anthropic | DeepSeek | Gemini |
|---|---|---|---|---|---|
| Anthropic Messages | ✅ | ✅ | ✅ (passthrough = lossless) | ❌ gate | ✅ |
| OpenAI Chat | ✅ | ✅ | ✅ (canonical) | ❌ gate | ✅ |
| OpenAI Responses | ✅ | ✅ | n/a | ❌ gate | n/a |

- **Capability gate** at the gateway boundary: when an inbound request
  contains an `Image` block but the routed provider's
  `capabilities.vision == false` (today: DeepSeek), the request is
  rejected with HTTP 400 and a frontend-shaped error envelope BEFORE any
  upstream call. See [`docs/architecture.md#capability-gate`](docs/architecture.md).
- `BinarySource::{Url, Base64, Bytes, ProviderFileId}` wired through every
  encoder. `Bytes` re-encodes to base64; `ProviderFileId` is dropped with
  a `tracing::warn!` when the target wire format doesn't model it.
- Vision smoke tests (`crates/providers/tests/vision_smoke.rs`,
  `crates/gateway/tests/vision_capability_mismatch.rs`) cover the active
  cells via mockito + a panicking stub provider.

#### `/v1/messages/count_tokens` endpoint

- New POST endpoint that mirrors Anthropic's `messages/count_tokens` API.
- Local token counting via `tiktoken-rs` — no upstream call required.
- Counts text, tool_use, tool_result, reasoning, redacted, image, and
  system instruction blocks; counts tool definitions and `tool_choice`.
- Per-message overhead applied per role.
- Smoke tests at `crates/gateway/tests/count_tokens_smoke.rs`.

#### Shared infrastructure

- `agent-shim-providers::oai_chat_wire` shared crate-internal lib:
  `canonical_to_chat::build`, `chat_sse_parser`, `chat_unary_parser`,
  `interleaved_reasoning`. Composed by the OpenAI-compatible, GitHub
  Copilot, and DeepSeek providers — no circular imports.
- `agent-shim-providers::http_client::build(read_timeout)` — shared TLS
  config, connect timeout, and read-gap timeout that every provider uses.
- `validate_oai_style_upstream` config helper, applied by all
  OpenAI-shaped upstreams.

#### Documentation

- Per-provider guides under `docs/providers/`: `anthropic.md`,
  `deepseek.md`, `gemini.md`.
- [ADR-0002](docs/adr/0002-frozen-core-v0.2.md) — the frozen-core policy
  that drove putting provider-specific data into namespaced
  `extensions` keys instead of new canonical variants.
- Refreshed `README.md`, `docs/architecture.md`, `docs/contributing.md`,
  `docs/configuration.md` for the v0.2 surface.
- `scripts/regen-fixtures.sh` for ongoing fixture maintenance.
- Opt-in nightly live-e2e GitHub Actions workflow
  (`.github/workflows/nightly-live.yaml`).

### Changed

- `EDITOR_VERSION` / `EDITOR_PLUGIN_VERSION` headers sent to GitHub
  Copilot bumped to `AgentShim/0.2.0`.
- `OpenAI Chat` decoder: `image_url` parts now decode into
  `ContentBlock::Image` with `BinarySource::{Url, Base64}` based on URL
  scheme (https / data: URI). Garbage URLs fall back to `Unsupported`.
- `Anthropic Messages` decoder: image source deserialized directly via
  `BinarySource`'s `serde` impl. `cache_control` preserved on the
  block's extensions map.
- GitHub Copilot provider declares `capabilities.vision = true` (it
  serves Claude and GPT-4o under the hood, both vision-capable).
- DeepSeek provider explicitly declares `capabilities.vision = false`
  so the gateway's capability gate rejects image requests with
  HTTP 400 before any upstream call.

### Deferred (Phase 3+)

- OpenAI `/v1/responses` frontend → Anthropic / Gemini backends
  (Phase 3). Today works for OAI-compat / Copilot / DeepSeek.
- Fallback chains, circuit breakers, retries with backoff (Phase 4).
- Rate limiting, per-agent API keys, request budget caps (Phase 4).
- Prometheus metrics, hot-reload config, OpenTelemetry (Phase 5).
- Audio / file content end-to-end.
- Multi-account Copilot.

## [0.1.1] — 2026-04-29

Fuzzy model discovery, polish.

## [0.1.0] — 2026-04-28

Initial MVP. Anthropic + OpenAI Chat frontends, OpenAI-compatible +
GitHub Copilot backends, tool calling, streaming, static config,
structured logging.
