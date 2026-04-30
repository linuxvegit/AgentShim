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

## Glossary maintenance

When introducing a new domain concept, add it here in the same paragraph it gets named. Don't rename existing terms without a search-and-replace across the codebase.
