# AgentShim — Design Spec

**Status:** Approved (brainstorming complete, ready for implementation planning)
**Date:** 2026-04-28
**Source requirements:** [`docs/Requirements.md`](../../Requirements.md)

---

## 1. Product Framing & MVP Scope

AgentShim is an open-source, single-static-binary gateway that lets any AI coding agent talk to any LLM backend.

**Differentiation thesis:** performance and footprint. AgentShim is a Rust drop-in replacement for LiteLLM-style Python proxies. Protocol fidelity is the correctness backbone that makes the perf claim meaningful — a fast proxy that breaks Claude Code's tool calls is worthless.

**License:** MIT.

**v0.1 (MVP) ships exactly this:**

- **Frontends:** Anthropic `/v1/messages` + OpenAI `/v1/chat/completions`. Both with full SSE streaming and tool calling.
- **Backends:** Generic OpenAI-compatible (covers DeepSeek, Kimi, Qwen-via-compat-mode, vLLM, Ollama, etc. with config) **and** GitHub Copilot (OAuth device flow + token exchange + refresh).
- **Cross-protocol translation:** Anthropic-in → OpenAI-out and Anthropic-in → Copilot-out both work, including tool-call streaming deltas.
- **Config:** static YAML, route table mapping frontend models → backend targets. No hot-reload.
- **Observability:** structured `tracing` logs with request IDs. No Prometheus.

**Success criteria:** Claude Code talks to GitHub Copilot through AgentShim. Codex/Cursor talks to DeepSeek through AgentShim. Both with streaming and tool calls. Gateway overhead measurable and under 5ms p99 on the local-network hop.

---

## 2. Tech Stack

**Core:**
- Runtime: `tokio` (multi-threaded scheduler)
- HTTP server: `axum` on `hyper`
- HTTP client: `reqwest` with `rustls`
- Middleware: `tower`
- Serde: `serde` + `serde_json` with `RawValue` for arg-delta passthrough

**Cross-cutting:**
- Config: `figment` (YAML + env overlays) with `serde` schemas
- Logging/tracing: `tracing` + `tracing-subscriber` (JSON for prod, pretty for dev)
- CLI: `clap` derive
- Errors: `thiserror` for libraries, `anyhow` only at the binary boundary
- Async traits: native `async fn in trait` where possible; `async-trait` only when dyn-dispatch is needed
- TLS: `rustls` everywhere (no OpenSSL — true single-binary deploys)

**Build/dev:**
- Cargo workspace, `cargo nextest`, `cargo deny`, `cargo flamegraph`
- MSRV pinned to current stable - 2

**Deferred:**
- `metrics` / `prometheus-client` (Phase 5)
- `notify` for hot-reload (Phase 5)
- `governor` for rate limiting (Phase 4)

**Notable trade-off:** committing to `axum` over hand-rolled `hyper`. The 100–200ns overhead is well within the 5ms budget. If a benchmark later shows it costing us, we can drop to `hyper` for the streaming hot path without rewriting routing.

---

## 3. High-Level Architecture

**Request flow:**

```
                     ┌──────────────────────────────────────┐
                     │          HTTP server (axum)          │
                     │   Routes: /v1/messages               │
                     │           /v1/chat/completions       │
                     └──────────────────────────────────────┘
                                       │
                       ┌───────────────┴───────────────┐
                       ▼                               ▼
              ┌─────────────────┐             ┌─────────────────┐
              │ Frontend Adapter│             │ Frontend Adapter│
              │   (Anthropic)   │             │    (OpenAI)     │
              └────────┬────────┘             └────────┬────────┘
                       │                               │
                       └────────────┬──────────────────┘
                                    ▼
                       ┌─────────────────────────┐
                       │   CanonicalRequest      │
                       └────────────┬────────────┘
                                    ▼
                       ┌─────────────────────────┐
                       │   Router / Policy       │
                       │  • model alias resolve  │
                       │  • pick BackendTarget   │
                       └────────────┬────────────┘
                                    ▼
              ┌─────────────────┐       ┌─────────────────┐
              │ Backend Provider│       │ Backend Provider│
              │ (OpenAI-compat) │       │   (Copilot)     │
              └────────┬────────┘       └────────┬────────┘
                       │                         │
                       └────────────┬────────────┘
                                    ▼
                       ┌─────────────────────────┐
                       │  CanonicalStream        │
                       └────────────┬────────────┘
                                    ▼
                       ┌─────────────────────────┐
                       │ Frontend SSE Encoder    │
                       └────────────┬────────────┘
                                    ▼
                              SSE bytes to agent
```

