# Changelog

All notable changes to AgentShim are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.1] — 2026-05-09

Patch release: fixes a streaming termination bug on the OpenAI Chat
frontend that caused clients to hang indefinitely after the model had
finished responding.

### Fixed

- **OpenAI Chat `/v1/chat/completions` streaming response never
  terminated.** When `server.keepalive_secs > 0` (the default, 15s),
  the SSE encoder merged the canonical event stream with an infinite
  `IntervalStream` of keepalive pings via `futures::stream::select`,
  which only ends when *both* sides end. The ping side never ended,
  so even after `data: [DONE]\n\n` was emitted the HTTP body stayed
  open and clients (Codex, Cursor, Claude Code, etc.) sat on
  "waiting for reply" indefinitely. The fix mirrors the pattern
  already in use by the Anthropic Messages and OpenAI Responses
  encoders: an `AtomicBool` `done` flag flipped on `ResponseStop`
  (and on stream-level errors) gates the ping stream via
  `take_while` and trips a `terminate_on_sentinel` `scan` over the
  merged output, closing the body cleanly after `[DONE]`. Two
  regression tests with a 2-second `tokio::time::timeout` guard the
  termination contract for both `keepalive=Some(_)` and
  `keepalive=None` paths
  (`crates/frontends/src/openai_chat/encode_stream.rs`).

## [0.4.0] — 2026-05-08

Phase 4 release: the gateway becomes **resilient** as well as
protocol-translating. Four subsystems — fallback chains, per-route
retries, per-(upstream, model) circuit breakers, and four-dimensional
token-bucket rate limiting — compose under a new `ResilientCaller`
orchestrator (see [ADR-0004](docs/adr/0004-resilient-caller.md)). All
four subsystems are independently configurable and default to behavior
that preserves v0.3 wire output for healthy upstreams.

### Added

#### Per-route fallback chains (Plan 02)

- Routes can list primary + backup upstreams in an ordered
  `upstreams: [...]` array. Fallback fires on retry exhaustion against
  the current upstream when the error is fallback-eligible (network /
  upstream 5xx / upstream 429).
- Existing v0.3 singular `upstream` / `upstream_model` shape continues
  to work — internally it deserializes to a 1-element vec. Mixed
  shapes (both singular and array on the same route) are rejected at
  startup.
- Three new `HandlerError` variants: `NoUpstreamSucceeded` (HTTP 503),
  `AllBreakersOpen` (HTTP 503), and `RateLimited` (HTTP 429 +
  `Retry-After`) with dialect-correct envelopes for OpenAI Chat,
  OpenAI Responses, and Anthropic Messages.

#### Per-route retries (Plan 01)

- Exponential backoff + jitter + total-time budget within a single
  upstream. Defaults: 2 attempts, 100ms initial, 2.0× multiplier,
  ±25% jitter, 5000ms total budget.
- Default fallback eligibility: `network`, `upstream_5xx`,
  `upstream_429`. Operators override per route via
  `retry.retry_on: [...]`.

#### Per-(upstream, model) circuit breakers (Plan 03)

- Sliding-window failure-rate breaker. Trips when the failure rate
  over `window_secs` (default 60s) exceeds `failure_threshold_pct`
  (default 50%) across at least `min_requests` (default 20). Open
  state holds for `open_cooldown_secs` (default 30s) before
  transitioning to half-open.
- Half-open state allows exactly one probe via
  `AtomicBool::compare_exchange`. Probe success → Closed; probe
  failure → Open with a fresh cooldown.
- Per-(provider_name, model) keying — a misconfigured model on an
  otherwise healthy provider trips its own breaker without affecting
  siblings.

#### Four-dimensional token-bucket rate limiting (Plan 04)

- Independent buckets per: API key, route, upstream, source IP. A
  request must satisfy all applicable buckets; the first to reject
  names the dimension in the error.
