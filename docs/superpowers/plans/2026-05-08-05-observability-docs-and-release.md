# Plan 05 — Observability + Docs + v0.4.0 Release (Phase 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-08-phase-4-resiliency-design.md`](../specs/2026-05-08-phase-4-resiliency-design.md) (decision D11; §6.5 log shape; §7 plan decomposition; §12 Definition of Done).

**Goal:** Cap Phase 4 with structured tracing (every retry attempt, fallback transition, and breaker state change emits a `tracing` event with the standard field set from §6.5), an architecture decision record for the `ResilientCaller` orchestrator choice, a full documentation refresh (capability matrices, provider Resilience subsections, contributing guide), a complete CHANGELOG `[0.4.0]` entry, and the workspace version bump `0.4.0-dev → 0.4.0` plus release verification.

**Architecture:** Tracing fields are added at the points where Plans 01–04 already make decisions — the existing `tracing::warn!`/`tracing::info!` calls scattered through `crates/router/src/{retry,resilient_caller,circuit_breaker,rate_limit,auth}.rs` are unified under one stable field set defined in this plan, plus a new per-request summary log emitted at request end. ADR-0004 records §3 / §5 of the design spec (orchestrator choice over middleware-per-subsystem and inline-in-pipeline). The doc refresh is mechanical — capability matrices gain a "Resilience" row in `README.md` and `docs/architecture.md`; each provider doc gains a `## Resilience behavior` subsection that links to `docs/resilience.md` (created in Plan 04 T7).

**Tech stack:** No new dependencies. Uses existing `tracing` crate already in the workspace.

**Frontend changes:** NONE.
**Provider code changes:** NONE (provider docs only).
**Core changes:** NONE.

---

## File Structure

`crates/router/src/`:
- Modify: `retry.rs` — emit `tracing::warn!` per retry attempt with the standard field set.
- Modify: `resilient_caller.rs` — emit `tracing::warn!` per fallback transition; emit per-request summary `tracing::info!` at request end (success path) or `tracing::warn!` (failure path).
- Modify: `circuit_breaker.rs` — emit `tracing::info!` on every state change (Closed → Open, Open → HalfOpen, HalfOpen → Closed, HalfOpen → Open).
- Modify: `rate_limit.rs` — confirm log emission already done in P04 T2 matches the standard field set.

`crates/router/tests/`:
- Create: `tracing_fields.rs` — captures `tracing` events with the `tracing-test` helper and asserts the field set on each event kind.

`docs/adr/`:
- Create: `0004-resilient-caller.md` — records §3 architecture choice; documents alternatives A12 (middleware-per-subsystem) and A13 (inline-in-pipeline) rejected per §10.

`docs/`:
- Modify: `architecture.md` — capability matrix gains "Resilience" row; performance overhead targets paragraph from §8 of the spec.
- Modify: `contributing.md` — "How to add a resilience subsystem" subsection (the pattern the four subsystems share).
- Modify: `providers/anthropic.md` — `## Resilience behavior` subsection.
- Modify: `providers/openai-compatible.md` — `## Resilience behavior` subsection.
- Modify: `providers/gemini.md` — `## Resilience behavior` subsection.
- Modify: `providers/deepseek.md` — `## Resilience behavior` subsection.
- Modify: `providers/github-copilot.md` — `## Resilience behavior` subsection.

Top-level:
- Modify: `README.md` — capability matrix v0.4 row; "What's NOT in v0.4" supersedes v0.3; releases bullet for v0.4.0; full resilience config block in the configuration example.
- Modify: `CHANGELOG.md` — `[0.4.0]` entry above `[0.3.0]`.
- Modify: `Cargo.toml` — workspace version `0.4.0-dev` → `0.4.0`.
- Modify: `Cargo.lock` — updated by `cargo build` (committed alongside Cargo.toml).

---

## Tasks

### Task 1: Standard tracing field set for resilience events

**Files:**
- Modify: `crates/router/src/retry.rs`
- Modify: `crates/router/src/resilient_caller.rs`
- Modify: `crates/router/src/circuit_breaker.rs`
- Modify: `crates/router/src/rate_limit.rs`
- Create: `crates/router/tests/tracing_fields.rs`

- [ ] **Step 1: Define the standard event names + fields**

In `crates/router/src/resilient_caller.rs`, add a top-of-module doc comment that fixes the contract:

```rust
//! # Tracing event taxonomy (Plan 05 P05 T1)
//!
//! Every resilience event uses one of these `target = "agent_shim::resilience"`
//! event names, with the field set documented per kind:
//!
//! | event_name              | level | fields                                                                           |
//! |-------------------------|-------|----------------------------------------------------------------------------------|
//! | retry.attempt           | warn  | request_id, upstream, model, attempt, error_class, backoff_ms, total_elapsed_ms  |
//! | retry.exhausted         | warn  | request_id, upstream, model, attempts, last_error                                |
//! | fallback.transition     | warn  | request_id, from_upstream, to_upstream, reason                                   |
//! | breaker.state_change    | info  | upstream, model, from_state, to_state, reason                                    |
//! | rate_limit.rejected     | warn  | request_id, identity, dimension, retry_after_secs                                |
//! | request.completed       | info  | request_id, identity, frontend, model, outcome, total_elapsed_ms, tried (vec)    |
//!
//! All numeric fields use Rust integer types (no strings). `identity` uses
//! `AgentIdentity::log_id()` which returns `"anonymous"` or the full
//! `"sha256:<hex>"` form. Plaintext keys never appear in log output.
```

