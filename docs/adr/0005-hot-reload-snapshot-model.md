# ADR-0005 — Hot-Reload Snapshot Model

**Status:** Accepted
**Date:** 2026-05-09
**Phase:** 5 (v0.5.0)
**Source design:** [`docs/superpowers/specs/2026-05-09-phase-5-observability-design.md`](../superpowers/specs/2026-05-09-phase-5-observability-design.md) §5

## Context

v0.5 ships hot-reload of routing & policy config. Three orthogonal
choices pin the design:

1. How is the running state held so a reload can replace it atomically?
2. What boundary separates "policy" (reloadable) from "lifecycle" (not)?
3. How are breaker and rate-limit state-vs-policy resolved across reload?

This ADR records each choice and the alternatives we rejected.

## Decision

### 1. `arc-swap` over `Arc<AppSnapshot>`

The gateway holds an `Arc<ArcSwap<AppSnapshot>>` plus a `#[derive(Clone)]`
`AppCore` (cheap to clone — every field is itself an `Arc`). Each request
reads `state.snapshot.load_full()` once at the top of `pipeline::dispatch`
and uses that `Arc<AppSnapshot>` for the entire request lifetime.
Mid-request reload doesn't reach in-flight streams.

**Rejected: `RwLock<AppSnapshot>`.** Readers contend with writers even
though writes are rare. A long streaming request holds a read guard
across `await`, blocking reload indefinitely.

**Rejected: per-subsystem atomics.** Each of the four resilience
subsystems plus router plus auth would own its own swappable inner
state. A request started mid-reload could see new routes but old
breakers — a transactionality violation that's hard to debug.

### 2. "Policy reloads, lifecycle doesn't"

`AppCore` (immutable for the process lifetime, though `#[derive(Clone)]`
so the reload-applying task can fork a handle alongside the gateway
server) holds: providers (with embedded credentials), `BreakerRegistry`
(state, not policy), the `LimiterRegistry`, the OTel pipeline, the
metrics handle, and the listener bind addresses.

`AppSnapshot` (hot-swappable) holds: routes, retry/breaker/rate-limit
*policies*, fallback chains, auth keys + flags, logging filter.

**Rejected: full reload (everything).** Re-binding the listener mid-
operation drops in-flight connections. Re-initializing the OTel
exporter discards span queues. Re-keying providers requires reissuing
HTTP clients with fresh credentials — possible but a significantly
larger surface to validate. v0.6 may revisit upstream-set reload.

**Rejected: validation-only (no swap).** Useful but trivial — every
operator already knows how to `agent-shim validate-config` from a CI
pre-deploy step.

### 3. Breaker state survives reload; rate-limit buckets deferred to v0.6

`BreakerRegistry` lives in `AppCore`. The map of `(provider, model) →
BreakerState` is unaffected by reload. Only `BreakerPolicy` (thresholds,
windows, cooldowns) lives in `AppSnapshot` and updates on swap.
Rationale: an operator tightening thresholds shouldn't reset the
empirical evidence the breaker has gathered about an unhealthy upstream.
This half of spec §5.4 is fully implemented and covered by
`crates/gateway/tests/reload_in_flight.rs`.

`LimiterRegistry` also lives in `AppCore`. Each bucket is keyed by
`(dimension, identity, route, upstream)` and built from a specific
`BucketConfig` (rate_per_sec, burst). The `governor` crate that backs
the buckets does NOT expose retuning. Because the reload-applying task
only swaps `AppSnapshot`, a v0.5 reload that changes `rate_limit.*` has
**no effect on running buckets** — existing buckets keep enforcing the
old policy until the process restarts. v0.6 will either replace
`governor` with a custom rate-limit implementation that supports
retuning, or implement a token-preservation layer above `governor` so
the reload-applying task can rebuild the affected buckets in place.
This asymmetry is documented in spec §5.4 (with the v0.6 deferral note)
and in the operator guide `docs/observability.md`. Operators needing
immediate effect today must restart.

### 4. SIGHUP + POST /admin/reload, no filesystem watcher

Two reload triggers, one mpsc channel, one applying task. SIGHUP is
Unix-only (`#[cfg(unix)]` listener); `POST /admin/reload` is the cross-
platform path.

**Rejected: filesystem watcher (`notify` crate).** Watchers are fragile
across platforms. macOS bundling, Linux inotify quirks, Windows
ReadDirectoryChangesW — each has its own edge cases. Operators have
better mechanisms (sidecars, kubectl exec, CI scripts) to push reload
events; the gateway shouldn't own that responsibility.

## Consequences

**Positive:**
- Lock-free hot path: `arc-swap` `load_full()` is two atomic ops in the
  steady state.
- Clear mental model: "policy reloads, lifecycle doesn't" is one
  sentence operators internalize.
- No invasive changes to v0.4 code paths — `AppState` retains the same
  external shape; only its internals are split.

**Negative:**
- Rate-limit policy changes are inert until restart in v0.5. Documented
  in `docs/observability.md` and tracked as a v0.6 candidate. Operators
  loosening or tightening limits must restart to see the new policy.
- Reload-validation duplicates startup validation: `validate_for_reload`
  runs rules 11-14 plus calls `validate(...)`. Ergonomic shape;
  mechanical drift risk if a future rule lands only in one of the two
  entry points.

## Alternatives Considered (full list)

- **A1: full reload.** Re-bind everything. Rejected (§2 above).
- **A2: validation-only.** Operator checks YAML, restarts manually.
  Rejected (§2 above).
- **A3: filesystem watcher.** Rejected (§4 above).
- **A4: `RwLock<AppState>`.** Rejected (§1 above).
- **A5: per-subsystem atomics.** Rejected (§1 above).
- **A6: drop existing buckets unconditionally on reload.** Considered
  for v0.5 but deferred to v0.6 alongside the broader rate-limit retune
  effort; see §3.
- **A7: replace `governor` to support retuning.** Out of scope for v0.5;
  reasonable v0.6 task.

## Changelog

This ADR is referenced from `CHANGELOG.md` under `[0.5.0]`.
