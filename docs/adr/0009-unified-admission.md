# ADR-0009 — Unified admission: ResolvedRoute + AdmissionTicket

**Status:** Proposed
**Date:** 2026-05-29
**Phase:** 8 (candidate)
**Related:** ADR-0001 (Anthropic hybrid path — honored, not superseded),
ADR-0004 (resilient caller — its rate-limit + breaker gates move behind
admission), ADR-0006 (cost-aware routing — its filter pass moves behind
admission), ADR-0007 (frozen-core lift discipline — checked against and
respected; this ADR adds no `crates/core/` items), ADR-0008 (plugin
system — H2/H3 hooks remain on the pipeline, unchanged).

## Context

The v0.7 request pipeline assembles four route facts via four parallel
lookups on the same `(frontend, model)` pair:

- `Router::resolve(frontend, model)` for the chain.
- `Router::find_retry_policy(frontend, model)` for retry config.
- `Router::find_breaker_policy(frontend, model)` for breaker config.
- `find_route_entry(&snapshot.config, frontend, model)` (an
  un-encapsulated function in `crates/gateway/src/pipeline.rs`) for the
  cost-filter route fields (`min_tier`, `max_cost_usd`,
  `latency_budget_ms`).

The first three reads come from the immutable `ModelResolver` on
`AppCore`, built once at startup. The fourth reads from
`snapshot.config.routes`, which the reload-applying task hot-swaps
on SIGHUP or `POST /admin/reload`. This is a **field-level reload
split on the same routes record**: changing a route's `upstream`
chain requires restart, but changing its `max_cost_usd` tunes live.
The split is undocumented in v0.7 (this ADR adds it to CONTEXT.md
as "Routes-table reload split") and constrains the v0.8 design —
ResolvedRoute can collapse the three startup-frozen lookups, but
not the fourth.

`ModelResolver::list_catalog` walks the same `list_routes → resolve`
chain a third time to build the v0.7 model catalog. `ResilientCaller`
exposes two entry points (`complete` and `complete_with_cost_filter`),
glued together by `CostFilterInputs<'a>` — a workaround struct bundling
the inputs the second entry point needs to avoid an 11-positional-arg
signature. The route-uniform retry and breaker configs are then cloned
per chain element into two parallel `Vec`s wrapped by `PolicyVec<T>`,
which exists only to enforce that the two vectors stay the same length
as the chain.

In parallel, the pipeline's `try_proxy_raw` short-circuit (used today
by the OpenAI Responses frontend on Responses→Responses upstreams, and
by the Anthropic hybrid path for Anthropic→Anthropic) carries a known
gap, called out in a long comment at `crates/gateway/src/pipeline.rs`
L492–505: a successful `proxy_raw` skips the rate-limit and breaker
gates entirely, because both gates live inside `ResilientCaller::
complete`. The comment ends with "Either fix needs a small refactor
(e.g. an explicit RateLimitTicket value that's only consumed when the
request actually completes) — out of scope for v0.4." This ADR is that
small refactor, generalised across rate-limit, breaker, and cost
filter, with capability gating folded in.

Three orthogonal choices pin the v0.8 design:

1. Where does the merged route value live, and how rich is it?
2. What is the lifecycle of the admission gates' reservations relative
   to the upstream call?
3. How is the capability gate handled on the byte-identity passthrough
   path, where the body is intentionally not decoded?

## Decision

### (1) `ResolvedRoute` is a thin value owned by the router crate

`ModelResolver::resolve_route(frontend, model)` returns one
`ResolvedRoute` value carrying the three startup-frozen route facts:

```rust
pub struct ResolvedRoute {
    pub chain: Vec<BackendTarget>,
    pub retry: RetryConfig,
    pub breaker: BreakerConfig,
    pub route_label: Arc<str>,   // "<frontend_kind_str>/<alias>"
}
```

The cost-filter route fields (`min_tier`, `max_cost_usd`,
`latency_budget_ms`) are deliberately **not** carried on
`ResolvedRoute`. They live on a different reload epoch
(`snapshot.config.routes`, hot-swapped on reload) than the resolver
itself (`AppCore.resolver`, built once at startup). Folding them into
`ResolvedRoute` would silently promote them from hot-reloadable to
startup-frozen — a behavioral regression for operators who tune cost
caps live. `find_route_entry` (today a free function in
`crates/gateway/src/pipeline.rs`) survives as the snapshot-reader for
those fields. The four v0.7 lookups collapse to two: one
`resolve_route` call covers chain/retry/breaker/label, and
`find_route_entry` reads cost caps from the current snapshot.

The shape is otherwise deliberately thin: no per-chain
`ResolvedPolicy` snapshot, no inbound request fields.
`RoutePolicy::resolve(canonical_request)` remains a separate call on
the dispatch path, so `list_catalog` — which has no inbound request —
can share the same `resolve_route` constructor.

