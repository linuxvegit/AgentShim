# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
cargo build --workspace                    # Build all crates
cargo build --release -p agent-shim        # Release binary

cargo fmt --all -- --check                 # Check formatting
cargo clippy --workspace --all-targets     # Lint (treat warnings as errors in CI)

cargo nextest run --workspace              # Run all tests (preferred)
cargo nextest run -p agent-shim-frontends  # Single crate
cargo nextest run cancellation_fuzz_anthropic  # Single test by name
cargo test --workspace                     # Fallback if nextest unavailable

cargo deny check                           # License & advisory check
```

Run the server:
```bash
agent-shim serve --config config/gateway.yaml
agent-shim validate-config --config gateway.yaml
```

## Architecture

AgentShim is a protocol-translating API gateway that lets AI agents (Claude Code, Cursor, etc.) talk to any LLM backend through their native protocol. Agents send Anthropic or OpenAI requests; the gateway translates to/from a canonical internal model, routes to the configured backend, and streams back in the agent's expected format.

### Request Flow

```
Agent HTTP Request → FrontendProtocol::decode_request → CanonicalRequest
  → Router::resolve(frontend, model) → BackendTarget
  → BackendProvider::complete(req, target) → CanonicalStream
  → FrontendProtocol::encode_stream → SSE HTTP Response
```

### Crate Dependency Layers

```
gateway (binary: axum server, CLI, handlers)
├── frontends (Anthropic Messages + OpenAI Chat adapters)
├── providers (OpenAI-compatible + GitHub Copilot clients)
├── router (model alias → backend resolution)
├── observability (tracing, request IDs, header redaction)
├── config (YAML schema, env overlay, secrets, validation)
└── core (canonical data model, zero I/O, #![forbid(unsafe_code)])
```

### Boundary Rule

Frontends and providers must never import each other. Both depend only on `core`. The gateway binary is the sole wiring point. Translation happens only at the two edges.

### Key Traits

- **`FrontendProtocol`** (frontends): `decode_request`, `encode_unary`, `encode_stream`
- **`BackendProvider`** (providers): `async fn complete() → CanonicalStream`

### Canonical Model (core)

`CanonicalRequest` / `CanonicalStream` / `StreamEvent` / `CanonicalResponse` — protocol-neutral types that both edges translate to/from. `StreamEvent` is a tagged enum: `ResponseStart`, `TextDelta`, `ToolCallArgumentsDelta`, `UsageDelta`, `MessageStop`, etc.

### Streaming

Encoding is fully lazy — `encode_stream` returns a `BoxStream<Bytes>` that pulls from upstream on demand. Dropping the output stream propagates backpressure to the provider connection. SSE keepalive is configurable (default 15s).

## Configuration

YAML config with env variable overlay using `AGENT_SHIM__` prefix (double underscores for nesting). All config structs use `deny_unknown_fields` — typos fail at startup. API keys use `Secret<String>` newtype that prevents debug/log output.

## Code Conventions

- `thiserror` in library crates, `anyhow` only at binary boundary
- `#![forbid(unsafe_code)]` in all crates except gateway
- `async-trait` only where dyn dispatch needed
- `serde_json::RawValue` for byte-accurate tool call argument passthrough
- Max line width 100 (`rustfmt.toml`)
- Property-based tests with `proptest`, mock server tests with `mockito`
