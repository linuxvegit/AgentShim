# Anthropic provider uses a hybrid passthrough+canonical path

**Status:** accepted (2026-04-30)

The Anthropic-as-backend provider has two code paths inside one `BackendProvider::complete()`. When `req.frontend.kind == FrontendKind::AnthropicMessages`, we proxy the raw inbound bytes through `BackendProvider::proxy_raw` — round-trip is byte-for-byte lossless on Anthropic-only features (`cache_control`, `thinking`, server tools, beta headers). For any other frontend (`openai_chat`, `openai_responses`), we take the canonical path: encode `CanonicalRequest` → Anthropic Messages JSON, parse Anthropic SSE → `CanonicalStream`.

We rejected pure passthrough because it would make OpenAI→Anthropic one-way (the OpenAI frontend produces a `CanonicalRequest`, not Anthropic bytes). We rejected pure canonical because it silently drops Anthropic-only features the moment Anthropic ships a new one we haven't plumbed into `extensions`. The hybrid keeps Anthropic→Anthropic lossless without sacrificing cross-protocol.

## Consequences

- The provider carries an architectural invariant: the same prompt routed through both paths must produce semantically equivalent output. Tested by golden fixtures.
- Two test surfaces per Anthropic-feature change (passthrough byte-equality + canonical event-equality).
- Future Anthropic backend variants (Bedrock, Vertex-Anthropic) inherit this shape — they pick a path based on whether the wrapping cloud envelope allows byte-passthrough.