- [ ] **Step 2: Write failing test for retry.attempt fields**

Create `crates/router/tests/tracing_fields.rs`:

```rust
//! Plan 05 P05 T1: assert resilience events use the standard field set.
//!
//! Uses `tracing-test` to capture events emitted during ResilientCaller
//! invocations. Each test pins one event name and its fields.

use tracing_test::traced_test;

// Test helpers are defined in `tests/common/mod.rs` — small wrappers around
// the existing P02 / P03 mock provider builders. They construct
// `ResilientCaller` instances configured to drive a specific event path
// (one upstream that fails-then-succeeds; two upstreams where the first
// fails after retries; etc.). See the helper module for full bodies.

#[tokio::test]
#[traced_test]
async fn retry_attempt_event_has_standard_fields() {
    let caller = common::caller_with_failing_then_success_provider().await;
    let _ = caller
        .complete(
            common::single_upstream_chain(),
            common::sample_request(),
            vec![common::fast_retry_policy(2)],     // RetryPolicy from P01
            vec![common::breaker_policy_disabled()], // BreakerPolicy from P03
            agent_shim_router::AgentIdentity::Anonymous,
            "203.0.113.42".into(),
            "openai_chat/gpt-4o".into(),
        )
        .await;

    // Two attempts — one retry — one retry.attempt event.
    assert!(logs_contain("retry.attempt"));
    assert!(logs_contain("upstream=\"openai\""));
    assert!(logs_contain("attempt=1"));
    assert!(logs_contain("error_class=\"upstream_5xx\""));
    assert!(logs_contain("backoff_ms="));
    assert!(logs_contain("total_elapsed_ms="));
}
```

Add `tracing-test = "0.2"` to `crates/router/Cargo.toml` `[dev-dependencies]`.

- [ ] **Step 3: Run the test to verify it fails**

```bash
rtk cargo test -p agent-shim-router --test tracing_fields retry_attempt_event_has_standard_fields --quiet
```

Expected: FAIL — current `retry.rs` already emits something but field names are not standardized.

- [ ] **Step 4: Standardize the retry event fields in retry.rs**

In `crates/router/src/retry.rs`, replace any existing retry log call with:

```rust
tracing::warn!(
    target: "agent_shim::resilience",
    name: "retry.attempt",
    request_id = %ctx.request_id,
    upstream = %target.provider,
    model = %target.upstream_model,
    attempt = attempt,
    error_class = %crate::fallback::error_class_label(&err),
    backoff_ms = backoff.as_millis() as u64,
    total_elapsed_ms = total_elapsed.as_millis() as u64,
    "retrying after error"
);
```

`error_class_label()` is a small new helper in `fallback.rs` that maps `&ProviderError` to one of `"network"`, `"upstream_5xx"`, `"upstream_429"`, `"upstream_4xx"`, `"decode"`, `"encode"`, `"capability_mismatch"`, `"unknown_provider"`.

Add to `crates/router/src/fallback.rs`:

```rust
/// Stable label for an error class, used in `tracing` field values.
/// (Plan 05 P05 T1.)
pub(crate) fn error_class_label(e: &agent_shim_core::ProviderError) -> &'static str {
    use agent_shim_core::ProviderError;
    match e {
        ProviderError::Network(_) => "network",
        ProviderError::Upstream { status, .. } if *status >= 500 => "upstream_5xx",
        ProviderError::Upstream { status: 429, .. } => "upstream_429",
        ProviderError::Upstream { .. } => "upstream_4xx",
        ProviderError::Decode(_) => "decode",
        ProviderError::Encode(_) => "encode",
        ProviderError::CapabilityMismatch(_) => "capability_mismatch",
        ProviderError::UnknownProvider(_) => "unknown_provider",
    }
}
```

- [ ] **Step 5: Run the test to verify it passes**

```bash
rtk cargo test -p agent-shim-router --test tracing_fields retry_attempt_event_has_standard_fields --quiet
```

Expected: PASS.

- [ ] **Step 6: Standardize fallback.transition events**

Add a new test in `tracing_fields.rs`:

```rust
#[tokio::test]
#[traced_test]
async fn fallback_transition_event_has_standard_fields() {
    let caller = common::caller_two_upstreams_first_fails_after_retries().await;
    let _ = caller.complete(...).await;
    assert!(logs_contain("fallback.transition"));
    assert!(logs_contain("from_upstream=\"openai\""));
    assert!(logs_contain("to_upstream=\"copilot\""));
    assert!(logs_contain("reason=\"retry_exhausted\""));
}
```

Run it (FAIL), then in `crates/router/src/resilient_caller.rs`, at the point in the chain walk where we move to the next chain element after retry exhaustion:

```rust
tracing::warn!(
    target: "agent_shim::resilience",
    name: "fallback.transition",
    request_id = %ctx.request_id,
    from_upstream = %from.provider,
    to_upstream = %to.provider,
    reason = "retry_exhausted",
    "falling back to next upstream"
);
```

Run it again (PASS).

- [ ] **Step 7: Standardize breaker.state_change events**

Add a test:

```rust
#[tokio::test]
#[traced_test]
async fn breaker_state_change_event_has_standard_fields() {
    let registry = common::registry_with_low_threshold();
    // Drive 20 failures to trip the breaker.
    for _ in 0..20 {
        registry.record_failure("openai", "gpt-4o", 1.0);
    }
    assert!(logs_contain("breaker.state_change"));
    assert!(logs_contain("from_state=\"closed\""));
    assert!(logs_contain("to_state=\"open\""));
    assert!(logs_contain("reason=\"failure_threshold_exceeded\""));
}
```

Run it (FAIL), then in `crates/router/src/circuit_breaker.rs`, at the four points where the state transitions:

```rust
fn transition(&mut self, from: BreakerState, to: BreakerState, reason: &'static str) {
    tracing::info!(
        target: "agent_shim::resilience",
        name: "breaker.state_change",
        upstream = %self.upstream,
        model = %self.model,
        from_state = %state_label(from),
        to_state = %state_label(to),
        reason = reason,
        "circuit breaker state changed"
    );
    self.state = to;
}
```

Where `state_label()` returns `"closed" | "open" | "half_open"`.

The four `reason` values are `"failure_threshold_exceeded"` (Closed → Open), `"cooldown_elapsed"` (Open → HalfOpen), `"probe_succeeded"` (HalfOpen → Closed), and `"probe_failed"` (HalfOpen → Open).

Run it (PASS).

- [ ] **Step 8: Standardize rate_limit.rejected events**

Add a test:

```rust
#[tokio::test]
#[traced_test]
async fn rate_limit_rejected_event_has_standard_fields() {
    let caller = common::caller_with_tiny_per_key_bucket().await;
    // Burn through the bucket, then make one more request.
    let _ = caller.complete_n_times(3).await;
    assert!(logs_contain("rate_limit.rejected"));
    assert!(logs_contain("dimension=\"per_key\""));
    assert!(logs_contain("retry_after_secs="));
    assert!(logs_contain("identity="));
}
```

Run (FAIL if P04 emitted a different field shape), then update the existing `tracing::warn!` in `rate_limit.rs` (added in P04 T2) to match:

```rust
tracing::warn!(
    target: "agent_shim::resilience",
    name: "rate_limit.rejected",
    request_id = %ctx.request_id,
    identity = %identity.log_id(),
    dimension = %dimension_label(&dim),
    retry_after_secs,
    "rate limit exceeded"
);
```

Run (PASS).

- [ ] **Step 9: Add per-request summary log**

Add a test:

```rust
#[tokio::test]
#[traced_test]
async fn request_completed_event_emitted_on_success() {
    let caller = common::caller_with_healthy_provider().await;
    let _ = caller.complete(...).await;
    assert!(logs_contain("request.completed"));
    assert!(logs_contain("outcome=\"success\""));
    assert!(logs_contain("total_elapsed_ms="));
}

#[tokio::test]
#[traced_test]
async fn request_completed_event_emitted_on_chain_exhausted() {
    let caller = common::caller_all_upstreams_fail().await;
    let _ = caller.complete(...).await;
    assert!(logs_contain("request.completed"));
    assert!(logs_contain("outcome=\"no_upstream_succeeded\""));
    // tried vec includes per-upstream attempt counts and last_error labels.
}
```

Run (FAIL), then in `crates/router/src/resilient_caller.rs`, wrap the body of `complete()` with a summary emit:

```rust
let start = std::time::Instant::now();
let mut tried: Vec<TriedUpstream> = Vec::new();
let result = self.inner_complete(chain, req, ..., &mut tried).await;
let outcome = match &result {
    Ok(_) => "success",
    Err(ResilienceError::RateLimited { .. }) => "rate_limited",
    Err(ResilienceError::NoUpstreamSucceeded { .. }) => "no_upstream_succeeded",
    Err(ResilienceError::AllBreakersOpen { .. }) => "all_breakers_open",
    Err(ResilienceError::TerminalError { .. }) => "terminal_error",
};
let level = if result.is_ok() { tracing::Level::INFO } else { tracing::Level::WARN };
tracing::event!(
    target: "agent_shim::resilience",
    name: "request.completed",
    level,
    request_id = %ctx.request_id,
    identity = %identity.log_id(),
    frontend = %frontend_kind_str,
    model = %model_alias,
    outcome,
    total_elapsed_ms = start.elapsed().as_millis() as u64,
    tried = ?tried,
);
result
```

`TriedUpstream { upstream: String, attempts: u32, last_error: Option<String>, elapsed_ms: u64 }` is already in scope from P02 T3.

`request_id` flows in from the existing `crates/observability/` request-id middleware via `ResilientCaller::Context` (a small new struct, ~10 lines, holding fields populated by the pipeline at request entry).

Run (PASS).

- [ ] **Step 10: Run full test suite**

```bash
rtk cargo test --workspace --quiet
```

Expected: all existing tests + 5 new `tracing_fields.rs` tests pass.

- [ ] **Step 11: Commit**

