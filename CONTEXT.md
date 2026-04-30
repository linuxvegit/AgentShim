# Domain language

Shared vocabulary used across the AgentShim codebase. Use these terms exactly when reading, writing, or reviewing — including in skills, ADRs, and design docs.

## Entities

**Frontend**
The inbound API dialect adapter. One per protocol an agent might speak: `anthropic_messages`, `openai_chat`, `openai_responses`. A frontend decodes inbound requests into the **canonical model** and encodes outbound streams/responses back into its dialect.

**Provider**
The outbound backend client. One per backend family: `openai_compatible`, `github_copilot`. A provider receives a `CanonicalRequest` plus a `BackendTarget` and returns a `CanonicalStream`.

**Canonical model**
Protocol-neutral types living in `agent-shim-core`. Both frontends and providers depend on canonical types and never on each other.

**Route**
A config entry binding `(frontend, model_alias) → (upstream, upstream_model, route_policy)`. Lives in `gateway.yaml` under `routes:`.

**Upstream**
A backend service the gateway can talk to (e.g. DeepSeek, Copilot, Ollama). Configured under `upstreams:`.

**Router**
The component that resolves `(frontend_kind, model_alias) → BackendTarget` from the route table.

**BackendTarget**
The output of route resolution. Identifies the upstream provider, the model name to send upstream, and the **route policy** for this route.

**Route policy** *(`RoutePolicy`)*
Per-route defaults that fill in when the inbound request didn't supply a value. Today: default reasoning effort, default `anthropic-beta` header. Owns the **policy merge rule** — "inbound wins, else route default, else nothing." Lives in `agent-shim-core::policy`.

**Resolved policy** *(`ResolvedPolicy`)*
The output of `RoutePolicy::resolve(canonical_request)`. A per-request snapshot of the merged values, stored on `CanonicalRequest.resolved_policy`. Providers read from this; they do not consult `RoutePolicy` directly.

**Reasoning effort**
Qualitative thinking-effort level: `minimal | low | medium | high | xhigh`. Cross-dialect translation:
- Anthropic `thinking: { budget_tokens }` → `ReasoningOptions.budget_tokens`
- OpenAI `reasoning_effort` → `ReasoningOptions.effort`
- OpenAI Responses `reasoning.effort` → `ReasoningOptions.effort`

Forwarded outbound as `reasoning_effort` (chat completions) or `reasoning.effort` (Responses API).

**Anthropic beta header**
An `anthropic-beta` HTTP header value (e.g. `context-1m-2025-08-07`) that toggles a feature without changing the model name. Captured from the inbound request, replayed verbatim on the outbound, with a per-route fallback.

## Stream events

`StreamEvent` is the canonical-model tagged union: `ResponseStart`, `TextDelta`, `ToolCallArgumentsDelta`, `ReasoningDelta`, `UsageDelta`, `MessageStop`, etc. Frontends and providers translate to/from this.

**Interleaved reasoning**
The canonical model carries reasoning as `ContentBlock::Reasoning` blocks ordered alongside `Text` and `ToolCall` blocks in the *same* `Vec<ContentBlock>`. Providers parse upstream output with an `in_reasoning: bool` state machine and emit `ContentBlockStart`/`ContentBlockStop` events around each reasoning↔text transition. Anthropic's `thinking → text → tool_use → thinking` pattern round-trips losslessly. DeepSeek's "all reasoning, then all content" is the degenerate case (one reasoning block, then one text block). Gemini's `thoughts: bool` flag on `parts` uses the same machinery. Lives in `agent-shim-providers::oai_chat_wire::interleaved_reasoning`.

## Phase 2 architecture

**Native provider**
A `BackendProvider` impl built specifically for one upstream's wire format and quirks. Distinct from the generic `openai_compatible` shim — natives handle reasoning fields, cache-usage mappings, non-OpenAI streaming formats (e.g. Gemini's JSON-array stream), etc. v0.2 ships natives for `deepseek`, `gemini`, and `anthropic`.

**oai_chat_wire**
Crate-internal lib at `agent-shim-providers::oai_chat_wire` shared between the OpenAI-compat provider and any "OpenAI-Chat-shape with quirks" provider (DeepSeek, future Kimi/Qwen). Owns `canonical_to_chat`, `chat_sse_parser`, `chat_unary_parser`, `interleaved_reasoning`. Sibling provider modules compose it; they never `pub(crate)`-import each other.

**Hybrid Anthropic path**
The Anthropic-as-backend provider has two paths inside one `BackendProvider::complete()`:
- **Passthrough path** — when `req.frontend.kind == FrontendKind::AnthropicMessages`, proxy the raw inbound bytes through `BackendProvider::proxy_raw`. Round-trip is byte-for-byte lossless on Anthropic-only features (`cache_control`, `thinking`, beta headers).
- **Canonical path** — for any other frontend, translate `CanonicalRequest` → Anthropic Messages JSON and parse Anthropic SSE → `CanonicalStream`.

Architectural invariant: same prompt routed through both paths must produce semantically equivalent output.

**Capability gate**
Pre-network-call check raised as `ProviderError::CapabilityMismatch` when the frontend sent content (e.g. an image) the target provider's `ProviderCapabilities` says it can't handle. Frontend renders a 400 in its dialect. Replaces the alternative of letting upstream return a confusing error.

**Extensions namespace**
`ContentBlock`, `Message`, `CanonicalRequest`, and `CanonicalResponse` carry `extensions: HashMap<String, serde_json::Value>` for protocol-specific data not promoted to canonical fields. v0.2 convention: keys are namespaced by provider — `gemini.safety_ratings`, `anthropic.cache_creation`, `deepseek.<...>`. Documented first-class behaviors live in `docs/providers/<provider>.md`. Promotion to typed canonical fields happens in v0.3 based on cross-provider read patterns, not prediction.

## Glossary maintenance

When introducing a new domain concept, add it here in the same paragraph it gets named. Don't rename existing terms without a search-and-replace across the codebase.