- Built on the [`governor`](https://crates.io/crates/governor) crate
  for atomic, lock-free per-bucket math.
- HTTP 429 responses include `Retry-After` derived from `governor`'s
  `wait_time_from(now)`.

#### API-key auth (Plan 04)

- Keys come in via `Authorization: Bearer <key>` or
  `x-api-key: <key>`. The gateway hashes the key with SHA-256 and
  looks up the hash in `auth.keys.<sha256:hex>`. Plaintext is never
  stored or logged.
- `auth.enabled=false` (default) skips header inspection entirely
  (zero overhead).
- `auth.enabled=true, required=false`: unknown keys → tagged
  `Anonymous` (uses anonymous bucket).
- `auth.required=true`: unknown keys → HTTP 401 before any upstream
  contact.

#### Structured resilience tracing (Plan 05)

- Every retry attempt, fallback transition, breaker state change,
  rate-limit rejection, and request completion emits a structured
  `tracing` event under `target = "agent_shim::resilience"` with a
  fixed field set (see `crates/router/src/resilient_caller.rs`
  module-level doc). Identity in logs is the SHA-256 hash form
  (`sha256:<hex>`) or `anonymous` — plaintext keys never appear in
  logs.
- Per-request summary log at request end with the full chain walk.

### Changed

- **Default `retry.max_attempts: 2`** — small behavior change on
  upgrade (one extra round-trip on transient errors). Operators
  wanting strict v0.3 behavior (no retries) set
  `retry: {max_attempts: 1}` per route.
- `BackendTarget` resolution now returns `Vec<BackendTarget>` instead
  of `Option<BackendTarget>`. The first element is the primary; the
  rest are fallback candidates. v0.3 single-upstream routes produce a
  1-element vec.
- `ResilientCaller::complete` replaces the direct
  `provider.complete()` call in `pipeline.rs`. All four subsystems
  compose at this single point; see
  [ADR-0004](docs/adr/0004-resilient-caller.md).

### Deprecated

None. Both v0.3 and v0.4 config shapes are supported indefinitely.

### Fixed

None — this release is additive against the v0.3 baseline.

### Documentation

- New [`docs/resilience.md`](docs/resilience.md) operator-facing guide
  with quick-start config, SHA-256 key generation recipe, retry
  tuning, layering walkthrough, log-line reference, and the
  multi-instance caveat.
- New [`docs/adr/0004-resilient-caller.md`](docs/adr/0004-resilient-caller.md)
  records the orchestrator-pattern choice over middleware-per-subsystem
  and inline-in-pipeline alternatives.
- `docs/architecture.md` capability matrix gains a Resilience row;
  performance overhead targets paragraph from the design spec.
- `docs/contributing.md` gains a "How to add a resilience subsystem"
  subsection.
- All five provider docs gain a "Resilience behavior" subsection
  documenting fallback eligibility per provider.

### Known limitations

- **Single-instance state.** Breaker and rate-limit state lives in
  process memory. Multi-instance deployments behind a load balancer
  lose strict enforcement until the breaker actually trips at the
  upstream. Distributed state (Redis backend) is a Phase 5 candidate.
- **Pre-stream fallback only.** Once `provider.complete()` returns
  `Ok(stream)` and bytes flow to the client, fallback is no longer
  possible. Mid-stream failures surface as stream-level errors. This
  matches v0.3 behavior; v0.4 does not introduce buffering.

### Frozen-core invariant resumes

ADR-0003 (Phase 3) was a bounded one-time exception. All five
Phase 4 plan files declared `core changes: NONE`, and
`git diff v0.3.0..master -- crates/core/` is empty for the v0.4.0
release tag.

## [0.3.0] — 2026-05-08

Phase 3 release: the OpenAI Responses frontend now routes to every
backend, vision Tier-1 covers the Responses row of the capability
matrix, and the first new core field since v0.1 lands as a typed
`Usage.safety_ratings` (per [ADR-0003](docs/adr/0003-promote-safety-ratings.md)).

### Added

#### OpenAI Responses frontend × all backends

- **Responses → Anthropic** via the canonical translation path
  (Plan 02). Hybrid passthrough still runs only when both inbound and
  outbound are Anthropic Messages; Responses-shaped requests go
  through `anthropic::request::build` / `anthropic::response::parse`.
- **Responses → Gemini** via the existing canonical Generate Content
  client (Plan 03). Gemini's `thoughts: true` reasoning parts surface
  as Responses `response.reasoning.delta` / `.done` events, and
  `safetyRatings` reach the `response.completed` payload.
- **Responses event-model audit** (Plan 01): golden SSE captures pin
  `response.output_item.{added,done}`,
  `response.content_part.{added,done}`,
  `response.output_text.{delta,done}`,
  `response.function_call_arguments.{delta,done}`,
  `response.reasoning.{delta,done}`, `response.completed`,
  `response.error` against real OpenAI traces.
- **Responses tool round-trip**: `function_call` /
  `function_call_output` items in `input` decode to canonical
  `ToolCallBlock` / `ToolResultBlock`, and outbound canonical tool
  calls re-emit as streaming `function_call` output items.
- **Responses reasoning round-trip**: inbound `reasoning` items in
  `input` decode to `ContentBlock::Reasoning`; outbound
  `StreamEvent::ReasoningDelta` surfaces as `response.reasoning.delta`
  + `response.reasoning.done` (previously dropped silently).

#### Vision (Tier-1) for Responses

End-to-end image support for the Responses row of the capability
matrix:

| Frontend → Provider | OAI-compat | Copilot | Anthropic | DeepSeek | Gemini |
|---|---|---|---|---|---|
| OpenAI Responses | ✅ | ✅ | ✅ (canonical) | ❌ gate | ✅ |

- The DeepSeek negative cell goes through the same capability gate as
  the existing Anthropic Messages and OpenAI Chat rows.
- Vision smoke tests cover all four active cells via mockito plus the
  capability-mismatch test using a panicking text-only stub provider.

#### `Usage.safety_ratings` — first new canonical field since v0.1

- New typed canonical field `Usage::safety_ratings: Option<Vec<SafetyRating>>`
  in `crates/core/src/safety.rs`, with open-enum
  `SafetyCategory::Other(String)` and `SafetyLevel::Other(String)`
  variants so unknown wire values round-trip losslessly.
- ADR-0003 records the promotion:
  [`docs/adr/0003-promote-safety-ratings.md`](docs/adr/0003-promote-safety-ratings.md).
  This is the v0.3 frozen-core exception that ADR-0002 reserved.
- Cross-protocol smoke tests (Responses → Anthropic and
  Responses → Gemini): text, tools, reasoning, safety, vision —
  15+ new tests under `crates/protocol-tests/tests/`.

#### Documentation

- ADR-0003 published.
- Per-provider docs gain a "Responses frontend" section.
- README capability matrix bumped to v0.3 with the Responses row
  fully populated.

### Changed

- Gemini provider now **double-writes** safety ratings: both the new
  typed `Usage.safety_ratings` field and the legacy
  `extensions["gemini.safety_ratings"]` key. The legacy write is
  removed in v0.3.1, marked at the call sites with
  `// REMOVE in v0.3.1` per ADR-0003.
- `EDITOR_VERSION` / `EDITOR_PLUGIN_VERSION` headers sent to GitHub
  Copilot bumped to `AgentShim/0.3.0`.

### Fixed

- **Anthropic streaming parser** now emits `ToolCallStop` for
  `tool_use` blocks (latent v0.2 bug surfaced during Plan 02 T2 —
  the parser previously emitted `ContentBlockStop` only, leaving
  the canonical event sequence missing the matching tool-call stop
  event for Responses-bound consumers).
- **OpenAI Responses decoder** now maps `input_image` content parts
  to `ContentBlock::Image` (latent bug surfaced during Plan 04 T4 —
  the decoder previously routed image parts to
  `ContentBlock::Unsupported`, silently dropping vision payloads
  before they reached any provider).
- **OpenAI Responses outbound encoder** now threads images through
  to the upstream wire shape (latent bug surfaced during Plan 04 T4 —
  the encoder previously called `extract_text` only, dropping image
  blocks on the way out).

### Deprecated

- `extensions["gemini.safety_ratings"]` — read from
  `Usage.safety_ratings` instead. The extension write is retained for
  the v0.3.0 transitional minor and removed in v0.3.1 per ADR-0003.

### Deferred (Phase 4+)

- Fallback chains, circuit breakers, per-route retries with backoff
  (Phase 4).
- Per-key rate limiting, per-agent API keys, request budget caps
  (Phase 4).
- Prometheus metrics, hot-reload config, OpenTelemetry (Phase 5).
- Audio / file content end-to-end (Phase 6+).
- Multi-account Copilot (Phase 6+).
- OAuth Anthropic / "Workbench" tokens (Phase 6+).

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
