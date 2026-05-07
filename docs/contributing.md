# Contributing

## Toolchain

The required Rust toolchain version is pinned in `rust-toolchain.toml`.
Running `cargo build` inside the repo will automatically install it via rustup.

Additional tools used in CI:

```bash
cargo install cargo-nextest   # faster test runner
cargo install cargo-deny      # licence + advisory checks
```

## How to Add a Frontend

A frontend translates between a wire protocol (e.g. Anthropic Messages API)
and the internal canonical types.

1. Create `crates/frontends/src/<name>/` with sub-modules: `decode`, `encode_stream`, `encode_unary`, `mapping`, `wire`, `mod`.
2. Implement `FrontendProtocol` for your struct.
3. Re-export from `crates/frontends/src/lib.rs`.
4. Add a `FrontendKind` variant in `crates/core/src/target.rs`.
5. Wire the new variant in the gateway router.
6. Add fixture-based integration tests in `crates/protocol-tests/tests/`.

## How to Add a Provider

A provider calls an upstream inference API and yields a `CanonicalStream`.

1. Create `crates/providers/src/<name>/` (or a single `<name>.rs` for tiny providers).
2. Implement `BackendProvider` (`name`, `capabilities`, `complete`, optionally `proxy_raw` and `list_models`).
3. Add a config variant in `crates/config/src/lib.rs` and a `from_config(name, &<Name>Upstream)` factory.
4. Wire the new variant into `crates/gateway/src/state.rs::AppState::new`.
5. Set `capabilities.vision` correctly. Vision-capable providers route through the canonical `ContentBlock::Image` → wire encoder; text-only providers should leave it `false` so the gateway capability gate rejects image requests with HTTP 400 BEFORE any upstream call.
6. If the provider has fields the canonical model doesn't carry (per ADR-0002 frozen-core), put them on the relevant content block's `extensions` map under a per-provider prefix: `<provider_name>.<field>`. Examples:
    * `gemini.safety_ratings`
    * `gemini.citation_metadata`
    Do NOT add new variants to the canonical content/event enums.
7. Use `http_client::build(read_timeout)` (in `crates/providers/src/http_client.rs`) for the HTTP client. It applies the shared TLS config, connect timeout, and read-gap timeout that every other provider uses.
8. Add integration tests under `crates/providers/tests/<name>_smoke.rs` using `mockito`. Cover at minimum: text streaming, tool-call streaming, and one cross-protocol flow (e.g. an Anthropic-shaped CanonicalRequest routed through the new provider).

## Capability flags

`ProviderCapabilities` is the gateway's contract for what a provider can do.
The fields are checked at the gateway boundary (today: `vision`) or used
informationally (`streaming`, `tool_use`, `json_mode`). Setting these
honestly matters — `vision: true` against a text-only model means an
image-bearing request will fail at the upstream rather than in the
gateway, which is a worse user experience than the gate's HTTP 400.

## Fixture regeneration

Provider response fixtures under `crates/protocol-tests/fixtures/<provider>/`
are hand-crafted captures of real upstream responses. When a provider
ships a wire-format change (new event variant, renamed usage field, etc.)
you'll need to refresh the captures.

```bash
# Refresh all providers (requires API keys)
ANTHROPIC_API_KEY=… DEEPSEEK_API_KEY=… GEMINI_API_KEY=… \
    scripts/regen-fixtures.sh

# Or one provider at a time
ANTHROPIC_API_KEY=… scripts/regen-fixtures.sh anthropic
```

After regenerating, run `cargo nextest run --workspace` to confirm the
new fixtures still pass through the gateway's encode/decode paths.

The script is a developer tool, NOT a CI step. CI uses the committed
fixtures and never calls billed APIs.

## Frozen-core policy (ADR-0002)

The canonical types in `agent-shim-core` are frozen against
provider-specific additions. Concretely:

* DO put new fields on the `extensions: ExtensionMap` of the relevant
  content block (or `Usage`, or `StreamEvent`'s payload).
* DO NOT add new variants to `ContentBlock`, `StopReason`,
  `StreamEvent`, etc.
* Namespace extension keys with `<provider>.` to prevent collisions.
* If you genuinely need a new canonical concept (e.g. a new
  cross-provider modality), open an ADR.

This keeps the cross-protocol seam stable: every frontend can decode
every provider's canonical events without knowing which provider
emitted them.

## Test Commands

```bash
# All tests (recommended)
cargo nextest run --workspace

# Single crate
cargo nextest run -p agent-shim-frontends

# Standard cargo test (no nextest)
cargo test --workspace

# Specific test
cargo test cancellation_fuzz_anthropic
```

## Style

- `rustfmt` is enforced in CI — run `cargo fmt --all` before committing.
- `clippy -D warnings` is enforced — run `cargo clippy --workspace --all-targets`.
- Prefer immutable data: avoid `mut` where a `let` binding suffices.
- Functions should be short (<50 lines) and focused.
- All public items should have a doc comment.