**Trade-offs considered.** A "fat" variant that also carried a
`RouteEntryRef` to the cost-filter fields was the obvious symmetric
shape. Rejected because it requires either (a) reading
`RouteEntryRef` from the snapshot at `resolve_route` time, which
breaks `ModelResolver`'s startup-immutable property and forces a
reload-time rebuild of the resolver (out of scope for this ADR,
would need its own design); or (b) caching the cost fields into the
resolver at startup, which silently freezes them and regresses the
live-tuning semantics operators rely on. The asymmetry is real and
documented in the new CONTEXT.md "Routes-table reload split" entry —
it's not pretty, but it is the existing contract.

A "fat" variant that bundled the per-chain `ResolvedPolicy` was
rejected because catalog has no request to merge against; we would
have to either split the constructor or accept a nullable policy
field. A "lazy" variant where `chain` and policy were methods on a
struct holding only route-level data was rejected because the
indirection bought no Locality — every caller wants the chain
immediately.

`ResolvedRoute` lives in `agent-shim-router` rather than in
`agent-shim-core`. ADR-0007's frozen-core-lift discipline would admit
a new trait but not a new `pub struct`, and a `ResolvedRoute` trait
would buy nothing — the data shape is fully owned by the router.

### (2) `Admission` is a Module returning an RAII `AdmissionTicket`

`Admission::admit(resolved, route_entry, request, identity, client_ip)`
composes, in order:

1. **Rate-limit reservation.** All four buckets (per API key, per
   route, per upstream, per source IP) reserve a slot; the
   reservations are held as RAII handles inside the ticket.
2. **Cost-filter pass.** `cost_filter::filter_chain` runs against
   `resolved.chain` using `route_entry` (read from
   `snapshot.config.routes` by the caller, preserving the routes-table
   reload split — see §1) for route-level cost caps. Survivors keep
   their original chain order. An empty result yields
   `AdmissionError::NoEligibleUpstream` and the rate-limit reservations
   release on Drop.
3. **Capability gate.** Skipped under the byte-identity dialect rule
   in §3 below; otherwise checks `chain[0].provider`'s capabilities
   against the canonical request and yields
   `AdmissionError::CapabilityMismatch` on miss.

On success the ticket carries the filtered chain, an
`Arc<ResolvedRoute>`, the rate-limit reservation handles, and (in PR 2
of the landing sequence) per-element breaker holds. The ticket
exposes:

```rust
pub fn chain(&self) -> &[BackendTarget];
pub fn consume(&self);   // idempotent
```

`consume()` is called by the caller at first byte from the upstream
(see §2 below for the trigger). The rate-limit and breaker buckets are
decremented at that point. Dropping the ticket without `consume()`
releases every slot.

`ResilientCaller::complete` collapses to a single signature:

```rust
async fn complete(
    &self,
    ticket: AdmissionTicket,
    req: CanonicalRequest,
) -> Result<CanonicalStream, ResilientCallerError>;
```

`complete_with_cost_filter`, `CostFilterInputs<'a>`, and `PolicyVec<T>`
are removed. Retry and breaker policy live on the ticket's
`ResolvedRoute` and are cloned per chain element internally.

**Trade-offs considered.** A "thick value, no RAII" variant required
the caller to explicitly call `release_on_failure(ticket)` on every
error path; we rejected it because forgotten releases become budget
drift bugs that surface only under load. A "thin ticket + separate
ResolvedRoute" variant kept the call sites lighter but left
`ResilientCaller` responsible for rate-limit and breaker reservations,
which is the precise split that produced the KNOWN GAP on the
passthrough path — the design only works if a single Module owns both
the reservation and the chain it reserved against.

### (3) `consume()` fires at first byte from the upstream

Rate-limit and breaker budgets are deducted when the upstream's first
byte arrives, not at admit time and not at stream end. Caller-side
cancellations after first byte still count, matching the v0.4
`ResilientCaller` semantics.

**Rationale.** These gates protect upstreams, not user fairness. A
request rejected by capability mismatch, cost filter, or auth before
any upstream packet is sent should not deduct upstream-protection
budget. Conversely, an upstream that returned a 503 to byte 1 already
cost the upstream; deducting at stream end would let a fast-failing
upstream poison its own retry budget with no signal that the request
ever ran.

**Trade-offs considered.** "Consume at admit time, refund on failure"
was rejected because every failure path (capability mismatch, cost
filter, provider error, client cancellation, plugin abort) must
remember to refund, and any single forgotten path is a budget
arithmetic bug. "Consume at stream completion" was rejected because
upstream A failing and `ResilientCaller` falling back to upstream B
would only deduct once for B, hiding A's load on the upstream-side
metrics that operators rely on for capacity planning.

