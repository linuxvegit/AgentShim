# ADR-0004: ResilientCaller orchestrator (v0.4)

**Status:** Accepted (2026-05-08)
**Phase:** 4 (resilient gateway)
**Source:** [Phase 4 design spec §3 + §5](../superpowers/specs/2026-05-08-phase-4-resiliency-design.md).

## Context

Phase 4 ships four resilience subsystems — fallback chains, retries,
circuit breakers, rate limiting — that interact in a non-trivial order:
rate limits gate before any upstream call; breakers gate per chain
element; retries operate within an upstream; fallback fires on retry
exhaustion. The interactions are tight enough that the layering order
itself is a load-bearing part of the design.

The pipeline already calls `provider.complete()` exactly once per
request. We need a single point where the four subsystems compose,
without scattering layering decisions across `pipeline.rs`,
`router/`, and `gateway/handlers/`.

## Decision

Introduce a `ResilientCaller` struct in `crates/router/` that wraps the
provider lookup, breaker registry, and limiter registry. The pipeline
calls `resilient_caller.complete(chain, req, ...)` instead of
`provider.complete(req, target)` directly. The orchestrator owns the
layering order documented in §5.2 of the design spec.

## Alternatives considered

### A12: Middleware-per-subsystem (rejected)

Each subsystem ships as an axum middleware layer. The pipeline becomes
`auth → rate_limit → breaker → retry → fallback → provider`.

Rejected because:

- The order is not actually a layered pipeline. Per-upstream rate
  limits and breakers must run *inside* the chain walk, not before it.
  Middleware can only decorate a single inner call.
- Sharing state (the per-upstream breaker decision needs to feed into
  retry-budget accounting) requires reaching across middleware layers,
  which defeats the layering's only benefit.
- Cancellation correctness becomes harder — middleware unwinds in
  reverse order, but our breaker state must commit only on the inner
  call's resolution, not on layer exit.

### A13: Inline-in-pipeline (rejected)

Embed the four subsystems' calls directly in `pipeline.rs` request
handlers.

Rejected because:

- `pipeline.rs` would grow past ~1,200 lines, well beyond the file-size
  guidance in the project style guide.
- The same logic would be duplicated across each frontend's handler
  (Anthropic Messages, OpenAI Chat, OpenAI Responses).
- Unit testing requires standing up the full handler stack, instead of
  testing the orchestrator in isolation with mock providers.

## Consequences

**Positive:**

- One place to read the layering decision; one place to test it; one
  place to evolve it (e.g., when distributed state lands in v0.5).
- Subsystem code (`retry.rs`, `circuit_breaker.rs`, `rate_limit.rs`,
  `auth.rs`) stays focused on its own concern; the orchestrator owns
  composition.
- Property tests on the layering live in `crates/router/tests/` and
  use mock providers — no axum machinery required.

**Negative:**

- `ResilientCaller::complete` has 7 parameters. Bundling them into a
  `ResilientCallContext` struct is a follow-up if signatures grow further.
- Operators reading code for the first time need to know the orchestrator
  exists. README + `docs/architecture.md` link to it.

**Frozen-core invariant resumes.** ADR-0003 was the bounded one-time
exception. Phase 4 plan files declare `core changes: NONE`.

## Reversibility

The orchestrator is internal — operators interact only with config
files and HTTP responses. A future refactor (e.g., splitting the
orchestrator into separate fallback-walker and retry-runner traits)
is purely internal and does not break any external interface.
