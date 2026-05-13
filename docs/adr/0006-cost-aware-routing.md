# ADR-0006 — Cost-Aware Routing: Static Tags + Four-Axis Filter Pass

**Status:** Accepted
**Date:** 2026-05-12
**Phase:** 6 (v0.6.0)
**Source design:** [`docs/superpowers/specs/2026-05-12-phase-6-cost-aware-gateway-design.md`](../superpowers/specs/2026-05-12-phase-6-cost-aware-gateway-design.md) §1, §4, §11

## Context

v0.6 closes the last v0.5 "Known limitations" entry: cost / latency-aware
routing. Operators running mixed upstream portfolios (e.g. Anthropic
premium + DeepSeek economy + Copilot subscription) need a declarative
way to constrain spend, latency, and tier selection per route without
bypassing the v0.4 [ResilientCaller](0004-resilient-caller.md) chain
walk. The design spec §1 frames the problem: the gateway already routes
on a static `(frontend, model_alias) → upstream` table, but operators
have no way to say "for this route, only spend up to $X per request" or
"only fall back to a same-or-higher tier upstream".

Three orthogonal choices pin the v0.6 design:

1. How is per-request cost represented and computed — static tags vs.
   learned, agent-driven hint vs. operator policy?
2. How does cost reasoning compose with the v0.4 fallback chain — does
   it re-sort upstreams, or only filter them?
3. Where in the request pipeline does the cost reasoning run, relative
   to the v0.4 ResilientCaller and the v0.5 rate-limit gate?

This ADR records each decision and the alternatives we rejected.

## Decision

We chose **static cost tags + four-axis filter pass** over the
candidates surveyed during the Phase 6 brainstorm.

### 1. Static configuration over learned / agent-driven hints

Every cost-relevant property is **operator-declared on the upstream**
(tier, per-token cost, p95 latency budget) or **on the route**
(`min_tier`, `max_cost_usd`). The estimator is a pure function over
`(CanonicalRequest, UpstreamCost)` using `tiktoken-rs`'s `cl100k_base`
encoder + the request's `max_tokens` ceiling. No state, no learning, no
header inspection.

**Rejected: learned EWMA.** A rolling EWMA over observed token counts
gives a more accurate "predicted cost" — but it's stateful (which
complicates reload semantics; see ADR-0005), it hides the policy
decision behind a moving target operators can't reason about, and it
optimises a problem operators don't have (the cap is a guardrail, not
a budget tracker).

**Rejected: agent-driven hints** (e.g. `agent-shim-budget: low`
header). The policy decision belongs to the operator running the
gateway, not the agent calling it. Letting agents self-declare their
budget tier creates an obvious trust hole.

### 2. Four orthogonal axes, applied as filters

The four axes:

1. **Tier label** — `economy` / `standard` / `premium` per upstream;
   `min_tier` per route. Operator-readable, no dollar amounts on a
   per-route basis required.
2. **Per-token cost** — `cost.input_per_million_usd` +
   `cost.output_per_million_usd` per upstream. Used together with the
   request's `max_tokens` to compute a pessimistic upper-bound cost
   estimate.
3. **Per-upstream p95 latency budget** — `p95_latency_budget_ms` per
   upstream. Compared against the v0.5
   `agent_shim_upstream_duration_seconds` histogram via a
   `LatencyProbe` trait.
4. **Per-route cost cap** — `max_cost_usd` per route. Estimated cost
   above the cap causes the upstream to be filtered out of that route's
   chain.

The four axes operate as **filters**, not as a re-sorter. The
operator's chain order from v0.4 is preserved. This is the
"filter-then-chain-order" decision from the brainstorm — picked
because it keeps operator intent visible and predictable.

**Rejected: re-sort the chain by predicted cost.** This breaks the
operator's explicit "preferred → fallback" ordering from v0.4. An
operator who put `claude-opus` first because it's the model they want
shouldn't see `claude-haiku` chosen on the basis of a cost estimate the
operator didn't ask the gateway to optimise.

**Rejected: per-axis enable flags.** All four axes are always
evaluated; an upstream that doesn't set `cost` simply doesn't have the
cap axis applied to it (Copilot subscription case). This keeps the
schema minimal and the operator's mental model uniform.

### 3. Filter sits between rate-limit gate and ResilientCaller chain walk

```
RateLimit → CostFilter → ResilientCaller (retry/breaker/fallback)
```

