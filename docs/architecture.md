# Architecture

## Crate Graph

```
agent-shim (gateway binary)
├── agent-shim-core        — canonical types (StreamEvent, CanonicalRequest, …)
├── agent-shim-config      — YAML + env configuration
├── agent-shim-observability — tracing / metrics setup
├── agent-shim-frontends   — request decoding + response encoding per API dialect
│   ├── anthropic_messages
│   ├── openai_chat
│   └── openai_responses
├── agent-shim-providers   — upstream HTTP clients
│   ├── oai_chat_wire      — shared OpenAI Chat encode/decode lib
│   ├── openai_compatible  — generic OpenAI-compatible client
│   ├── github_copilot     — Copilot token exchange + relay
│   ├── anthropic          — direct api.anthropic.com client (passthrough + canonical)
│   ├── deepseek           — native api.deepseek.com client (reasoning_content interleave)
│   └── gemini             — Generate Content API client (JSON-array streaming)
├── agent-shim-router      — route table: match frontend request → provider
└── agent-shim-protocol-tests — integration & fuzz tests (dev only)
```

## Request Lifecycle

```
Client HTTP request
  │
  ▼
axum router  ──►  FrontendProtocol::decode_request
                        │
                        ▼
                  CanonicalRequest
                        │
                        ▼
              Router::resolve  →  ProviderConfig
                        │
                        ▼
              Capability gate (image-vs-vision; aborts before upstream)
                        │
                        ▼
              Provider::call_stream / call_unary
                        │
                        ▼
                  CanonicalStream  (StreamEvent)
                        │
                        ▼
              FrontendProtocol::encode_stream / encode_unary
                        │
                        ▼
              SSE / JSON HTTP response to client
```

## Canonical Model

All internal data flows through types defined in `agent-shim-core`:

| Type | Purpose |
|------|---------|
| `CanonicalRequest` | Normalised inference request (messages, tools, params) |
| `CanonicalStream` | `Pin<Box<dyn Stream<Item=Result<StreamEvent,…>>>>` |
| `StreamEvent` | Tagged union of all streaming lifecycle events |
| `CanonicalResponse` | Completed non-streaming response |
| `StopReason` | Normalised stop cause across providers |

## Streaming Pipeline

Encoding is fully lazy: `encode_stream` returns a `FrontendResponse::Stream` whose
inner `BoxStream<Bytes>` pulls from the upstream `CanonicalStream` on demand.
Dropping the output stream propagates backpressure to the provider connection.

The Gemini provider is special: its streaming endpoint emits a **JSON array**
of response objects rather than SSE events. The provider ships a custom
byte-level scanner (`crates/providers/src/gemini/stream.rs`) that splits the
incoming array into per-element JSON objects, handling chunk boundaries at
any byte position (mid-string, mid-escape, between objects). The scanner is
exhaustively chunk-fuzzed.

## Boundary Rule

Frontends and providers **must not** import each other.
Both depend only on `agent-shim-core`.
The gateway crate is the only place that wires them together.

The shared `oai_chat_wire` lib lives inside `agent-shim-providers` so the
DeepSeek native provider, OpenAI-compatible provider, and GitHub Copilot
provider can all reuse the canonical→Chat-Completions encoder + parser
without circular imports. It exposes `canonical_to_chat::build`,
`chat_sse_parser`, and `chat_unary_parser`.

## Capability gate

Plan 04 added a capability gate at the gateway boundary. It runs after route
resolution and policy resolve, but BEFORE the provider's `complete()` call:

```rust
fn check_capabilities(req: &CanonicalRequest, caps: &ProviderCapabilities)
    -> Result<(), ProviderError>
{
    if request_has_image(req) && !caps.vision {
        return Err(ProviderError::CapabilityMismatch(
            "target provider does not support vision (image blocks present in request)".into(),
        ));
    }
    Ok(())
}
```

When the gate fires, no network call to the upstream is made. The error
surfaces back through `HandlerError::CapabilityMismatch { kind, message }`
and is rendered into the inbound frontend's error envelope shape:

* Anthropic Messages → `{"type":"error","error":{"type":"invalid_request_error","message":"…"}}`
* OpenAI Chat / Responses → `{"error":{"message":"…","type":"invalid_request_error","code":"capability_mismatch"}}`

Both with HTTP 400. See `crates/gateway/src/pipeline.rs::check_capabilities`.

## Extension namespaces

Per ADR-0002 (frozen-core), the canonical model never grows new variants for
provider-specific fields. Provider-specific data lands on the relevant
content block's `extensions` map under a per-provider prefix:

| Wire field | Origin provider | Extension key |
|---|---|---|
| `cache_control` | Anthropic | `cache_control` |
| `cache_creation_input_tokens` | Anthropic | (Usage struct field) |
| `prompt_cache_hit_tokens` | DeepSeek | (Usage struct field) |
| `safetyRatings` | Gemini | `gemini.safety_ratings` |
| `citationMetadata` | Gemini | `gemini.citation_metadata` |

When adding a new provider, namespace any new extension keys with
`<provider_name>.` so they don't collide.

## The hybrid Anthropic path

The Anthropic provider has TWO outbound paths:

1. **Passthrough (byte-identity)**: when the inbound frontend is also
   Anthropic Messages, the request body is forwarded to api.anthropic.com
   verbatim and the SSE response is streamed back unchanged. This is the
   only path that round-trips lossless features like `cache_control`
   markers, signature deltas on thinking blocks, and Anthropic-only
   metadata fields.
2. **Canonical translation**: when the inbound frontend is OpenAI Chat (or
   any non-Anthropic dialect), the request is encoded by the
   `anthropic::request::build` module and the response is parsed by
   `anthropic::response`. This path is lossy for Anthropic-only features
   (signatures, redacted thinking) but supports vision, tools, and
   reasoning round-trip.

The pipeline picks between them via `BackendProvider::proxy_raw` returning
`Ok(Some(...))` for the passthrough case and `Ok(None)` for the canonical
case.
