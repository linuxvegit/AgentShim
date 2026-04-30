# Plan 01 — `oai_chat_wire/` Extraction + Anthropic-as-Backend Provider

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-04-30-phase-2-provider-breadth-design.md`](../specs/2026-04-30-phase-2-provider-breadth-design.md) (decisions D2, D7, D8).

**Goal:** Land the foundational shared lib and the first new v0.2 provider in the same plan. Two coupled work items because (a) the `oai_chat_wire/` extraction is non-feature-bearing prep that's dead weight on its own, and (b) the Anthropic provider lands the hybrid passthrough+canonical pattern that subsequent providers reference.

**Architecture:**
- Extract shared OpenAI-Chat-shape primitives from `crates/providers/src/openai_compatible/` into a new crate-internal lib `crates/providers/src/oai_chat_wire/`. `openai_compatible/` and (subsequently) `github_copilot/` compose it via re-exports during the migration; sibling crates never reach into each other.
- Promote shared Anthropic mapping tables (stop-reason, role, tool_use ↔ tool_call) from the frontend into `agent-shim-core::mapping::anthropic_wire` so both edges depend on it without violating the boundary rule (frontends and providers never import each other).
- Add `crates/providers/src/anthropic/` with one `BackendProvider` impl that branches in `complete()` on `req.frontend.kind`: `AnthropicMessages` → bytes-passthrough via `proxy_raw`; everything else → canonical encode/decode.
- New config variant `UpstreamConfig::Anthropic(AnthropicUpstream)` with API-key auth, configurable `anthropic-version` header.

**Tech stack:** No new dependencies. Reuses `reqwest`, `serde_json`, `tokio`, `async-trait`, `bytes`, `futures-util`. Tests: `mockito` (existing), inline canonical-event assertions (D10).

**Core changes:** NONE (v0.2 frozen-core policy, ADR-0002). Mapping-table promotion is a *move* from `frontends/` to `core/`, not a type addition.

---

## File Structure

`crates/core/`:
- Create: `crates/core/src/mapping/anthropic_wire.rs`
- Modify: `crates/core/src/lib.rs` (add `pub mod mapping;`)
- Create: `crates/core/src/mapping/mod.rs`

`crates/frontends/`:
- Modify: `crates/frontends/src/anthropic_messages/mapping.rs` (re-export from core)
- Modify: `crates/frontends/src/anthropic_messages/{decode,encode_stream,encode_unary}.rs` (point to core)

`crates/providers/`:
- Create: `crates/providers/src/oai_chat_wire/mod.rs`
- Create: `crates/providers/src/oai_chat_wire/canonical_to_chat.rs` (move from `openai_compatible/encode_request.rs`)
- Create: `crates/providers/src/oai_chat_wire/chat_sse_parser.rs` (move from `openai_compatible/parse_stream.rs`)
- Create: `crates/providers/src/oai_chat_wire/chat_unary_parser.rs` (move from `openai_compatible/parse_unary.rs`)
- Modify: `crates/providers/src/openai_compatible/mod.rs` (re-exports + delegate)
- Modify: `crates/providers/src/openai_compatible/{encode_request,parse_stream,parse_unary}.rs` → become thin shims pointing at `oai_chat_wire`, then deleted in step 5
- Modify: `crates/providers/src/github_copilot/mod.rs` (update imports to `oai_chat_wire`)
- Create: `crates/providers/src/anthropic/mod.rs`
- Create: `crates/providers/src/anthropic/passthrough.rs`
- Create: `crates/providers/src/anthropic/request.rs`
- Create: `crates/providers/src/anthropic/response.rs`
- Modify: `crates/providers/src/lib.rs` (add `pub mod oai_chat_wire; pub mod anthropic;`)

`crates/config/`:
- Modify: `crates/config/src/schema.rs` (add `Anthropic(AnthropicUpstream)` variant)
- Modify: `crates/config/src/validation.rs` (validate the new variant)

`crates/gateway/`:
- Modify: `crates/gateway/src/state.rs` or wherever providers are registered (wire `anthropic` upstream from config)

`crates/protocol-tests/`:
- Create: `crates/protocol-tests/tests/anthropic_passthrough.rs`
- Create: `crates/protocol-tests/tests/anthropic_canonical.rs`
- Create: `crates/protocol-tests/tests/cross_openai_chat_to_anthropic.rs`
- Create fixtures under `crates/protocol-tests/fixtures/anthropic/`:
  - `text_stream.{request,upstream,expected}.{json,sse}`
  - `tool_call_stream.{request,upstream,expected}.{json,sse}`
  - `cache_control_passthrough.{request,upstream,expected}.{json,sse}`

Root:
- Modify: `config/gateway.example.yaml` (add `anthropic` upstream commented example)
- Modify: `README.md` (mention Anthropic backend in capability list)

---

## Tasks

### Task 1: Promote shared Anthropic mapping tables to `agent-shim-core`

- [ ] Create `crates/core/src/mapping/mod.rs` exposing `pub mod anthropic_wire;`.
- [ ] Move stop-reason translation, role mapping, and tool_use ↔ tool_call mapping from `crates/frontends/src/anthropic_messages/mapping.rs` into `crates/core/src/mapping/anthropic_wire.rs`.
- [ ] Update `frontends/anthropic_messages/mapping.rs` to re-export from core for backward compat.
- [ ] Run `cargo nextest run -p agent-shim-frontends` — must stay green.

### Task 2: Extract `oai_chat_wire/` lib

- [ ] Create `crates/providers/src/oai_chat_wire/{mod,canonical_to_chat,chat_sse_parser,chat_unary_parser}.rs`.
- [ ] Move logic from `openai_compatible/{encode_request,parse_stream,parse_unary}.rs` into the new modules verbatim.
- [ ] Replace `openai_compatible/{encode_request,parse_stream,parse_unary}.rs` with thin re-exports of `oai_chat_wire::*` (kept as a compat shim during this plan).
- [ ] Update `crates/providers/src/lib.rs` to expose `pub mod oai_chat_wire;`.
- [ ] Migrate `github_copilot/mod.rs` imports from `crate::openai_compatible::{encode_request,parse_stream,parse_unary}` → `crate::oai_chat_wire::*`.
- [ ] Run full test suite (`cargo nextest run --workspace`) — must stay green. **Failure here means the extraction was lossy; fix before continuing.**
- [ ] Delete the now-unused shim files from `openai_compatible/` once all imports migrated.

### Task 3: Add `agent-shim-config` Anthropic upstream variant

- [ ] In `crates/config/src/schema.rs`, add:

```rust
pub enum UpstreamConfig {
    OpenAiCompatible(OpenAiCompatibleUpstream),
    GithubCopilot(GithubCopilotUpstream),
    Anthropic(AnthropicUpstream),       // NEW
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicUpstream {
    pub api_key: Secret<String>,
    #[serde(default = "default_anthropic_base_url")]
    pub base_url: String,
    #[serde(default = "default_anthropic_version")]
    pub anthropic_version: String,
    #[serde(default = "default_request_timeout")]
    pub request_timeout_secs: u64,
    #[serde(default)]
    pub default_headers: BTreeMap<String, String>,
}

fn default_anthropic_base_url() -> String { "https://api.anthropic.com".into() }
fn default_anthropic_version() -> String { "2023-06-01".into() }
```

- [ ] Add validation: api_key non-empty, base_url parses as URL, anthropic_version non-empty.
- [ ] Add unit test asserting valid + invalid YAML cases.

### Task 4: Implement Anthropic provider — passthrough path

- [ ] In `crates/providers/src/anthropic/mod.rs` define `AnthropicProvider` struct holding `name`, `base_url`, `api_key`, `anthropic_version`, `client`, `default_headers`, `capabilities` (`{ streaming: true, tool_use: true, vision: true, json_mode: false }`).
- [ ] In `passthrough.rs`, implement the bytes-forwarding path. Headers attached: `x-api-key`, `anthropic-version`, `content-type: application/json`, plus any `anthropic-beta` from `target.policy.default_anthropic_beta` (per existing pattern).
- [ ] Wire `BackendProvider::proxy_raw` to call into `passthrough::send`. Unlike OAI-compat, no `model` rewrite is needed when frontend == anthropic_messages (the inbound body already has the right model name; the route's `upstream_model` rewrite happens during decode). **Verify this assumption** by reading `frontends::anthropic_messages::decode` — if it preserves the inbound model verbatim, the passthrough should still apply target.model rewrite for safety. Document the choice.
- [ ] In `complete()`, branch on `req.frontend.kind`. If `AnthropicMessages` and `req.stream`, take the passthrough fast path: re-serialize the canonical request as Anthropic JSON (or — preferred — capture the original bytes earlier in the pipeline; if bytes aren't available at the provider boundary today, the fast path encodes from canonical and accepts a small re-serialization cost. Investigate gateway pipeline to determine if raw bytes can be threaded through).

### Task 5: Implement Anthropic provider — canonical path

- [ ] In `request.rs`, write `build(req: &CanonicalRequest, target: &BackendTarget) -> serde_json::Value` that produces an Anthropic Messages JSON body. Reuses mapping tables from `agent-shim-core::mapping::anthropic_wire` (introduced in Task 1).
- [ ] In `response.rs`, write `parse(stream: BytesStream) -> CanonicalStream` parsing Anthropic SSE events (`message_start`, `content_block_start`, `content_block_delta`, `content_block_stop`, `message_delta`, `message_stop`, `ping`, `error`) into `StreamEvent`s. Reuses Anthropic event-shape knowledge from frontend encoder (mapping-tables-only, no frontend code import).
- [ ] In `complete()`, when `req.frontend.kind != AnthropicMessages`, take the canonical path: build body, send POST, parse SSE.
- [ ] Both paths share auth header construction and error-mapping helpers — extract a `headers.rs` module if duplication exceeds ~20 lines.

### Task 6: Wire provider into gateway

- [ ] In `crates/providers/src/anthropic/mod.rs`, add `pub fn from_config(name: &str, cfg: &AnthropicUpstream) -> Result<AnthropicProvider, ProviderError>` mirroring `openai_compatible::from_config`.
- [ ] In `crates/gateway/src/state.rs` (or wherever provider registration lives), wire the `Anthropic` variant: instantiate via `from_config`, register in `ProviderRegistry`.
- [ ] Run `cargo build --workspace` — must compile clean.

### Task 7: Tests — passthrough fixtures

- [ ] Create fixtures `crates/protocol-tests/fixtures/anthropic/text_stream.{request,upstream,expected}.{json,sse}`. The `request` is a real Anthropic Messages request body. The `upstream` is the raw SSE bytes Anthropic emits. The `expected` is what AgentShim emits (byte-equal to upstream for the passthrough path).
- [ ] Create fixtures for `tool_call_stream.*` and `cache_control_passthrough.*`.
- [ ] Capture script: `scripts/capture-anthropic-fixtures.sh` that, given `ANTHROPIC_API_KEY`, runs `curl` against api.anthropic.com and saves outputs. Gated by `--features live` indirectly (just runnable if the key is set).
- [ ] Test `crates/protocol-tests/tests/anthropic_passthrough.rs`: spin up the gateway with mockito-mocked Anthropic API serving the fixture's `upstream` SSE, send the fixture's `request`, assert byte-equality with `expected`.

### Task 8: Tests — canonical path

- [ ] Test `crates/protocol-tests/tests/anthropic_canonical.rs`: send a `CanonicalRequest` with `frontend.kind = OpenAiChat` (synthetic, not coming from the OpenAI frontend decoder) directly to the Anthropic provider's `complete()`. Assert canonical events emitted match expected `Vec<StreamEvent>`.
- [ ] Test `crates/protocol-tests/tests/cross_openai_chat_to_anthropic.rs`: full request through the gateway. POST `/v1/chat/completions`, route to Anthropic upstream, mockito serves Anthropic SSE, assert OpenAI Chat-shape SSE comes back to the client.

### Task 9: Documentation + config example

- [ ] Add `docs/providers/anthropic.md` documenting setup, capability flags, sample routes, the hybrid path's user-visible behavior (cache_control round-trips losslessly only when frontend is `anthropic_messages`).
- [ ] Add commented Anthropic upstream + route example to `config/gateway.example.yaml`.
- [ ] Update `README.md` capability list to include Anthropic backend.

### Task 10: Verification gate

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo nextest run --workspace`
- [ ] `cargo deny check`
- [ ] Manual smoke: `cargo run -p agent-shim -- validate-config --config config/gateway.example.yaml` succeeds.

**Success criterion:** Claude Code → AgentShim → api.anthropic.com works (via passthrough) with `cache_control` markers preserved on the wire. Codex/Cursor → AgentShim → api.anthropic.com works (via canonical path) with text streaming and tool calls. All tests green; no warnings.