The filter pass runs:

- **after** the rate-limit gate (request admission already decided),
- **before** the v0.4 ResilientCaller's chain walk (filtered chain is
  what walks, so retry / fallback only see survivors).

When the filter empties the chain, the gateway returns
**HTTP 503 `NoEligibleUpstream`** with a JSON body listing each skipped
upstream and the per-axis reason. The response is rendered through the
inbound frontend's existing error envelope (Anthropic, OpenAI Chat,
OpenAI Responses) so agents see the failure in their native shape.

**Rejected: filter inside ResilientCaller's per-element loop.** That
would mean a failed retry triggers a re-evaluation of cost — the
estimate doesn't change request-to-request for the same chain element,
so re-running per element is pure overhead. Filter once, then walk.

**Rejected: filter as a router pre-pass.** The v0.4 router resolves
`(frontend, model) → Vec<BackendTarget>` purely by config; introducing
per-request cost evaluation there would conflate the model index with
runtime policy. Keeping the cost pass outside the router preserves
that crate's "no I/O, no runtime state" rule.

## Consequences

### Positive

- **Declarative.** Operators see exactly how cost decisions are made
  by reading `gateway.yaml`. No magic, no learned state.
- **Independently testable.** `cost_filter.rs` is a pure function over
  `(chain, route, request, config, probe)`. Each axis has positive +
  negative unit coverage.
- **Reuses v0.5 telemetry.** The latency axis is sourced from the
  existing `agent_shim_upstream_duration_seconds` histogram via a
  `LatencyProbe` trait; no new measurement infrastructure required.
- **Conservative estimate.** Cost estimate uses the request's
  `max_tokens` ceiling (default 4096) — operators set `max_cost_usd`
  with the natural semantic "reject if it COULD cost more than this".
- **`tier` is required.** No implicit default that silently
  misconfigures routing on a forgotten field.

### Negative

- **`tier` is a breaking config change.** Operators upgrading from
  v0.5 must add one line per upstream. CHANGELOG `[0.6.0]` calls this
  out as the single breaking change.
- **Cost estimate uses `tiktoken-rs`'s `cl100k_base` encoder**, which
  is a frontier-model tokeniser — not exact for every backend. The
  estimate is "good enough for filtering" but not "good enough for
  billing reconciliation".
- **One extra step in the request path.** The cost filter introduces
  a small synchronous chain traversal plus one tiktoken encoding pass
  (typically < 1ms). Negligible for typical request sizes but
  measurable for very short requests with very long chains.
- **`/metrics` text scrape per latency-probe query.** The Prometheus
  probe parses the full `/metrics` text on each query. The P04 quality
  review flagged this as a latency-overhead candidate; if measured
  problematic, swap to a direct histogram-handle clone in a v0.6.1
  follow-up.

### Trade-offs vs alternatives

- **Learned EWMA** — rejected as v0.6 scope. Stateful, complicates
  reload semantics, hides the policy decision. Static is enough for
  declarative cost management. v0.7+ candidate.
- **Agent-driven hints** (e.g. `agent-shim-budget: low` header) —
  rejected because the policy decision belongs to the operator, not
  the agent.
- **Re-sort the chain by predicted cost** — rejected because it
  breaks the operator's explicit chain order, which is the
  "preferred → fallback" intent from v0.4.
- **Filter inside ResilientCaller's per-element loop** — rejected
  because cost evaluation doesn't change request-to-request; once-per-
  request is correct.
- **Defer cost router to v0.7** — rejected because it would leave
  Phase 6 as a debt-payoff-only release, which doesn't match the
  "Phase N = one cohesive theme" cadence.

## Alternatives considered (full list)

- **A1: learned EWMA over observed token counts.** Rejected (§1).
- **A2: agent-driven hint header.** Rejected (§1).
- **A3: re-sort chain by predicted cost.** Rejected (§2).
- **A4: filter inside ResilientCaller's chain-element loop.** Rejected (§3).
- **A5: filter as a router pre-pass.** Rejected (§3).
- **A6: per-axis enable flags on the schema.** Rejected (§2 — axes are
  always evaluated; absent config just makes the axis a no-op for that
  upstream).
- **A7: defer cost routing to v0.7.** Rejected (§Trade-offs).

## P04 review findings deferred to v0.6.1

