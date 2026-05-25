# Architecture Deepening Opportunities

Generated from an `improve-codebase-architecture` scan on 2026-05-15.

This is a candidate list, not an implementation plan. The goal is to identify
Modules whose Interfaces can become deeper: more behavior behind a smaller
Interface, with better Leverage for callers and better Locality for future
changes.

## 1. Resolved Route Module

**Files**

- `crates/router/src/lib.rs`
- `crates/router/src/static_routes.rs`
- `crates/gateway/src/pipeline.rs`

**Problem**

The Router Interface is still shallow. `Router::resolve` returns the fallback
chain, but callers must separately recover retry policy, breaker policy, route
entry, route label, wildcard semantics, and cost-filter inputs. The gateway
pipeline mirrors `StaticRouter`'s lookup in `find_route_entry`, so route
resolution knowledge is split across the router and gateway.

Deletion test: deleting `find_route_entry` would not remove complexity; it
would force cost-filter route lookup details back into each caller that needs
route-scoped policy data. That means the missing Module is real.

**Solution**

Deepen route resolution around a resolved Route value. That value should carry
the chain plus the route-scoped policy/config facts the pipeline needs after
resolution.

**Benefits**

Locality improves because wildcard/specific matching, route labels, route
policy lookup, and cost-filter route entry selection live in one place.
Leverage improves because tests can assert route resolution as a single
Interface instead of stitching together chain resolution plus side lookups.

## 2. Canonical Stream Lifecycle Module

**Files**

- `crates/frontends/src/openai_responses/encode_stream.rs`
- `crates/frontends/src/anthropic_messages/encode_stream.rs`
- `crates/gateway/src/handlers/mod.rs`
- `crates/providers/src/gemini/response.rs`
- `crates/providers/src/openai_compatible/responses_api/parse_stream.rs`
- `crates/providers/src/anthropic/response.rs`
- `crates/providers/src/deepseek/response.rs`

**Problem**

The canonical stream lifecycle is encoded in many local state machines. Each
Module needs to know ordering rules for `ResponseStart`, `MessageStart`,
content block start/stop, text/tool/reasoning deltas, `MessageStop`,
`UsageDelta`, and `ResponseStop`. Prior fixes in this repository clustered
around exactly these facts: OpenAI Responses output-index mapping,
`ResponseStop` usage preservation, Anthropic tool-call start ordering, and
mixed text plus tool-result handling.

Deletion test: deleting any one stream state machine does not delete the
ordering complexity; it reappears in another Frontend or Provider. The current
Interfaces are therefore shallow around lifecycle invariants.

**Solution**

Concentrate lifecycle accumulation and invariant checking behind one deeper
Module, or at minimum behind a shared protocol-test harness that every
Frontend/Provider stream encoder/parser must pass. Avoid moving this into
`agent-shim-core` unless a future ADR explicitly lifts the frozen-core
discipline for that shape.

**Benefits**

Locality improves because stream-ordering bugs have one main home. Leverage
improves because new Frontends and Providers can reuse the same lifecycle
checks instead of re-learning event ordering. Tests improve by asserting
canonical event validity once and applying the same fixtures across dialects.

## 3. Raw Passthrough Admission Module

**Files**

- `crates/gateway/src/pipeline.rs`
- `crates/providers/src/lib.rs`
- `docs/adr/0001-anthropic-hybrid-path.md`

**Problem**

`BackendProvider::proxy_raw` is an important Adapter for byte-for-byte
passthrough, especially for Anthropic Messages and OpenAI Responses. But the
pipeline documents a known gap: successful raw passthrough skips the
`ResilientCaller` path, so rate-limit and breaker gates do not run in the same
way as the canonical path. This makes the admission Interface shallow: callers
must know which path commits upstream resources before which gates.

Deletion test: deleting the raw-passthrough branch would remove the gap, but
it would also lose the core Leverage of the hybrid path. The right fix is not
to delete passthrough; it is to deepen admission around it.

**Solution**

Introduce a raw/canonical admission Module that owns the ordering model for
auth, route resolution, plugin hooks, capability checks, rate-limit decisions,
cost decisions, passthrough eligibility, and the final provider call. A
ticket-like Interface could let a gate reserve or validate without consuming
twice when passthrough falls back to canonical translation.

**Benefits**

Locality improves because "what must happen before any upstream call" becomes
one decision point. Leverage improves because passthrough can preserve its
lossless behavior while sharing admission guarantees with the canonical path.
Tests can focus on the raw/canonical equivalence surface instead of duplicating
full gateway scenarios.

## 4. Plugin Hook Runner Module

**Files**

- `crates/plugins/src/registry.rs`
- `crates/plugins/src/invoke.rs`
- `crates/plugins/src/trait_def.rs`
- `docs/adr/0008-plugin-system.md`

**Problem**

`PluginRegistry` already has the right external shape, but the hook execution
Implementation repeats several facts across H2, H3, H5, and H7: plan lookup,
enabled filtering, timeout selection, `on_error`, protected-field checking,
stream error behavior, and response-complete spawning. The Interface to each
hook method is useful, but the shared invocation semantics are not yet deep
enough.

Deletion test: deleting one hook method would not remove timeout/error-policy
complexity; it would reappear in the next hook. That is a signal for an
internal seam, not a new public Interface.

**Solution**

Deepen an internal hook runner Module inside `PluginRegistry`. Hook-specific
methods should describe only their differences: input/output shape and hook
position. The runner should own common invocation semantics.

**Benefits**

Locality improves because plugin safety rules live in one implementation.
Leverage improves when built-in plugins land: adding `prompt_compressor`,
`pii_scrubber`, and `usage_recorder` should exercise the same runner rather
than new one-off hook code. Tests can pin hook execution policy once and cover
hook-specific differences separately.

## 5. Capability Language Cleanup

**Files**

- `crates/core/src/capabilities.rs`
- `crates/providers/src/lib.rs`
- `crates/gateway/src/pipeline.rs`
- `docs/architecture.md`

**Problem**

There are two `ProviderCapabilities` Modules with different fields and naming:
one in `agent-shim-core`, and one in `agent-shim-providers`. The live gateway
capability gate uses the providers version, while core also exports a richer
capability type. That makes the capability Interface ambiguous for future
contributors.

Deletion test: deleting either type today would expose unclear ownership.
Keeping both without a clear seam makes future capability additions more
error-prone.

**Solution**

Clarify which Module owns provider capability language. Options include
consolidating on one type, adding an explicit conversion Adapter, or
deprecating the unused public type over time. Any change to the public core
type must respect ADR-0007's frozen-core lift discipline.

**Benefits**

Locality improves because capability checks and provider declarations share
one vocabulary. Leverage improves when adding audio, file upload, system
prompt, JSON mode, reasoning, or tool-related capability checks. Tests can
target one capability Interface instead of deciding which type is authoritative.