**Crate boundaries (workspace):**

| Crate | Depends on | Responsibility |
|---|---|---|
| `core` | (leaf) | `CanonicalRequest`, `Message`, `ContentBlock`, `StreamEvent`, `BackendTarget`, error types, IDs. Zero I/O. |
| `frontends` | `core` | Decode incoming HTTP → `CanonicalRequest`. Encode `CanonicalStream` → wire SSE / JSON. Owns SSE event-shape fidelity. |
| `providers` | `core` | Encode `CanonicalRequest` → upstream HTTP. Decode upstream response/SSE → `CanonicalStream`. Owns provider quirks. |
| `router` | `core` | Resolve frontend model → `BackendTarget`. v0.1 = static map; designed so fallback/circuit-breaker slots into the same trait later. |
| `config` | `core` | YAML schema, env overlay, validation. |
| `observability` | `core` | `tracing` setup, request-ID middleware, redaction helpers. |
| `gateway` | all above | The binary. axum app assembly, lifecycle, signal handling. |
| `protocol-tests` | all | Integration crate. Golden SSE captures, mock upstreams, end-to-end fidelity tests. |

**Two core traits:**

```rust
// frontends crate
pub trait FrontendProtocol: Send + Sync {
    fn decode_request(&self, http: HttpRequest) -> Result<CanonicalRequest, FrontendError>;
    fn encode_stream(&self, stream: CanonicalStream) -> HttpResponse;
    fn encode_unary(&self, response: CanonicalResponse) -> HttpResponse;
}

// providers crate
#[async_trait]
pub trait BackendProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> ProviderCapabilities;
    async fn complete(
        &self,
        req: CanonicalRequest,
        target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError>;
}
```

`complete` always returns a stream, even for non-streaming calls. The frontend `encode_unary` path collapses the stream into a single response. One code path for both modes.

**Boundary rule:** the router never touches provider JSON; provider adapters never touch frontend JSON. Translation happens only at the two edges.

---

## 4. Canonical Data Model

The contract everything else implements. Neutral block-based shape, inspired by both Anthropic and OpenAI.

**Top-level request:**

```rust
pub struct CanonicalRequest {
    pub id: RequestId,
    pub frontend: FrontendInfo,
    pub model: FrontendModel,
    pub system: Vec<SystemInstruction>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub tool_choice: ToolChoice,
    pub generation: GenerationOptions,
    pub response_format: Option<ResponseFormat>,
    pub stream: bool,
    pub metadata: RequestMetadata,
    pub extensions: ExtensionMap,
}

pub struct SystemInstruction {
    pub source: SystemSource,  // Anthropic-system | OpenAI-system | OpenAI-developer
    pub content: Vec<ContentBlock>,
}
```

**Messages and content blocks:**

```rust
pub struct Message {
    pub role: MessageRole,    // User | Assistant | Tool
    pub content: Vec<ContentBlock>,
    pub name: Option<String>,
    pub extensions: ExtensionMap,
}

pub enum ContentBlock {
    Text(TextBlock),
    Image(ImageBlock),
    Audio(AudioBlock),         // defined now, unused in v0.1
    File(FileBlock),           // defined now, unused in v0.1
    ToolCall(ToolCallBlock),
    ToolResult(ToolResultBlock),
    Reasoning(ReasoningBlock),
    RedactedReasoning(RedactedReasoningBlock),
    Unsupported(UnsupportedBlock),
}

pub struct ToolCallBlock {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: ToolCallArguments,
    pub extensions: ExtensionMap,
}

pub enum ToolCallArguments {
    Complete(serde_json::Value),
    Streaming(Box<RawValue>),
}

pub struct ToolResultBlock {
    pub tool_call_id: ToolCallId,
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    pub extensions: ExtensionMap,
}
```

