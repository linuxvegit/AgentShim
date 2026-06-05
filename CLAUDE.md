# CLAUDE.md

Guidance for Claude Code working in this repository. Keep this file short — additions land only when they prevent a real mistake.

## Build & Test

```bash
cargo build --workspace
cargo build --release -p agent-shim

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings   # CI treats warnings as errors

cargo nextest run --workspace          # preferred
cargo nextest run -p agent-shim-frontends    # single crate
cargo nextest run cancellation_fuzz_anthropic   # single test
cargo test --workspace                 # fallback

cargo deny check                       # licenses + advisories
```

**Windows binary-locked rebuild.** If `cargo build --release` fails with `Access is denied (os error 5)`, a running `agent-shim.exe` is holding the file. Rename the locked exe (Windows allows rename of a running binary) and rebuild — it auto-cleans next time the holder exits:

```powershell
Rename-Item target\release\agent-shim.exe target\release\agent-shim.exe.OLD-by-<PID>
cargo build --release -p agent-shim
```

CLI surface: `serve`, `validate-config`, `show-catalog [--format json|table] [--strict]`, `models <name> [--format json|table]`, `copilot login|models [--format json|table]`, `service install|...` (Windows).

## Architecture

Protocol-translating gateway. Agents send Anthropic Messages, OpenAI Chat, or OpenAI Responses; the gateway decodes to a canonical model, routes per `(frontend, model)`, re-encodes to the upstream's native shape.

```
HTTP req → FrontendProtocol::decode_request → CanonicalRequest
       → RoutePolicy::resolve → ResolvedPolicy (effort + mapping + headers)
       → Router::resolve → BackendTarget chain
       → BackendProvider::complete → CanonicalStream
       → FrontendProtocol::encode_stream → SSE response
```

```
gateway (binary, pipeline, handlers, admin)
├── frontends   — anthropic_messages, openai_chat, openai_responses
├── providers   — openai_compat, github_copilot, anthropic, deepseek, gemini
├── router      — alias resolution, ModelIndex, ResilientCaller, cost filter
├── plugins     — H2/H3/H5/H7 hook registry + builtin
├── observability(+derive) — tracing/OTel/Prometheus, file logging, `#[derive(Metric)]`
├── tokens      — cl100k for cost + compressor
├── config, protocol-tests
└── core        — canonical model, ResolvedPolicy, ModelMetadata, #![forbid(unsafe_code)]
```

**Boundary:** frontends and providers never import each other. Both depend only on `core`. The gateway binary is the sole wiring point. Translation happens only at the two edges.

**Key traits:**
- `FrontendProtocol` — `decode_request`, `encode_unary`, `encode_stream`, `kind()`.
- `BackendProvider` — `complete()`, `list_models() -> Option<BTreeMap<String, ModelMetadata>>` (None = no catalog), `proxy_raw()` optional for byte-passthrough.

**Canonical model (frozen-core):** providers read `CanonicalRequest::resolved_policy`, never raw inbound headers. `ReasoningEffort` is `Minimal|Low|Medium|High|Xhigh|Max`; encoders compress per-provider (`xhigh`/`max` → `"high"` for OpenAI Chat/Responses; Copilot passes through via `ProviderCapabilities::accepts_xhigh`; Anthropic emits `output_config.effort` + adaptive thinking under `anthropic-beta: effort-2025-11-24`).

**Streaming:** `encode_stream` returns `BoxStream<Bytes>` that pulls lazily; dropping it backpressures the provider. SSE keepalive default 15s.

## Configuration

YAML with `AGENT_SHIM__`-prefixed env overlay (double underscores nest). Every struct uses `#[serde(deny_unknown_fields)]` — typos fail at startup. API keys: `Secret<String>` newtype (no Debug/Display leakage).

Per-route knobs: `reasoning_effort` (default), `reasoning_mapping: [{ match, set }, …]` (first-match rewrite), `anthropic_beta` (default header), `plugins: { h2: [...], … }`.

Validation/logging toggles: `validation.strict_upstream_models` (hard-fail typos), `logging.print_catalog_on_start` (dump catalog to stderr after discovery).

## Conventions

- `thiserror` in libs; `anyhow` only at the binary boundary.
- `#![forbid(unsafe_code)]` everywhere except `gateway`.
- `async-trait` only where dyn dispatch is needed.
- `serde_json::RawValue` for byte-accurate tool-call args.
- New `serde` fields additive: `#[serde(default, skip_serializing_if = "Option::is_none")]` keeps wire compatibility.
- Max line width 100 (`rustfmt.toml`).
- Property tests `proptest`; mock HTTP `mockito`.
- Workspace version at `workspace.package.version`; bump once per release, crates inherit via `version.workspace = true`.

## Hard-won Traps

- **Empty `Vec<Box<dyn Layer<S>>>` is deny-all.** In `tracing_subscriber` 0.3, an empty Vec wrapped via `.with(...)` short-circuits the whole subscriber. When optional layers may be absent, wrap as `Option<Vec<_>>`; `None` is a true no-op. Caused stdout silence in v0.7 → fixed in v0.8 `tracing_setup::init`.
- **`let _ = try_init()` is a debugging black hole.** Surface the error (`eprintln!` at startup; `tracing::warn!` later).
- **`proxy_raw` log emission belongs in the success arm.** Per-request `→` log inside `Ok(Some(_))` only — otherwise non-applicable frontends double-log via the canonical fall-through.
- **One catalog read site.** `ModelIndex::metadata(provider, model)` feeds logs, `/v1/models`, `/admin/catalog`, `show-catalog`, and `copilot models`. Populate once via `ModelIndex::with_metadata` at startup; don't re-query upstream per request.