The Phase 6 P04 quality review surfaced five **Minor** items that we
chose not to address in v0.6 itself. They are recorded here so future
implementers see the deferrals and can pick them up:

- **M-1 — `FilterReason::TiktokenFallback` is `#[allow(dead_code)]`
  today.** The variant is defined for API stability and metric-label
  parity with the spec, but `filter_chain` never produces it because
  the `OnceLock`-initialised encoder + `encode_ordinary` handles
  arbitrary bytes gracefully. If a future stricter mode wants to
  surface encoder fallbacks, the variant is already in place; for v0.6
  it just earns the `dead_code` allow.
  *Closed in v0.6.1 (commit `dbd9bb3`).*

- **M-3 — Helper duplication between `crates/router/src/cost_filter.rs`
  and `crates/config/src/validation.rs`.** Both modules carry near-
  identical `upstream_cost / upstream_tier / upstream_latency_budget`
  match-on-variant helpers (rules 15-17 in validation, filter axes in
  cost_filter). If the duplication becomes annoying, a shared trait or
  extension impl on `UpstreamConfig` is the right refactor — but
  keeping it out of P04 was a conscious scope cut.
  *Closed in v0.6.1 (commit `a2cecbb`).*

- **M-6 — `metric-name-match` hand-maintenance.** The
  `crates/observability/src/metrics/names.rs` `all_unique` /
  `all_prefixed` tests rely on a hand-maintained array of every metric
  constant. The `COST_FILTERED_TOTAL` addition required appending to
  the array in two places; future metrics will too. Worth a derive
  macro / module-level introspection for v0.7.
  *Closed in v0.6.1 (commit `cfc8f22`).*

- **M-7 — Per-position policy-vec uniformity assumption.** The
  `ResilientCaller` chain walk re-aligns retry/breaker policy vectors
  by indexing into `vec![policy; chain.len()]` — uniform per route.
  After cost filtering, the chain length shrinks but the policy
  vectors are realigned to the survivor count. The assumption
  ("policy is uniform per route") is correct today but tightens the
  contract; if v0.7 introduces per-position policy overrides this
  alignment logic needs revisiting.
  *Closed in v0.6.1 (commit `e6864ef`).*

- **M-8 — Non-text content blocks ignored for cost estimation.**
  `cost_estimate.rs::canonical_request_text` only considers
  `ContentBlock::Text`; ToolCall / ToolResult / Image / Reasoning
  blocks are skipped. For text-only request shapes (the common case)
  this is correct; for image-heavy or tool-heavy traffic the estimate
  under-counts input tokens. If multimodal cost tracking becomes
  important, extend `canonical_request_text` (and probably move
  toward `tiktoken-rs`-aware estimation for tool schemas).
  *Closed in v0.6.1 (commit `9fc5aa5`).*

These items were individually small. None of them blocked v0.6 from
shipping; together they formed the agenda for v0.6.1, where all five
landed (P01 closed M-1 + M-3; P02 closed M-6; P03 closed M-7; P04
closed M-8). The v0.7 candidates listed in the "Open questions"
section below remain v0.7+ scope; they were never P04-deferred.

## Open questions

- **v0.7: realised-cost tracking.** Should the cost estimate move
  toward realised-cost tracking (rolling EWMA over observed token
  counts) to reduce false-positive filters when the estimate exceeds
  reality? Spec §10 risk table flags this as the most likely operator
  complaint.
- **v0.7: distributed cost-filter state.** Currently each gateway
  instance applies the filter independently. Multi-instance deployments
  behind a load balancer don't share filter counts. Out of scope here.
- **v0.7: agent-driven preferences as a separate, opt-in channel.**
  Operators may want a way to let trusted agents express preferences
  ("prefer-cheap" / "prefer-fast") that compose with — but never
  override — the operator's static filter. If demand surfaces, a
  scoped trusted-agent extension is the natural shape.

## Related ADRs

- [ADR-0004](0004-resilient-caller.md) — the v0.4 `ResilientCaller`
  chain walk that this filter sits on top of.
- [ADR-0005](0005-hot-reload-snapshot-model.md) — the v0.5 hot-reload
  snapshot model that P02's `ArcSwap<LimiterRegistry>` mirrors and
  whose snapshot boundary the v0.6 cost-filter config crosses.

## Changelog

This ADR is referenced from `CHANGELOG.md` under `[0.6.0]`.