### (4) Capability gate is skipped on byte-identity dialect match

When the inbound frontend dialect equals the chain head's provider's
native dialect, `Admission::admit` skips the capability gate. Today
this covers two dialect-identity pairs:

- `FrontendKind::AnthropicMessages` → `anthropic` provider, with the
  passthrough path active.
- `FrontendKind::OpenAiResponses` → `openai_compatible` provider with
  a Responses-shape upstream and `try_proxy_raw` set.

The assumption is that a provider speaking its native dialect is a
capability superset of that dialect's frontend: any content the
frontend can decode, the provider can re-emit. This holds today.

**Why skip and not gate.** The passthrough path deliberately does not
decode the body, so `request_has_image(req)` is not available without
a partial-decode prepass that would also re-encode and lose the
byte-identity guarantee ADR-0001 protects. The alternatives were:

- "Partial-decode to extract capability-relevant fields, then continue
  passthrough" — rejected because it adds a second body parse with no
  caller benefit and complicates the byte-identity proof.
- "Force a full decode/re-encode so the cap gate always runs" —
  rejected as a direct ADR-0001 conflict; this is exactly the round-
  trip the hybrid path exists to avoid.

The assumption is documented in CONTEXT.md (`Byte-identity dialect
skip`) so the next person who teaches a provider a feature its native
frontend doesn't have can find this decision and reopen it.

### (5) ADR-0001 is honored

The passthrough path remains byte-identical end-to-end. Admission
does not decode, does not parse, does not re-encode. Rate-limit and
breaker reservations are observation-only until `consume()`; they
allocate no bytes on the inbound body. The hybrid-path invariant
("same prompt routed through both paths produces semantically
equivalent output", ADR-0001 §Consequences) is unchanged because
admission is upstream of both paths and treats them symmetrically.

## Landing sequence

Three PRs, each independently reviewable and bisect-friendly:

1. **PR 1: `ResolvedRoute` in `agent-shim-router`.** Add
   `ModelResolver::resolve_route`; replace the three startup-frozen
   lookups in `pipeline.rs` (`Router::resolve`, `find_retry_policy`,
   `find_breaker_policy`) and the walker in `list_catalog`. Old
   `find_retry_policy` / `find_breaker_policy` methods removed in the
   same PR — no deprecation window. `find_route_entry` (the snapshot
   reader for cost-filter fields) is **untouched** in PR 1, preserving
   the routes-table reload split documented in CONTEXT.md.
2. **PR 2: `Admission` Module, canonical path only.** Introduce
   `Admission::admit` and `AdmissionTicket`. `admit` takes both a
   `ResolvedRoute` (startup-frozen route facts) and a
   `&RouteEntry` (read from `snapshot.config.routes` at call time by
   the pipeline, preserving the reload split). Rewrite
   `ResilientCaller::complete` to take a ticket. Remove
   `complete_with_cost_filter`, `CostFilterInputs<'a>`, `PolicyVec<T>`.
   Pipeline canonical path routes through admission; passthrough still
   uses the v0.7 short-circuit (KNOWN GAP comment preserved).
3. **PR 3: Passthrough on the ticket.** Pipeline `try_proxy_raw`
   branch calls `find_route_entry` then `Admission::admit`, then
   `proxy_raw`, then `ticket.consume()` on the first byte. Remove the
   L492–505 KNOWN GAP comment. The
   `crates/gateway/tests/rate_limit_per_key_envelope.rs` Anthropic
   test gets an Anthropic→Anthropic passthrough case to cover the
   path it previously had to route through OAI-compat to exercise.

## Consequences

- The `Router::find_retry_policy` / `find_breaker_policy` shape is
  gone; downstream code that depends on it (none today outside the
  gateway and the catalog) must move to `resolve_route`.
  `find_route_entry` survives as the snapshot reader for cost-filter
  fields and is the explicit seam between the two reload epochs.
- `ResilientCaller::complete` signature changes are not source-
  compatible; the gateway is the only caller, so the blast radius is
  contained to PR 2.
- The byte-identity dialect skip is a documented assumption, not a
  proof. If a future Anthropic vision feature is reachable through
  `FrontendKind::AnthropicMessages` but not yet plumbed into the
  Anthropic provider's `proxy_raw`, the assumption breaks and the cap
  gate has to either grow a partial-decode prepass or move into the
  provider's `proxy_raw` itself. The CONTEXT.md entry is the breadcrumb.
- `crates/core/` diff is zero against the v0.7 baseline. ADR-0007's
  discipline does not need to be invoked.
- Rate-limit and breaker metrics gain a new "reserved-but-not-consumed"
  signal that operators can use to detect capability/cost rejections
  upstream of the resilient caller. Whether to surface this as a
  separate counter is left to the Phase 8 design spec.
