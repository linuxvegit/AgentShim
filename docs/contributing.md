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

## Promoting an extension key to a typed canonical field

The frozen-core policy is permissive on purpose: it lets a provider
ship a new field on day one without forcing every other provider, every
frontend, and every consumer to rev. The expected lifecycle is:

1. **Land as an extension.** New provider-specific data goes onto
   `extensions["<provider>.<key>"]` with whatever JSON shape the wire
   uses. ADR-0002 codifies this as the default.
2. **Wait for cross-provider read patterns.** As long as exactly one
   provider writes the key and exactly one frontend reads it, it stays
   an extension forever. Promotion is justified only when multiple
   frontends (or future providers) end up reading the same key with
   the same shape.
3. **Promote via ADR.** When the read pattern crystallizes, write a
   numbered ADR (next number after the most recent one). ADR-0003 is
   the canonical template — see
   [`docs/adr/0003-promote-safety-ratings.md`](adr/0003-promote-safety-ratings.md).
   The ADR records the typed shape, why open enums (carrying
   `Other(String)`) over closed enums where appropriate, and what
   pieces of the legacy wire shape do NOT make the cut.
4. **Run a one-minor double-write window.** The minor that introduces
   the typed field also keeps writing the legacy extension key, with
   `// REMOVE in v<NEXT>` markers at the double-write call sites. The
   next minor drops the extension write. Consumers get exactly one
   minor to migrate; the migration is a one-line change (replace
   `extensions.get("foo.bar")` with `usage.bar.as_ref()`).
5. **Confirm `core changes: NONE` returns.** The promotion is a
   one-time exception to the freeze. Plan files after the promotion
   should again declare `core changes: NONE` — the canonical types
   stay closed against per-provider growth.

The ADR-0002 → ADR-0003 sequence (`gemini.safety_ratings` →
`Usage.safety_ratings`) is the worked example. Future promotions
should follow the same shape: ADR first, typed shape with open enums
where the wire is open, double-write window, removal in the next
minor.

## How to add a resilience subsystem

Phase 4 introduced four subsystems (fallback, retry, circuit breaker,
rate limit) that share a common shape. If a future feature needs the
same shape (e.g., distributed state in v0.5, or cost-aware routing),
follow this pattern:

1. **New module under `crates/router/src/<subsystem>.rs`.** Pure logic
   first — state machine, math, classification — with property tests
   that don't touch the rest of the gateway. Unit tests in-module.
2. **`Registry` newtype.** A `Registry` struct holds the subsystem's
   per-key state (e.g., `BreakerRegistry`, `LimiterRegistry`). Use
   `RwLock<HashMap<K, V>>` for state that grows on first-touch and
   updates on contention; use atomic primitives or lock-free crates
   (`governor`) for hot-path-only paths.
3. **Wire into `ResilientCaller`.** Add an `Arc<NewRegistry>` field
   and a check call at the right point in the layering — see §5.2 of
   the Phase 4 design spec. Update the [tracing event taxonomy](
   ../crates/router/src/resilient_caller.rs) module-level doc comment
   with the new event names + field set.
4. **Config schema.** Add the subsystem's config block to
   `crates/config/src/schema.rs` (top-level if cross-cutting; on
   `RouteEntry` if per-route). Add validation rules to
   `crates/config/src/validation.rs`. Defaults should reproduce
   pre-feature behavior — operators opt in explicitly. Validate
   plaintext-vs-hash, range bounds, and cross-references at startup.
5. **Tests in three layers** mirroring §8 of the design spec:
   - Pure unit tests in-module (state machine + math).
   - Subsystem-composition tests in `crates/router/tests/`
     (against mock providers, not real HTTP).
   - End-to-end smokes in `crates/gateway/tests/` (`mockito` for HTTP).
6. **Operator docs.** A `## <Subsystem>` subsection in
   `docs/resilience.md`; an entry in the operator log line reference;
   a config example. If the subsystem changes the error envelope
   shape, update `docs/providers/*.md` Resilience-behavior tables.
7. **ADR.** If the layering choice is non-obvious, write an ADR.
   Otherwise, the ADR can wait until a competing approach surfaces.

The pattern is intentionally light — none of the four Phase 4
subsystems used all seven steps, but each used at least four.

## How to add a metric (v0.5)

1. Pick a name following Prometheus conventions: `agent_shim_<noun>_total`
   for counters, `_seconds` for time histograms, `_bytes` for byte
   histograms.
2. Add a `pub const` in `crates/observability/src/metrics/names.rs`.
3. Register a description in `describe_metrics()` in
   `crates/observability/src/metrics/mod.rs`.
4. Add a typed wrapper in `recorders.rs` if the call site is
   gateway/observability; add a `pub(crate) const` in
   `crates/router/src/lib.rs::metric_names` if the call site is the
   router crate (which can't depend on observability).
5. Emit via `metrics::counter!()`/`histogram!()` macros at the
   instrumentation point. Keep label cardinality bounded — never use
   `request_id`, `api_key_hash`, or anything user-supplied as a label.
6. Update the name-parity test in
   `crates/router/tests/metric_names_match_observability.rs` if you
   added a router-side const.

## How to add a span attribute (v0.5)

1. Pick a name following OTel semantic conventions where applicable
   (`http.*`, `agent_shim.*`). Use `agent_shim.*` for AgentShim-
   specific fields.
2. Add the field to the relevant `span!` macro invocation. Use
   `tracing::field::Empty` if the value is set later via `record(...)`.
3. If the field needs to land on the `gateway.request` root span, the
   value must be derivable at the top of `pipeline::dispatch` (or
   recorded after derivation via `root_span.record(field, value)`).
4. Update the in-memory exporter test in
   `crates/router/tests/otel_spans.rs` to assert the field's presence.

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