`ToolCallArguments` is an enum because providers stream args as raw JSON fragments that aren't valid JSON until the last delta. Holding `RawValue` mid-stream avoids re-parsing and preserves precision (`6.0` vs `6`). Only parse once at completion.

**Media:**

```rust
pub enum BinarySource {
    Url(String),
    Base64 { mime: String, data: String },
    Bytes { mime: String, data: Bytes },
    ProviderFileId { provider: String, id: String },
}
```

**Stream type:**

```rust
pub type CanonicalStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, StreamError>> + Send>>;

pub enum StreamEvent {
    ResponseStart { id: ResponseId, model: String, created_at: SystemTime },
    MessageStart { role: MessageRole },
    ContentBlockStart { index: u32, kind: ContentBlockKind },
    TextDelta { index: u32, text: String },
    ReasoningDelta { index: u32, text: String },
    ToolCallStart { index: u32, id: ToolCallId, name: String },
    ToolCallArgumentsDelta { index: u32, json_fragment: String },
    ToolCallStop { index: u32 },
    ContentBlockStop { index: u32 },
    UsageDelta(Usage),
    MessageStop { stop_reason: StopReason, stop_sequence: Option<String> },
    ResponseStop { usage: Option<Usage> },
    Error(StreamError),
    RawProviderEvent(RawProviderEvent),
}

pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    ContentFilter,
    Refusal,
    Error,
    Unknown(String),
}

pub struct Usage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
    pub reasoning_tokens: Option<u32>,
    pub estimated: bool,
    pub provider_raw: Option<serde_json::Value>,
}
```

**Extension-map pattern.** Every major struct carries `extensions: HashMap<String, serde_json::Value>`. Decoders dump unknown fields here; encoders consult it for protocol-specific hints. This prevents the canonical model from blocking new provider features.

**Not modeled in v0.1:** logprobs (extension-only), parallel-tool hints (extension-only), embeddings/moderation endpoints (out of scope).

---

## 5. Streaming Model & SSE Parity Strategy

**Pipeline:**

```
upstream HTTP body (bytes)
  → provider SSE/JSON parser     (in `providers`)
  → Stream<CanonicalEvent>
  → frontend SSE encoder         (in `frontends`)
  → axum::response::Sse / raw bytes
```

Every stage is a `Stream` adapter. No buffering of the full response. Backpressure flows naturally from the client socket through `hyper` → encoder → canonical stream → upstream client.

**Event-shape parity examples:**

*Anthropic `/v1/messages` SSE.* Maps from canonical:
- `ResponseStart` + `MessageStart` → one `message_start` event
- `ContentBlockStart{Text}` → `content_block_start` with `type: "text"`
- `TextDelta` → `content_block_delta` with `delta: { type: "text_delta", text: "..." }`
- `ContentBlockStart{ToolUse}` + `ToolCallStart` → single `content_block_start` with `type: "tool_use"`
- `ToolCallArgumentsDelta` → `content_block_delta` with `delta: { type: "input_json_delta", partial_json: "..." }`
- `MessageStop` → `message_delta` (with stop_reason) followed by `message_stop`

*OpenAI `/v1/chat/completions` SSE.* Maps from canonical:
- `TextDelta` → chunk with `delta.content`
- `ToolCallStart` → chunk with `delta.tool_calls[{index, id, type, function:{name, arguments:""}}]`
- `ToolCallArgumentsDelta` → chunk with `delta.tool_calls[{index, function:{arguments: "<fragment>"}}]`
- `MessageStop` → final chunk with `finish_reason`, then `data: [DONE]`

Translation tables live next to encoder code in `frontends/`. Tested via golden files (Section 10).

