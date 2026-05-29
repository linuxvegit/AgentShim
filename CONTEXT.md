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

**Model index** *(`ModelIndex`)*
Per-provider map of `BTreeMap<String, ModelMetadata>` populated from each upstream's `/models` response at startup (and on `POST /admin/reload` / SIGHUP). Backs both the existing fuzzy resolver (used during `Router::resolve` to upgrade `gpt-4o` → `gpt-4o-2024-11-20`) and the new catalog surfaces (`/v1/models`, `/admin/catalog`, `agent-shim show-catalog`). Lives in `agent-shim-router::model_index`.

**Model catalog** *(`ModelCatalog`)*
The composed view of (a) the route table — which model aliases AgentShim accepts on which frontend — and (b) the upstream-discovered capabilities for each alias's `BackendTarget`. Built by `ModelResolver::list_catalog` from the same inputs `resolve` consumes. Surfaced on `GET /v1/models` (public, OpenAI shape) and `GET /admin/catalog` (operator, with policy extras).

**Model record** *(`ModelRecord`)*
One entry in the catalog. Carries the alias, the resolved upstream chain head, the full chain (length 1 for singular routes, N for fallback chains), the upstream metadata, and the same-family long-context sibling alias if any. Lives in `agent-shim-core::catalog`.

**Model metadata** *(`ModelMetadata`)*
Structured upstream capability blob: context window, max output tokens, family, supports flags (vision, tool calls, streaming, structured outputs, reasoning effort), version. The same struct backs `ModelIndex` entries and `ModelRecord.metadata`. Lives in `agent-shim-core::catalog`.

**BackendTarget**
The output of route resolution. Identifies the upstream provider, the model name to send upstream, and the **route policy** for this route.

**Route policy** *(`RoutePolicy`)*
Per-route defaults that fill in when the inbound request didn't supply a value. Today: default reasoning effort, default `anthropic-beta` header, and a **reasoning mapping table**. Owns the **policy merge rule** — "inbound wins, else route default, else nothing." Lives in `agent-shim-core::policy`.

**Resolved policy** *(`ResolvedPolicy`)*
The output of `RoutePolicy::resolve(canonical_request)`. A per-request snapshot of the merged values, stored on `CanonicalRequest.resolved_policy`. Providers read from this; they do not consult `RoutePolicy` directly.

**Reasoning effort**
Qualitative thinking-effort level drawn from the six-value canonical vocabulary `minimal | low | medium | high | xhigh | max`. Cross-dialect translation is per-direction:
- Anthropic inbound `output_config.effort` (`effort-2025-11-24` beta) → `ReasoningOptions.effort`
- OpenAI Chat inbound `reasoning_effort` (`minimal/low/medium/high`) → `ReasoningOptions.effort`
- OpenAI Responses inbound `reasoning.effort` → `ReasoningOptions.effort`
- Outbound Anthropic: `Minimal → "low"` (no minimal upstream); else identity
- Outbound OpenAI Chat (non-Copilot) and Responses API: `Xhigh|Max → "high"`
- Outbound Copilot Chat (`accepts_xhigh = true`): `Xhigh|Max → "xhigh"`

**Reasoning mapping table** *(`reasoning_mapping`)*
Optional per-route ordered list of `{ match, set }` rules over canonical effort. First rule whose `match` equals the post-default inbound effort fires its `set`; unmatched passes through. Both fields are canonical-vocabulary effort strings (`minimal/low/medium/high/xhigh/max`), NOT raw inbound or outbound dialect strings. Lives in `agent-shim-core::policy::RoutePolicy`.

**Mapping rule** *(`MappingRule`)*
One entry in a reasoning mapping table — `{ match: ReasoningEffort, set: ReasoningEffort }`.

**Effort vocabulary**
Six canonical levels: `Minimal | Low | Medium | High | Xhigh | Max`. The 2026-05-28 mapping spec retired the older five-value list by adding `Max`, and the mapping table operates exclusively on this vocabulary (not on per-dialect strings).

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

## Plugin system (Phase 7 candidate)

**Plugin**
A YAML-declared, named entry under `plugins:` in `gateway.yaml`. Carries a `kind:` (which Rust implementation), a `config:` blob (kind-specific), an `on_error:` policy, and a `timeout_ms:`. Routes reference plugins by name to attach them to a hook. Analogous shape to **Upstream** — a named, configured entity. Distinct from **PluginKind** (the Rust type).

