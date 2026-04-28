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

1. Create `crates/providers/src/<name>.rs` (or a sub-directory for larger providers).
2. Implement the `Provider` trait (`call_stream` / `call_unary`).
3. Add a config variant in `crates/config/src/lib.rs`.
4. Register the provider in `crates/gateway/src/` provider wiring.
5. Add integration tests using `mockito` to simulate the upstream API.

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