**Stop-event ordering is a contract.** Canonical sequence: `ResponseStart → MessageStart → [ContentBlockStart → ...deltas... → ContentBlockStop]* → MessageStop → ResponseStop`. Provider parsers normalize to this even when upstream emits in a different order (e.g., OpenAI's `finish_reason` arrives on the last content delta chunk; the parser separates them).

**Mid-stream errors:**
1. **Before any byte sent:** convert to frontend HTTP error.
2. **After stream started, before `MessageStart`:** emit error event in protocol's shape and close. Anthropic has a real `error` event; OpenAI gets a final chunk with `finish_reason: "error"` + a synthetic delta error marker.
3. **Mid-content:** emit `MessageStop { stop_reason: Error }` + close. **No retry once bytes commit** (see Section 7).

**Heartbeats.** Per-frontend keepalive on a 15s timer when no upstream events have arrived (Anthropic `ping`, OpenAI `: comment\n\n`). Configurable, default on.

**Non-streaming requests.** Frontend `encode_unary` consumes the canonical stream into a single response. Same provider code path; the frontend collapses.

**Cancellation.** Client disconnect → axum drops response future → drops encoder → drops canonical stream → drops provider HTTP request. Tokio cancels cleanly. Provider trait contract: must not panic on drop mid-stream.

---

## 6. GitHub Copilot Adapter

The MVP-defining piece. Three layers of complexity: auth, token lifecycle, and header/endpoint quirks.

**Auth flow (one-time setup, per user):**

GitHub OAuth Device Flow against the Copilot OAuth client.

```
$ agent-shim copilot login
  → POST https://github.com/login/device/code
      client_id=<copilot-public-client-id>
      scope=read:user
  → prints: "Visit https://github.com/login/device, enter code XXXX-XXXX"
  → polls POST https://github.com/login/oauth/access_token
  → receives gho_<github_oauth_token>
  → persists to ~/.config/agent-shim/copilot.json (mode 0600)
```

Persisted token is the **GitHub OAuth token** (long-lived, user-revocable). It is *not* sent to the Copilot completions endpoint.

**Token exchange (per-session, automatic):**

```
GET https://api.github.com/copilot_internal/v2/token
  Authorization: token gho_<github_oauth_token>
  → { token: "tid=...;exp=...;...", expires_at: <unix>, refresh_in: <secs>, ... }
```

The returned token is a structured string with embedded expiry. The Copilot API base URL is *also* returned by this exchange (not hardcoded; can vary by user/region).

**Token cache:**

A `CopilotTokenManager` actor (single-task, channel-driven) owns:
- Current Copilot API token + parsed expiry
- The GitHub OAuth token (read once at startup, watched for file change)
- A refresh task that proactively re-exchanges at `refresh_in` (or `expires_at - 5min`, whichever is sooner)
- An on-demand "give me a valid token" channel that blocks the *first* caller during refresh and serves the rest from cache

Avoids thundering-herd refreshes. Max one in-flight refresh.

**Completion endpoint:**

```
POST <endpoint_from_token_exchange>/chat/completions
  Authorization: Bearer <copilot_api_token>
  Editor-Version: AgentShim/0.1.0
  Editor-Plugin-Version: AgentShim/0.1.0
  Copilot-Integration-Id: vscode-chat
  Openai-Intent: conversation-panel
  X-Request-Id: <uuid>
  Content-Type: application/json

  { ...OpenAI chat completions body... }
```

Body shape is OpenAI-compatible. Headers are non-negotiable; Copilot returns 400/403 without them. Available models come from a separate `/models` endpoint.

**Module layout:**

```
providers/src/github_copilot/
  mod.rs              // BackendProvider impl
  auth.rs             // device flow login (called from CLI subcommand)
  token_manager.rs    // actor + cache + proactive refresh
  models.rs           // /models endpoint, capability discovery
  request.rs          // CanonicalRequest → Copilot HTTP body (reuses openai_compatible's encoder)
  response.rs         // Copilot SSE → CanonicalStream (reuses openai_compatible's parser)
```

Body encoder and SSE parser are 90% the same as `openai_compatible`. We reuse those modules via `pub(crate)` helpers (`encode_request_body`, `parse_sse_stream`); only auth/headers/endpoint-discovery is Copilot-specific. No copy-paste.

**Capabilities declaration:**

```rust
ProviderCapabilities {
    streaming: true,
    tool_calling: true,
    vision: true,         // for gpt-4o, claude-3.5-sonnet
    reasoning: true,      // for o1
    json_mode: true,
    json_schema: true,    // gpt-4o
    available_models: dynamic, // discovered via /models
}
```

**Hard parts explicitly accepted:**
- The auth client_id is technically a "public secret" scraped from official Copilot extensions. We document this honestly; using AgentShim with Copilot requires a paid Copilot subscription (GitHub enforces server-side). Same posture as `copilot.lua`, `copilot.vim`.
- Endpoint URL discovery is dynamic — must call token exchange at least once to learn it.
- Rate limits are per-GitHub-account, not per-key. 429s are surfaced clearly, not retried blindly.
- Model availability changes — refresh `/models` on a slow timer (1h) so config validation can warn about unknown models.

**v0.1 scope on Copilot:** device flow login via CLI, single-account token cache (multi-account deferred), streaming + non-streaming chat completions, tool calling, all models from `/models`. Vision deferred (Copilot supports it, but our v0.1 doesn't ship vision end-to-end). Reasoning blocks deferred (Copilot doesn't expose o1's reasoning content anyway).

---

## 7. Hard Problems Catalog

**1. Tool-call streaming deltas.** Canonical event `ToolCallArgumentsDelta { json_fragment: String }` carries raw bytes; encoders concatenate and re-emit in their wire shape. Never parse mid-stream.

**2. System / developer prompt placement.** `Vec<SystemInstruction>` with `SystemSource` discriminator preserves origin. Single-prompt providers flatten with documented join (`\n\n`). Frontend encoders reconstruct the original placement on the response side.

**3. Reasoning / thinking blocks.** `ContentBlock::Reasoning` + `RedactedReasoning` first-class. Per-route policy (config) controls expose/suppress/summarize. v0.1: pass through when both sides support, drop silently when target doesn't, no summarization.

**4. Vision / multimodal formats.** `BinarySource` enum with URL / Base64 / Bytes / ProviderFileId variants. Provider capability flags gate routing; `CapabilityMismatch` error before upstream call. **v0.1 does not ship vision end-to-end** — types exist, encoders/parsers stubbed, golden test is TODO.

**5. Stop reason mapping.** Normalize to `StopReason` enum; preserve original in `Unknown(String)`. Documented mapping table next to each frontend encoder, unit-tested per direction.

**6. SSE wire-shape parity.** Frontend encoders are compatibility-critical code with **golden tests** captured from real upstream APIs. Canonical event set is a superset; encoders translate down.

**7. Retry safety after partial stream.** Hard rule: **retry/fallback only allowed pre-first-byte**. After `MessageStart` is emitted to the client, errors close the stream cleanly — no second attempt. v0.1 has no fallback chains; the canonical stream wraps with a "committed" flag so this stays correct when fallback ships in Phase 4.

**8. Token accounting.** `Usage` has explicit `Option` fields + `estimated: bool` + `provider_raw` verbatim copy. v0.1 just passes through; no client-side estimation.

**9. JSON mode / structured output.** `ResponseFormat` enum on canonical request. Anthropic frontend rejects with 400 for v0.1 (deferring synthesize-with-tool-use translation). OpenAI/Copilot path passes through.

**10. Model alias resolution.** Router has explicit alias map in config. Unknown models: configurable behavior (`reject` | `passthrough_to_default`). v0.1 default is `reject` with a clear error listing known aliases.

**11. Header forwarding & client identity.** Whitelist of forwarded headers per frontend; everything else dropped. Client IP / forwarded-for handled at axum middleware layer, not in adapters.

**12. Cancellation / disconnect.** Drop chain via tokio (Section 5). Providers must not panic on drop. Tested via fuzz-style integration test that disconnects at random byte offsets.

**13. Anthropic prompt caching (`cache_control`).** Lives in `extensions` on the relevant `ContentBlock`. Anthropic frontend decoder writes it; Anthropic provider encoder reads it; everyone else ignores. Lossless round-trip for Anthropic→Anthropic; silently dropped for cross-protocol — documented behavior.

**Deferred:** parallel tool-call hints (extension-only), logprobs (extension-only), audio I/O (types exist, no wiring).

---

## 8. Project Layout

```
agent-shim/
  Cargo.toml                          # workspace manifest
  Cargo.lock
  rust-toolchain.toml                 # pin MSRV
  rustfmt.toml
  clippy.toml
  deny.toml                           # cargo-deny config
  README.md
  LICENSE                             # MIT

  crates/
    core/                             # leaf — zero I/O
      Cargo.toml
      src/
        lib.rs
        request.rs
        message.rs
        content.rs
        tool.rs
        media.rs
        stream.rs
        usage.rs
        ids.rs
        capabilities.rs
        target.rs
        extensions.rs
        error.rs

    frontends/
      Cargo.toml
      src/
        lib.rs                        # FrontendProtocol trait, registry
        anthropic_messages/
          mod.rs
          decode.rs
          encode_unary.rs
          encode_stream.rs
          wire.rs
          mapping.rs
        openai_chat/
          mod.rs
          decode.rs
          encode_unary.rs
          encode_stream.rs            # incl. [DONE] terminal + heartbeat comments
          wire.rs
          mapping.rs

    providers/
      Cargo.toml
      src/
        lib.rs                        # BackendProvider trait, ProviderRegistry
        openai_compatible/
          mod.rs
          encode_request.rs           # pub(crate) — reused by copilot
          parse_stream.rs             # pub(crate) — reused by copilot
          parse_unary.rs
          config.rs
        github_copilot/
          mod.rs
          auth.rs
          token_manager.rs
          models.rs
          headers.rs
          endpoint.rs

    router/
      Cargo.toml
      src/
        lib.rs
        static_routes.rs              # v0.1: model alias → BackendTarget map
        target.rs
        # Phase 4 stubs (file exists, types defined, behavior trivial):
        fallback.rs
        rate_limit.rs
        circuit_breaker.rs

    config/
      Cargo.toml
      src/
        lib.rs
        schema.rs
        loader.rs
        validation.rs
        secrets.rs
        # reload.rs                   # Phase 5

    observability/
      Cargo.toml
      src/
        lib.rs
        tracing_setup.rs
        request_id.rs
        redaction.rs
        # metrics.rs                  # Phase 5
        # capture.rs                  # Phase 5

    gateway/
      Cargo.toml
      src/
        main.rs                       # clap entry; subcommands: serve, copilot login, validate-config
        cli.rs
        server.rs
        state.rs
        shutdown.rs
        commands/
          serve.rs
          copilot_login.rs
          validate_config.rs

    protocol-tests/
      Cargo.toml                      # [dev-dependencies] only; not published
      tests/
        anthropic_unary.rs
        anthropic_stream.rs
        anthropic_tool_calls.rs
        openai_unary.rs
        openai_stream.rs
        openai_tool_calls.rs
        cross_anthropic_to_openai.rs
        cross_openai_to_anthropic.rs
        copilot_e2e.rs                # uses recorded fixtures, not live API
        cancellation.rs               # disconnect at random offsets
      fixtures/
        anthropic/
          text_stream.sse
          tool_call_stream.sse
        openai/
        copilot/

  config/
    gateway.example.yaml              # commented reference config
    gateway.minimal.yaml              # smallest working config

  deploy/
    Dockerfile                        # multi-stage; final = scratch + binary + ca-certificates
    docker-compose.yaml
    # kubernetes/                     # Phase 5

  docs/
    Requirements.md
    superpowers/
      specs/
        2026-04-28-agent-shim-design.md
    architecture.md
    providers/
      openai-compatible.md
      github-copilot.md               # incl. device-flow walkthrough
    frontends/
      anthropic-messages.md
      openai-chat-completions.md
    configuration.md
    deployment.md
    contributing.md                   # how to add a new provider/frontend

  .github/
    workflows/
      ci.yaml                         # fmt + clippy + nextest + cargo-deny
      release.yaml                    # cross-compile binaries + Docker image
    ISSUE_TEMPLATE/
    PULL_REQUEST_TEMPLATE.md

  scripts/
    capture-fixtures.sh               # re-record golden SSE from live APIs
    bench.sh                          # criterion runner
```

**Layout choices:**
- Phase-4/5 modules ship as no-op stubs in v0.1. Avoids future API breaks; makes architecture visible from day one.
- `protocol-tests` is its own crate, not scattered `tests/` directories. End-to-end fidelity is a system property.
- `fixtures/` checked into the repo. Captured SSE files are kilobytes; golden tests are worthless without them.
- No `examples/` directory in v0.1.

---

## 9. Roadmap

**Phase 1 — v0.1 MVP.** Anthropic + OpenAI frontends, OpenAI-compatible + Copilot backends, tool calling, streaming, static config, structured logging. Definition of done: Claude Code → Copilot works; Codex → DeepSeek (via OpenAI-compat) works; both with streaming + tools; gateway overhead under 5ms p99 measured.

**Phase 2 — v0.2 Provider breadth.** Native adapters for providers whose quirks the OpenAI-compat shim doesn't handle cleanly: DeepSeek (reasoning content + cache hints), Qwen/DashScope, Kimi/Moonshot, Gemini (genuinely different API shape), Doubao/Volcengine, Grok/xAI. Each adapter is a single module + golden test fixtures. Capability declarations filled in honestly per provider. Vision wires up end-to-end.

**Phase 3 — v0.3 OpenAI Responses API frontend.** `/v1/responses` is the future for Codex-family agents, with a richer item-based event model. Own frontend module + encoder, sharing canonical core.

**Phase 4 — v0.4 Routing & reliability.** Fill stubbed router modules: fallback chains with the pre-first-byte rule; per-provider/per-key circuit breakers; key pools with weighted load balancing; per-route retries with exponential backoff and jitter; per-agent API keys with scopes; per-key rate limiting; request budget caps; timeout policies. Cost/latency-aware routing (optional) lands here.

**Phase 5 — v0.5 Observability & ops.** Prometheus metrics; optional redacted request/response capture; hot-reloadable config with validation + rollback-on-error; Kubernetes manifests; OpenTelemetry traces.

**Phase 6 — v1.0 polish.** Audio/file end-to-end if not already pulled in; multi-account Copilot; benchmarks published; security audit; SemVer commitment on the canonical model.

**Sequencing rationale:** breadth before reliability because OSS adoption is gated on "does it support my provider," not on "does it have circuit breakers." Routing/reliability before observability because you can't measure SLOs you don't enforce yet.

**Permanently out of scope:** embeddings/moderation/audio-transcription endpoints, built-in billing, bundled admin UI, end-user identity provider.

---

## 10. Testing Strategy

Three layers. Total runtime target: under 60s for the full suite via `cargo nextest`.

**Layer 1 — Unit tests, co-located.** Standard `#[cfg(test)] mod tests`. Pure logic: stop-reason mapping, role mapping, JSON-fragment accumulation, token cache state machine, config validation, alias resolution. No I/O, no async runtime. Coverage target: 80% on `core/`, `frontends/*/mapping.rs`, `router/`, `config/`.

**Layer 2 — Per-protocol golden SSE tests, in `protocol-tests` crate.** For each (frontend, scenario) pair we hold a captured wire-format SSE file and assert exact byte-equivalent output:

```rust
#[tokio::test]
async fn anthropic_text_stream_roundtrip() {
    let canonical = anthropic_decoder::decode_request(captured_request());
    let canonical_stream = mock_provider_stream("fixtures/canonical/text_stream.jsonl");
    let bytes = anthropic_encoder::encode_stream(canonical_stream).collect_bytes().await;
    assert_eq!(bytes, read_fixture("fixtures/anthropic/text_stream.sse"));
}
```

Two variants per scenario: same-protocol round-trip (must be lossless on wire bytes that matter) and cross-protocol (assert event semantics, not byte equality). Fixtures checked in; `scripts/capture-fixtures.sh` re-records from live APIs.

A `MockProvider` in `protocol-tests` reads `.jsonl` of canonical events and replays them as a `CanonicalStream` with configurable per-event delays.

**Layer 3 — End-to-end integration against live APIs, opt-in.** Behind `--features live` and `AGENT_SHIM_E2E=1`. Real `agent-shim serve` against real `api.deepseek.com` / Copilot / etc. Catches upstream API drift. Nightly workflow with secrets, plus on-demand via PR label. Two scenarios per provider: streaming hello-world + tool-call round trip.

**Cancellation/disconnect fuzz test (Layer 2).** `cancellation.rs` streams a long response and disconnects at random byte offsets (seeded RNG). Asserts no panics, no leaked tasks, upstream connection cleanly aborted. ~50 iterations per CI run.

**Performance benchmark.** `criterion` benches in `benches/`:
- Canonical request encode/decode round-trips per frontend (target: < 10µs)
- Full request path with mock upstream (target: gateway overhead < 5ms p99)
- 1000 concurrent streams with mock upstream (memory + scheduler overhead)

Baseline numbers committed to `benches/baseline.json`. CI fails on >10% regression on PRs touching hot paths.

**Property tests (`proptest`) for the canonical model.** Generators for `CanonicalRequest` and `StreamEvent`; assert encode-then-decode round-trips on each frontend. Catches "field added but encoder doesn't emit it" bugs. Small set initially (5–10 properties).

**Not tested automatically:** GitHub OAuth device flow (manually verified). Real load tests beyond the 1000-stream criterion bench (Phase 5 ops concern).

**Coverage policy.** 80% line coverage via `cargo llvm-cov` on `core/`, `router/`, `config/`, and `mapping.rs` files. Lower bar on adapter I/O code. Reported in CI but not gating below 80% — gating mandatory at v0.2 once surface stabilizes.

---

## 11. Out of Scope for v0.1

**Frontends not in v0.1:**
- OpenAI `/v1/responses` — Phase 3
- OpenAI `/v1/embeddings`, `/v1/moderations`, `/v1/audio/*`, `/v1/images/*` — indefinitely out
- Gemini-native frontend — never (Gemini is a backend only)

**Backends not in v0.1:**
- Native DeepSeek, Kimi, Qwen, Doubao, Gemini, Grok adapters — Phase 2. Most work in v0.1 via OpenAI-compatible adapter with appropriate config; v0.1 just doesn't ship per-provider quirk handling.
- Anthropic-as-backend (passthrough) — Phase 2.
- Local model runtimes (Ollama, vLLM, llama.cpp server) — work via OpenAI-compat in v0.1; no special-casing.

**Features not in v0.1:**
- Vision / multimodal end-to-end (types exist, no wiring)
- Audio I/O
- Embeddings / reranking
- Routing fallback chains, circuit breakers, retries with backoff — Phase 4
- Per-key rate limiting, quotas, per-agent API keys with scopes — Phase 4
- Cost/latency-aware routing — Phase 4 (optional)
- Prometheus metrics — Phase 5
- Hot-reloadable config — Phase 5
- Request/response capture for debugging — Phase 5
- OpenTelemetry traces — Phase 5
- Kubernetes manifests — Phase 5
- Multi-account Copilot — Phase 6
- Admin UI — separate project, possibly never
- Built-in billing / metering — out indefinitely
- End-user authentication / identity — out indefinitely
- JSON-mode synthesis on Anthropic frontend (returns 400 in v0.1) — Phase 2 or later
- Reasoning content summarization — Phase 4+ if ever
- Structured outputs translation across protocols (just passthrough in v0.1)

**Operational stances:**
- Single-process, single-host. No clustering, no shared state, no Redis. Horizontal scale = run more binaries behind a load balancer.
- File-based secrets (env vars + YAML). No Vault, no AWS Secrets Manager integration.
- No per-tenant isolation. Multi-tenant is a Phase 4 concern after API keys land.