```bash
git add crates/router/src/retry.rs crates/router/src/resilient_caller.rs crates/router/src/circuit_breaker.rs crates/router/src/rate_limit.rs crates/router/src/fallback.rs crates/router/Cargo.toml crates/router/tests/tracing_fields.rs
git commit -m "feat(router): standardize resilience tracing field set (Plan 05 P05 T1)

Pins the tracing event taxonomy from §6.5 of the Phase 4 design spec:
- retry.attempt (warn): request_id, upstream, model, attempt,
  error_class, backoff_ms, total_elapsed_ms
- retry.exhausted (warn): request_id, upstream, model, attempts,
  last_error
- fallback.transition (warn): request_id, from_upstream, to_upstream,
  reason
- breaker.state_change (info): upstream, model, from_state, to_state,
  reason
- rate_limit.rejected (warn): request_id, identity, dimension,
  retry_after_secs
- request.completed (info on success / warn on failure): request_id,
  identity, frontend, model, outcome, total_elapsed_ms, tried (vec)

All events use target='agent_shim::resilience'. API keys never appear
in log output — identity field uses AgentIdentity::log_id() which
returns 'anonymous' or 'key:sha256:<hex8>'.

5 new tests in crates/router/tests/tracing_fields.rs use tracing-test
to capture events and assert each field set."
```

---

### Task 2: ADR-0004 — ResilientCaller orchestrator choice

**Files:**
- Create: `docs/adr/0004-resilient-caller.md`

- [ ] **Step 1: Write the ADR**

Create `docs/adr/0004-resilient-caller.md`:

```markdown
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
```

- [ ] **Step 2: Commit**

```bash
git add docs/adr/0004-resilient-caller.md
git commit -m "docs: ADR-0004 — ResilientCaller orchestrator (Plan 05 P05 T2)

Records the §3 / §5 architecture choice from the Phase 4 design spec:
the orchestrator pattern over middleware-per-subsystem (A12) and
inline-in-pipeline (A13). Documents why each alternative was rejected
and what consequences the chosen approach carries forward.

Frozen-core invariant resumes from this ADR onward — Phase 4 plans
declare 'core changes: NONE'."
```

---

### Task 3: Architecture + contributing doc updates

**Files:**
- Modify: `docs/architecture.md`
- Modify: `docs/contributing.md`

- [ ] **Step 1: Update docs/architecture.md capability matrix**

Add a "Resilience" row to the capability matrix table:

```markdown
| Resilience | ❌ | ❌ | ❌ | ✅ |
```

(Columns are v0.1 / v0.2 / v0.3 / v0.4. Update the column header row if it stops at v0.3.)

Add a new `## Resilience layer (v0.4+)` section after the "Streaming" section:

```markdown
## Resilience layer (v0.4+)

Phase 4 introduces a `ResilientCaller` orchestrator in `crates/router/`
that sits between the existing `ModelResolver` and the existing
`BackendProvider` trait. It composes four subsystems:

1. **Per-route fallback chains.** Each route lists primary + backups
   in `upstreams: [...]`. The chain is walked in order on retry
   exhaustion.
2. **Per-route retries.** Exponential backoff + jitter + total-time
   budget within a single upstream.
3. **Per-(upstream, model) circuit breakers.** Sliding-window
   failure-rate trip; single half-open probe.
4. **Token-bucket rate limiting on four dimensions:** per API key,
   per route, per upstream, per source IP.

See [ADR-0004](adr/0004-resilient-caller.md) for the layering rationale
and [`docs/resilience.md`](resilience.md) for the operator-facing guide.

### Performance overhead targets

(From §8 of the Phase 4 design spec.)

- Rate-limit gate disabled: zero atomic ops, zero allocations on hot path.
- Rate-limit gate enabled, no buckets exceeded: ≤4 atomic loads per request.
- Breaker gate (always on): one `RwLock::read()` per chain element;
  uncontended path ~50ns.
- Retry overhead: zero on success.

No benchmark in v0.4 — Phase 5's metrics will surface real-world overhead.
```

Update the "Frozen-core invariant" paragraph (if present) to note the
resumption: ADR-0003 was the bounded one-time exception in Phase 3;
Phase 4 plans declared `core changes: NONE` and the invariant holds
again.

- [ ] **Step 2: Update docs/contributing.md**

Add a new subsection `## How to add a resilience subsystem`:

```markdown
## How to add a resilience subsystem

Phase 4 introduced four subsystems (fallback, retry, circuit breaker,
rate limit) that share a common shape. If a future feature needs the
same shape (e.g., distributed state in v0.5, or cost-aware routing),
follow this pattern:

1. **New module under `crates/router/src/<subsystem>.rs`.** Pure logic
   first — state machine, math, classification — with property tests
   that don't touch the rest of the gateway.
2. **`Registry` newtype.** A `Registry` struct holds the subsystem's
   per-key state (e.g., `BreakerRegistry`, `LimiterRegistry`). Use
   `Arc<RwLock<HashMap<K, V>>>` for state that updates on contention,
   or atomic primitives + `DashMap` for hot-path-read-only state.
3. **Wire into `ResilientCaller`.** Add an `Arc<NewRegistry>` field
   and a check call at the right point in the layering — see §5.2 of
   the Phase 4 design spec. Update the [tracing event taxonomy](
   ../crates/router/src/resilient_caller.rs) module-level doc comment
   with the new event names + field set.
4. **Config schema.** Add the subsystem's config block to
   `crates/config/src/schema.rs` (top-level if cross-cutting; on
   `RouteEntry` if per-route). Add validation rules to
   `crates/config/src/validation.rs`. Defaults should reproduce
   pre-feature behavior — operators opt in explicitly.
5. **Tests in three layers** mirroring §8 of the design spec:
   pure unit tests in-module; subsystem-composition tests in
   `crates/router/tests/`; cross-protocol smokes in
   `crates/protocol-tests/tests/`.
6. **Operator docs.** A `## <Subsystem>` subsection in
   `docs/resilience.md`; an entry in the operator log line reference;
   a config example.