**PluginKind**
The Rust implementation of a plugin behavior. Built from a `PluginFactory` at startup. One kind can back many plugins (each with its own config). Naming follows the project's existing `FrontendKind` precedent — the "type" suffix is `Kind`, not "type" or "class". Not part of operator-facing language; lives in code and tests only.

**Hook**
One of four well-defined points in the request lifecycle where plugins run: `on_decoded_request`, `on_resolved`, `on_stream_event`, `on_response_complete`. Each plugin subscribes to a subset of hooks; route YAML attaches plugins to a hook in a chosen order.

## Admission unification (Phase 8 candidate)

**Resolved route** *(`ResolvedRoute`)*
The output of `ModelResolver::resolve_route(frontend, model_alias)`. One value carrying the route-resolution facts the immutable `ModelResolver` owns: the fallback `chain: Vec<BackendTarget>`, the route-uniform `retry: RetryConfig`, the route-uniform `breaker: BreakerConfig`, and the `route_label: Arc<str>` (`<frontend_kind>/<alias>`, used by metric labels). Replaces the v0.7 pattern of `resolve` + `find_retry_policy` + `find_breaker_policy` as three parallel lookups on the same `(frontend, model)` pair. Lives in `agent-shim-router`. Catalog (`list_catalog`) shares the same constructor.

The cost-filter route fields (`min_tier`, `max_cost_usd`, `latency_budget_ms`) are deliberately **not** carried on `ResolvedRoute` because they live on a different reload epoch — see the routes-table reload split below.

**Routes-table reload split**
The `routes:` table in `gateway.yaml` is read from two reload epochs depending on the field. The fields the immutable `ModelResolver` owns (`provider`, `model`, `upstream`, `upstreams`, `policy`, `retry`, `breaker`) are startup-frozen: `ModelResolver` lives on `AppCore` and is never rebuilt on reload. The cost-filter fields (`min_tier`, `max_cost_usd`, `latency_budget_ms`) are hot-reloadable: they are read from `snapshot.config.routes` via the `find_route_entry` helper at request time, and pick up changes on the next `POST /admin/reload` or SIGHUP. Operators who change a route's `provider` or `upstream` chain mid-process must restart; cost caps tune live. Documented here because the split is field-level on the same record and easy to miss.

**Admission**
The Module that decides "may this request reach an upstream, and if so which subset of the chain." Composes the v0.4 rate-limit gate, the v0.6 cost filter, and the capability gate behind one Interface: `Admission::admit(resolved, request, identity, client_ip) -> Result<AdmissionTicket, AdmissionError>`. Lives in `agent-shim-router`. Replaces the v0.7 split where canonical requests went through `ResilientCaller::complete_with_cost_filter` while passthrough requests skipped both rate-limit and breaker gates (the KNOWN GAP previously documented at `crates/gateway/src/pipeline.rs` L492–505).

**Admission ticket** *(`AdmissionTicket`)*
The RAII value `Admission::admit` returns on success. Carries the cost-filter-survivor `chain`, an `Arc<ResolvedRoute>`, and private RAII handles for the rate-limit reservation and per-element breaker holds. The reserved budget is **not consumed at admit time**: a Drop without `consume()` releases every slot. `AdmissionTicket::consume()` is called by `ResilientCaller` (or the passthrough fast path) when the first byte arrives from the upstream — at that point rate-limit and breaker budgets are deducted. This is the seam that makes passthrough and canonical share admission guarantees without paying twice when passthrough falls through to canonical translation.

**First-byte consume rule**
Budget-deducting gates (rate-limit, breaker) commit on first-byte-from-upstream rather than at admit time or at stream-end. Rationale: gates protect upstreams, not user fairness — a request killed by capability, cost, or auth before any upstream packet is sent should not deduct upstream-protection budget. Cancellations after first byte still count, matching the v0.4 ResilientCaller semantics.

**Byte-identity dialect skip**
The capability gate inside `Admission::admit` is skipped when the inbound frontend dialect equals the chain head's provider native dialect (today: Anthropic Messages → Anthropic provider; OpenAI Responses → openai_compatible Responses-shape upstream). The assumption is that a provider speaking its native dialect is a capability superset of that dialect's frontend. Holds today; documented assumption to revisit if e.g. Anthropic Messages frontend grows a feature the Anthropic provider doesn't yet implement. Distinct from the existing **capability gate** entry above, which describes the v0.3 check; this is the v0.8 predicate that gates *whether* that check runs.

## Glossary maintenance

When introducing a new domain concept, add it here in the same paragraph it gets named. Don't rename existing terms without a search-and-replace across the codebase.
