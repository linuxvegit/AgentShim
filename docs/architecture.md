# Architecture

## Crate Graph

```
agent-shim (gateway binary)
├── agent-shim-core        — canonical types (StreamEvent, CanonicalRequest, …)
├── agent-shim-config      — YAML + env configuration
├── agent-shim-observability — tracing / metrics setup
├── agent-shim-frontends   — request decoding + response encoding per API dialect
│   ├── anthropic_messages
│   └── openai_chat
├── agent-shim-providers   — upstream HTTP clients
│   ├── openai_compatible  — generic OpenAI-compatible client
│   └── github_copilot     — Copilot token exchange + relay
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

## Boundary Rule

Frontends and providers **must not** import each other.
Both depend only on `agent-shim-core`.
The gateway crate is the only place that wires them together.