7. **ADR.** If the layering choice is non-obvious, write an ADR.
   Otherwise, the ADR can wait until a competing approach surfaces.

The pattern is intentionally light — none of the four Phase 4
subsystems used all seven steps, but each used at least four.
```

- [ ] **Step 3: Run lint checks**

```bash
rtk cargo fmt --all -- --check
```

Expected: PASS (docs files are markdown; this is a smoke test that the previous task's commits don't break formatting).

- [ ] **Step 4: Commit**

```bash
git add docs/architecture.md docs/contributing.md
git commit -m "docs: architecture + contributing updates for v0.4 (Plan 05 P05 T3)

docs/architecture.md:
- Capability matrix gains a Resilience row (v0.4 only).
- New 'Resilience layer (v0.4+)' section with the four subsystems and
  links to ADR-0004 + docs/resilience.md.
- Performance overhead targets paragraph from §8 of the design spec.
- Frozen-core invariant note: resumed from ADR-0003 onward.

docs/contributing.md:
- New 'How to add a resilience subsystem' subsection, the 7-step
  shape that the Phase 4 subsystems share."
```

---

### Task 4: Provider doc Resilience subsections + README + CHANGELOG

**Files:**
- Modify: `docs/providers/anthropic.md`
- Modify: `docs/providers/openai-compatible.md`
- Modify: `docs/providers/gemini.md`
- Modify: `docs/providers/deepseek.md`
- Modify: `docs/providers/github-copilot.md`
- Modify: `README.md`
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Add Resilience subsection to each provider doc**

For each of the five provider docs, append at the end (or before any "Known issues" section if one exists):

```markdown
## Resilience behavior

This provider participates in the v0.4 resilience subsystem. See
[`docs/resilience.md`](../resilience.md) for the operator-facing guide
and the Phase 4 design spec for the layering details.

**Default fallback eligibility for this provider:**

| Error class                 | Eligible? |
|-----------------------------|-----------|
| Network errors (timeout, DNS) | ✅ Eligible — falls back |
| Upstream 5xx                  | ✅ Eligible — falls back |
| Upstream 429 (rate limit)     | ✅ Eligible — falls back |
| Upstream 4xx (auth, validation) | ❌ Terminal — no fallback |
| Decode/encode errors          | ❌ Terminal — no fallback |
| Capability mismatch           | ❌ Terminal — no fallback |

**Provider-specific notes:**

(Per provider — fill in any quirks.)

- `anthropic.md`: Anthropic returns 529 (overloaded) which maps to
  upstream_5xx and is fallback-eligible.
- `openai-compatible.md`: vendors that proxy OpenAI may return 503
  for capacity issues — fallback-eligible.
- `gemini.md`: Gemini returns 429 with `RESOURCE_EXHAUSTED` body for
  quota issues — fallback-eligible.
- `deepseek.md`: DeepSeek's `insufficient_balance` returns 402 — terminal,
  no fallback (consistent with billing-related errors).
- `github-copilot.md`: Copilot's auth refresh path returns 401 on stale
  tokens; AgentShim's existing token-refresh logic handles this *before*
  the resilience layer sees it. Genuine 401 (revoked Copilot subscription)
  is terminal.

**Streaming caveat (D4):** Once `provider.complete()` returns
`Ok(stream)` and bytes flow to the client, fallback is no longer
possible. Mid-stream failures surface as stream-level errors. This
matches v0.3 behavior; v0.4 does not introduce buffering.
```

For the four common rows of the table, copy verbatim. The "Provider-specific notes" bullet is one line per file (the relevant line above).

- [ ] **Step 2: Update README.md capability matrix**

In the capability matrix table, add a "Resilience" row showing v0.4 ✅:

```markdown
| Resilience               |       |       |       |   ✅   |
```

(Columns: v0.1 v0.2 v0.3 v0.4. Update the header row if needed.)

- [ ] **Step 3: Update README.md "What's NOT in v0.4"**

Replace the existing "What's NOT in v0.3" section with:

```markdown
## What's NOT in v0.4

Phase 4 ships the resilient gateway (fallback, retries, circuit breakers,
rate limiting). It does NOT ship:

- Distributed/shared state — breaker and rate-limit state lives in
  process memory; multi-instance deployments lose strict enforcement.
  Phase 5 candidate.
- Cost/latency-aware routing — Phase 5+.
- Per-key per-day budget caps — Phase 5.
- Prometheus metrics, OpenTelemetry, hot-reload config — Phase 5.
- Audio / file content end-to-end — Phase 6+ if at all.
- Multi-account Copilot — Phase 6.
- OAuth Anthropic — Phase 6.
- Embeddings, moderation, admin UI, end-user identity, billing —
  permanently out of scope.

