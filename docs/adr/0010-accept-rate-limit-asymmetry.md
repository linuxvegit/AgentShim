# ADR-0010: Accept rate-limit reservation asymmetry permanently

## Status

Accepted (2026-05-29) — supersedes the PR 4 entry of ADR-0009's
landing sequence (§Landing sequence, item 4).

## Context

ADR-0009 §3 documented an implementation asymmetry shipped in PR 2:

- **Breaker side**: true RAII. `BreakerHold::consume()` records
  success/failure; Drop without consume records as abandoned probe.
- **Rate-limit side**: observation-only. `governor::RateLimiter::
  check()` decrements the token bucket on call with no refund path.

ADR-0009 §Landing sequence named PR 4 as the closer: replace
`governor` with a reservation-aware limiter so the §3 asymmetry would
collapse and `AdmissionTicket::consume()` would be true RAII on both
sides.

As of this ADR, PR 1 (commit 557602c), PR 2 (commit 7573212), and
PR 3 (commit c46c902) have all shipped on master. Time to decide PR 4.

Re-examining the actual harm: the asymmetry over-charges **one token
per request** that (1) passes the rate-limit gate, (2) reaches one of
admission's later gates (cost-filter, capability, all-breakers-open),
and (3) is then rejected at that later gate. The same one-token charge
applies if the stream connects but never delivers a first byte. The
over-charge is bounded, per-request, and observable via the existing
`rate_limit_rejected_total` metric. As of this ADR, no user report or
ops dashboard names this asymmetry as a source of real harm.

## Decision

We are **not** closing the §3 asymmetry. PR 4 as written in ADR-0009's
landing sequence is **cancelled**.

Specifically:

- `governor::RateLimiter::check()` consume-on-call semantics stay.
- `RateLimitReservation` stays observation-only (Drop = no-op,
  `consume()` = no-op).
- `AdmissionTicket` continues to call `reservation.consume()` on first
  byte — the interface contract holds, the implementation is
  intentionally a no-op.
- `BreakerHold` remains true RAII (unchanged from PR 2).

`RateLimitReservation` stays in the public API. Removing it would be a
breaking change for callers that already shape around the symmetric
interface, and keeping it preserves the door for a future revisit
without an API break.

## Rationale

### 1. No signal of real harm

Operators are not reporting under-served per-key budgets correlated
with cost/capability/breaker rejections. The instrumentation is in
place (`rate_limit_rejected_total`, the breaker registry's
`abandoned_probe` accounting); if such a signal appears, this decision
can be revisited.

### 2. Cost of closing is non-trivial

Three closure paths were considered:

- **Custom `AtomicU64` token bucket with true-refund Drop.** Roughly
  a 3-5 day rewrite of a hot path, plus porting the Retry-After
  computation from GCRA's `wait_time_from` to a new clock arithmetic.
  Tests in `crates/router/src/rate_limit.rs` and
  `crates/gateway/tests/rate_limit_per_key_envelope.rs` are tuned to
  GCRA semantics and would need their own validation gate.
- **Swap to `gardal`** (or `leaky-bucket`). Smaller diff than a
  rewrite, but introduces a less battle-tested dependency. No signal
  that this is worth the audit cost.
- **Wrap `governor` + side-ledger for visibility only.** Adds
  complexity without actually fixing the token math — half-measure
  rejected as worst-of-both.

### 3. Sunk-cost rationale rejected

"We promised PR 4 in ADR-0009" is not sufficient grounds when
re-examination shows no real harm AND a real cost to close. The PR 4
plan in ADR-0009 was written under the assumption that the asymmetry
would matter operationally; the data after PR 3 ships disconfirms
that assumption.

### 4. Bounded over-charge is acceptable

One token per request, in a narrow failure path. This matches v0.7
baseline behavior on the canonical path — PR 2 did not regress, it
preserved the pre-existing semantic. The ADR-0009 §3 asymmetry table
documented this honestly at PR 2 time, and the documentation is the
right artifact for the trade-off going forward.

## Revisit triggers

Reopen this decision if any of these surface:

- An ops report of per-key budget under-serving correlated with
  cost/capability/breaker rejection counts.
- A test we want to write but cannot, because the asymmetry blocks
  the assertion.
- A v0.9+ feature that requires reservation semantics beyond
  Drop-without-consume — e.g. partial commit, cost-weighted
  reservation, predictive accounting across multiple in-flight
  requests.
- A breaking change in the `governor` crate that forces a swap
  anyway; in that case, picking a reservation-aware library closes
  the asymmetry as a side effect at no incremental cost.

## Consequences

- ADR-0009's "4-PR landing sequence" becomes a 3-PR sequence. All
  three have shipped on master.
- The §3 asymmetry table becomes a **stable, documented trade-off**
  rather than a deferred bug.
- The Module / Interface boundary that ADR-0009 built is preserved
  exactly as it is; only the per-dimension implementation differs
  from §3's aspiration.
- Doc comments that previously said "PR 4 closes this" now point at
  this ADR instead, naming the trade-off as accepted rather than
  pending.
- ADR-0009 §3 and §Landing sequence are amended in the same commit
  that introduces this ADR, both pointing at ADR-0010 as the
  superseding decision for the rate-limit side.