Phase 4's `ResilientCaller` orchestrator is the foundation that
distributed state and cost-aware routing will plug into in v0.5+.
```

- [ ] **Step 4: Update README.md releases bullet**

Add to the releases list:

```markdown
- [v0.4.0](https://github.com/<org>/<repo>/releases/tag/v0.4.0) (2026-05-08):
  Resilient gateway — fallback chains, per-route retries, circuit
  breakers, four-dimensional rate limiting, API key auth.
```

- [ ] **Step 5: Update README.md configuration example**

In the configuration example block, add the full resilience config (route-level + top-level) so operators see what v0.4 enables. Use the worked example from §4 of the design spec, condensed:

```yaml
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstreams:
      - {name: openai, model: gpt-4o-2024-11-20}
      - {name: copilot, model: gpt-4o}
    retry:
      max_attempts: 3
      initial_backoff_ms: 100
      multiplier: 2.0
      jitter_pct: 25
      total_budget_ms: 5000
    breaker:
      enabled: true
      failure_threshold_pct: 50
      min_requests: 20

auth:
  enabled: true
  required: false
  keys:
    "sha256:abc123...":
      label: "alice-ci"

rate_limit:
  enabled: true
  per_key:
    default: {rate_per_sec: 10, burst: 30}
    overrides:
      "sha256:abc123...": {rate_per_sec: 100, burst: 300}
    anonymous: {rate_per_sec: 1, burst: 5}
  per_upstream:
    openai: {rate_per_sec: 200, burst: 500}
```

- [ ] **Step 6: Add CHANGELOG `[0.4.0]` entry**

In `CHANGELOG.md`, insert above the `## [0.3.0]` heading:

```markdown
## [0.4.0] — 2026-05-08

Phase 4 release: the gateway becomes **resilient** as well as
protocol-translating. Four subsystems — fallback chains, per-route
retries, per-(upstream, model) circuit breakers, and four-dimensional
token-bucket rate limiting — compose under a new `ResilientCaller`
orchestrator (see [ADR-0004](docs/adr/0004-resilient-caller.md)). All
four subsystems are independently configurable and default to behavior
that preserves v0.3 wire output for healthy upstreams.

### Added

#### Per-route fallback chains (Plan 02)

- Routes can list primary + backup upstreams in an ordered
  `upstreams: [...]` array. Fallback fires on retry exhaustion against
  the current upstream when the error is fallback-eligible (network /
  upstream 5xx / upstream 429).
- Existing v0.3 singular `upstream`/`upstream_model` shape continues to
  work — internally it deserializes to a 1-element vec. Mixed shapes
  (both singular and array on the same route) are rejected at startup.
- Three new `HandlerError` variants:
  `NoUpstreamSucceeded` (HTTP 503), `AllBreakersOpen` (HTTP 503), and
  `RateLimited` (HTTP 429 + `Retry-After`) with dialect-correct
  envelopes for OpenAI Chat, OpenAI Responses, and Anthropic Messages.

#### Per-route retries (Plan 01)

- Exponential backoff + jitter + total-time budget within a single
  upstream. Defaults: 2 attempts, 100ms initial, 2.0× multiplier,
  ±25% jitter, 5000ms total budget.
- Default fallback eligibility: `network`, `upstream_5xx`,
  `upstream_429`. Operators override per route via
  `retry.retry_on: [...]`.

#### Per-(upstream, model) circuit breakers (Plan 03)

- Sliding-window failure-rate breaker. Trips when failure rate over
  `window_secs` (default 60s) exceeds `failure_threshold_pct` (default
  50%) across at least `min_requests` (default 20). Open state holds
  for `open_cooldown_secs` (default 30s) before transitioning to
  half-open.
- Half-open state allows exactly one probe via `AtomicBool::compare_
  exchange`. Probe success → Closed; probe failure → Open with fresh
  cooldown.
- Per-(provider_name, model) keying — a misconfigured model on a
  healthy provider trips its own breaker without affecting siblings.

#### Four-dimensional token-bucket rate limiting (Plan 04)

- Independent buckets per: API key, route, upstream, source IP. A
  request must satisfy all applicable buckets. The first to reject
  names the dimension in the error.
- Built on the [`governor`](https://crates.io/crates/governor) crate
  for atomic, lock-free per-bucket math.
- HTTP 429 responses include `Retry-After` from `governor`'s
  `wait_time_from(now)`.

#### API key auth (Plan 04)

- Keys come in via `Authorization: Bearer <key>` or
  `x-api-key: <key>`. The gateway hashes the key with SHA-256 and
  looks up the hash in `auth.keys.<sha256:hex>`. Plaintext is never
  stored or logged.
- `auth.enabled=false` (default) skips header inspection entirely
  (zero overhead).
- `auth.enabled=true, required=false`: unknown keys → tagged
  `Anonymous` (uses anonymous bucket).
- `auth.required=true`: unknown keys → HTTP 401 before any upstream
  contact.

#### Structured tracing (Plan 05)

- Every retry attempt, fallback transition, breaker state change,
  rate-limit rejection, and request completion emits a structured
  `tracing` event under `target = "agent_shim::resilience"` with a
  fixed field set (see `crates/router/src/resilient_caller.rs`
  module-level doc). Identity in logs is the SHA-256 hash form
  (`sha256:<hex>`) or `anonymous` — plaintext keys never appear in
  logs.
- Per-request summary log at request end with the full chain walk.
- Prometheus metrics + OpenTelemetry are deferred to Phase 5.

### Changed

- **Default `retry.max_attempts: 2`** — small behavior change on
  upgrade (one extra round-trip on transient errors). Operators
  wanting strict v0.3 behavior (no retries) set
  `retry: {max_attempts: 1}` per route.
- `BackendTarget` resolution now returns `Vec<BackendTarget>` instead
  of `Option<BackendTarget>`. The first element is the primary; the
  rest are fallback candidates. v0.3 single-upstream routes produce a
  1-element vec.
- `ResilientCaller::complete` replaces the direct `provider.complete()`
  call in `pipeline.rs`. All four subsystems compose at this single
  point; see [ADR-0004](docs/adr/0004-resilient-caller.md).

### Deprecated

None. Both v0.3 and v0.4 config shapes are supported indefinitely.

### Fixed

None — this release is additive against the v0.3 baseline.

### Documentation

- New [`docs/resilience.md`](docs/resilience.md) operator-facing guide
  with quick-start config, SHA-256 key generation recipe, retry
  tuning, layering walkthrough, log line reference, and the
  multi-instance caveat.
- New [`docs/adr/0004-resilient-caller.md`](docs/adr/0004-resilient-caller.md)
  records the orchestrator-pattern choice over middleware-per-subsystem
  and inline-in-pipeline alternatives.
- `docs/architecture.md` capability matrix gains a Resilience row;
  performance overhead targets paragraph from the design spec.
- `docs/contributing.md` gains a "How to add a resilience subsystem"
  subsection.
- All five provider docs gain a "Resilience behavior" subsection
  documenting fallback eligibility per provider.

### Known limitations

- **Single-instance state.** Breaker and rate-limit state lives in
  process memory. Multi-instance deployments behind a load balancer
  lose strict enforcement until the breaker actually trips at the
  upstream. Distributed state (Redis backend) is a Phase 5 candidate.
- **Pre-stream fallback only.** Once `provider.complete()` returns
  `Ok(stream)` and bytes flow to the client, fallback is no longer
  possible. Mid-stream failures surface as stream-level errors. This
  matches v0.3 behavior; v0.4 does not introduce buffering.

### Frozen-core invariant resumes

ADR-0003 (Phase 3) was a bounded one-time exception. All five
Phase 4 plan files declared `core changes: NONE`, and
`git diff v0.3.0..master -- crates/core/` is empty for the v0.4.0
release tag.
```

- [ ] **Step 7: Run all checks**

```bash
rtk cargo fmt --all -- --check && rtk cargo clippy --workspace --all-targets -- -D warnings && rtk cargo test --workspace --quiet
```

Expected: all PASS.

- [ ] **Step 8: Commit**

```bash
git add docs/providers/anthropic.md docs/providers/openai-compatible.md docs/providers/gemini.md docs/providers/deepseek.md docs/providers/github-copilot.md README.md CHANGELOG.md
git commit -m "docs: provider Resilience subsections + README + CHANGELOG (Plan 05 P05 T4)

Each of the 5 provider docs (anthropic, openai-compatible, gemini,
deepseek, github-copilot) gains a 'Resilience behavior' subsection
with the standard fallback-eligibility table and provider-specific
notes (e.g., Anthropic 529 = overloaded → fallback-eligible;
DeepSeek insufficient_balance = 402 → terminal).

README.md:
- Capability matrix gains v0.4 Resilience row.
- 'What's NOT in v0.4' supersedes v0.3.
- Releases bullet for v0.4.0.
- Configuration example gains the full resilience config block.

CHANGELOG.md [0.4.0] entry covers all five plans:
- Added: fallback chains, retries, breakers, rate limiting, auth,
  tracing.
- Changed: default retry.max_attempts=2 (small behavior change),
  BackendTarget vec resolution, ResilientCaller pipeline integration.
- Deprecated: none.
- Documentation: resilience.md, ADR-0004, architecture/contributing
  refresh, provider Resilience subsections.
- Known limitations: single-instance state, pre-stream fallback only.
- Frozen-core invariant resumes from ADR-0003."
```

---

### Task 5: Workspace version bump and release verification

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock` (regenerated by build)

- [ ] **Step 1: Verify EDITOR_VERSION constants are already at AgentShim/0.4.0**

```bash
rtk grep -n "AgentShim/" crates/providers/src/github_copilot/headers.rs
```

Expected output (set during the kickoff bump):

```
1:pub const EDITOR_VERSION: &str = "AgentShim/0.4.0";
2:pub const EDITOR_PLUGIN_VERSION: &str = "AgentShim/0.4.0";
```

If they are not already at `AgentShim/0.4.0`, edit them now.

- [ ] **Step 2: Bump workspace version**

In `Cargo.toml`, change line 6:

```toml
# from:
version = "0.4.0-dev"
# to:
version = "0.4.0"
```

- [ ] **Step 3: Verify the bump compiles + tests pass**

```bash
rtk cargo build --workspace
rtk cargo test --workspace --quiet
```

Expected: all PASS. `Cargo.lock` updates with the new version string for every workspace crate.

- [ ] **Step 4: Run release-mode build**

```bash
rtk cargo build --release -p agent-shim
```

Expected: `target/release/agent-shim` built clean. (This catches any release-only issues like missing `default-features = false` for an optional dep.)

- [ ] **Step 5: Run cargo deny**

```bash
rtk cargo deny check
```

Expected: no advisories, no license issues. The two new dependencies introduced in P04 (`governor`, `sha2`) are both MIT/Apache-2.0 dual-licensed and present no advisories.

- [ ] **Step 6: Verify frozen-core invariants**

```bash
rtk git diff v0.3.0..HEAD -- crates/core/
rtk git diff v0.3.0..HEAD -- crates/frontends/
rtk git diff v0.3.0..HEAD -- crates/providers/src/
```

Expected: all three commands produce empty output. (The third command intentionally excludes `crates/providers/Cargo.toml` and any docs; only source code under `src/` is checked.)

- [ ] **Step 7: Verify v0.3 example configs still validate**

```bash
rtk find config -name "*.yaml"
```

For each config file under `config/`:

```bash
rtk cargo run -p agent-shim -- validate-config --config <path>
```

Expected: all PASS. Specifically: any v0.3 config that uses the singular `upstream`/`upstream_model` shape and lacks `auth` / `rate_limit` / `breaker` / `retry` blocks must validate, since defaults preserve v0.3 behavior (D12 caveat: one extra retry on transient errors).

- [ ] **Step 8: Live-traffic smoke (per §8 of design)**

Optional but recommended before tagging. Run the local server with a 2-upstream fallback config; force-fail upstream A (misconfigure key); send a real request; verify upstream B serves the response. This catches integration issues that mocks miss.

- [ ] **Step 9: Final test suite + lint**

```bash
rtk cargo fmt --all -- --check
rtk cargo clippy --workspace --all-targets -- -D warnings
rtk cargo test --workspace --quiet
```

Expected: all PASS. Workspace test count: ~525 (477 v0.3 baseline + ~48 from Phase 4).

- [ ] **Step 10: Commit the version bump**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: bump workspace version 0.4.0-dev → 0.4.0 (Plan 05 P05 T5)

Phase 4 ships. Cargo.lock regenerated; all workspace crates pin the
new version. EDITOR_VERSION / EDITOR_PLUGIN_VERSION already at
AgentShim/0.4.0 from the kickoff bump.

Verified before commit:
- cargo build --workspace
- cargo build --release -p agent-shim
- cargo test --workspace --quiet (~525 tests pass)
- cargo clippy --workspace --all-targets -- -D warnings clean
- cargo fmt --all -- --check clean
- cargo deny check clean
- git diff v0.3.0..HEAD -- crates/core/ is empty (frozen-core holds)
- git diff v0.3.0..HEAD -- crates/frontends/ is empty
- git diff v0.3.0..HEAD -- crates/providers/src/ is empty
- All v0.3 example configs continue to validate."
```

- [ ] **Step 11: Tag the release (manual — operator step)**

The plan does not push or tag automatically. After the release-merge
commit lands on `master`, the operator runs:

```bash
rtk git tag -a v0.4.0 <release-merge-sha> -m "v0.4.0 — Phase 4: resilient gateway"
rtk git push origin v0.4.0
```

The tag points at the merge commit, matching the v0.3.0 tagging
convention from Phase 3.

---

## Definition of Done

- [ ] All 5 tasks complete.
- [ ] `cargo test --workspace --quiet` passes (~525 tests).
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] `cargo fmt --all -- --check` clean.
- [ ] `cargo build --release -p agent-shim` succeeds.
- [ ] `cargo deny check` clean.
- [ ] `git diff v0.3.0..HEAD -- crates/core/` is empty.
- [ ] `git diff v0.3.0..HEAD -- crates/frontends/` is empty.
- [ ] `git diff v0.3.0..HEAD -- crates/providers/src/` is empty.
- [ ] All v0.3 example configs continue to validate.
- [ ] CHANGELOG `[0.4.0]` entry comprehensive (Added/Changed/Deprecated/Fixed/Documentation/Known limitations/Frozen-core).
- [ ] README capability matrix v0.4; "What's NOT in v0.4"; releases bullet; full resilience config block.
- [ ] `docs/architecture.md` capability matrix Resilience row + performance overhead paragraph.
- [ ] `docs/resilience.md` operator-facing guide (created in P04 T7).
- [ ] `docs/adr/0004-resilient-caller.md` published.
- [ ] All five provider docs gain `## Resilience behavior` subsection.
- [ ] `docs/contributing.md` gains "How to add a resilience subsystem" subsection.
- [ ] Workspace version bumped `0.4.0-dev → 0.4.0`.
- [ ] `EDITOR_VERSION` / `EDITOR_PLUGIN_VERSION` already at `AgentShim/0.4.0` (set during kickoff).
- [ ] All 5 plan files merged via `--no-ff` with detailed commit messages.
- [ ] Live-traffic smoke test passes (real fallback chain; one upstream forced down; SDK gets response from backup).
- [ ] Tag `v0.4.0` on master release-merge commit (operator step).

After this plan merges and tags, **v0.4.0 is shipped.** The next phase (v0.5) plug-points are:

- Distributed state: `BreakerRegistry` and `LimiterRegistry` traits already shape the per-key state interface; a Redis-backed implementation slots in without changing the orchestrator.
- Cost/latency-aware routing: `Vec<BackendTarget>` ordering can be re-ranked dynamically; the chain-walker doesn't care about source.
- Prometheus metrics: the structured tracing field set is the metric surface — adding a Prometheus exporter is purely additive.
- Hot-reload: `AppState` is `Arc`-shared; rebuilding on SIGHUP and atomic-swapping is a v0.5 candidate.

The orchestrator pattern (ADR-0004) is the foundation. Phase 5
extends; Phase 4 closes.
