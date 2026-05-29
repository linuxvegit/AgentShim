# AgentShim 0.9.0 — Release Notes

**Release date:** 2026-05-29
**Previous version:** [0.8.0](https://github.com/anthropics/agent-shim/releases/tag/v0.8.0)

## Summary

v0.9.0 ships one architectural deepening (ADR-0009: unified admission
gate) and one small upstream-facing feature (ADR-0010 acknowledged
trade-off + richer Copilot model metadata):

1. **Unified Admission Module.** Rate-limit, cost-filter, capability,
   and breaker gates are now composed behind one entry point
   (`Admission::admit`) that returns a single RAII `AdmissionTicket`.
   The pipeline canonical path AND the `try_proxy_raw` passthrough
   path both route through admission — no more "passthrough bypasses
   the gates" KNOWN GAP.
2. **First-byte consume rule.** Budget-deducting gates commit on
   first-byte-from-upstream, not at admit time. A request killed by
   capability/cost before any upstream packet is sent does not deduct
   upstream-protection budget. Cancellations after first byte still
   count.
3. **Rich Copilot `/models` metadata.** `agent-shim copilot models`
   now prints a fixed-width table with reasoning-effort values,
   thinking-budget shapes (adaptive vs static), and the full
   capability matrix. The data flows through the existing catalog
   surface automatically.
4. **ADR-0010: rate-limit reservation asymmetry accepted permanently.**
   The §3 design contract between `BreakerHold` (true RAII) and
   `RateLimitReservation` (observation-only) is honored at the
   interface level; the implementation gap on the rate-limit side
   stays as a bounded, documented trade-off.

## Upgrade notes

| Audience | Action |
|---|---|
| **Operators** | Drop in the new binary. All admission behavior changes are internal — every existing YAML config continues to load and behaves identically on the happy path. The only operator-visible change is the new `passthrough` log line carrying the same gate-derived rejection envelopes as the canonical path. |
| **`BackendProvider` implementors (internal crate consumers)** | No trait changes. `Admission` and `AdmissionTicket` are new types in `agent-shim-router`; existing `BackendProvider` impls don't see them. |
| **`agent-shim-router` consumers (custom dispatch loops, none today outside this repo)** | `ResilientCaller::complete` signature collapsed to `complete(ticket: AdmissionTicket, req: CanonicalRequest)`. Old `complete_with_cost_filter`, `CostFilterInputs<'a>`, and `PolicyVec<T>` are removed — call sites move to building a ticket via `Admission::admit` first. |
| **Plugin developers** | No plugin-API changes. The PluginRegistry, four hook anchors (H2/H3/H5/H7), and per-route hook plans are unchanged. The pipeline does honor plugin mutations on the H2/H3 anchors by skipping passthrough when plugins changed the canonical request — but plugins themselves see no API change. |

## What's new

### Unified Admission Module

Three building blocks land together, designed in
[ADR-0009](adr/0009-unified-admission.md) and shipped in three PRs:

#### 1. `ResolvedRoute` thin value

`ModelResolver::resolve_route(frontend, model)` now returns a single
`ResolvedRoute` carrying the full fallback chain, retry config, breaker
config, and route label as one cheap-to-clone Arc-wrapped value. The
previous three startup-frozen lookups (`Router::resolve`,
`find_retry_policy`, `find_breaker_policy`) are gone — replaced by a
single seam that downstream code reads from.

```rust
pub struct ResolvedRoute {
    pub chain: Vec<BackendTarget>,
    pub retry: RetryConfig,
    pub breaker: BreakerConfig,
    pub route_label: Arc<str>,
}
```

`find_route_entry` survives as the snapshot reader for hot-reloadable
cost-filter fields. This is intentional — the routes-table reload
split (some fields startup-frozen, others hot-reloadable) is documented
in CONTEXT.md and preserved here.

#### 2. `Admission::admit` + `AdmissionTicket`

```rust
pub fn admit(
    &self,
    resolved: ResolvedRoute,
    route_entry: &RouteEntry,
    config: &GatewayConfig,
    request: &CanonicalRequest,
    identity: &AgentIdentity,
    client_ip: &str,
    image_estimator: &dyn ImageTokenEstimator,
) -> Result<AdmissionTicket, AdmissionError>;
```

Inside `admit`: rate-limit pre-chain check, per-upstream rate-limit,
cost-filter pass (skipping over-budget chain elements), capability
gate against EVERY surviving chain element (not just chain[0] — closing
a v0.8 gap), and breaker holds via `BreakerRegistry::try_hold` for
each element. Rejections name the dimension that fired the gate, with
typed `AdmissionError` variants the pipeline maps to per-frontend
envelopes.

The returned `AdmissionTicket` is a RAII handle bundling:

- The cost-filter-survivor chain.
- An `Arc<ResolvedRoute>`.
- The `AgentIdentity` (so downstream resilience-event logs can render
  the real identity, not a placeholder).
- Per-element `BreakerHold`s (true RAII).
- Rate-limit reservations (observation-only by intent — see ADR-0010).

`AdmissionTicket::consume(chain_index, succeeded)` is called by
`ResilientCaller` (canonical path) or the passthrough byte-stream
wrapper (`try_proxy_raw` path) on first byte from upstream. The call
is idempotent; the selected chain element's breaker hold records
`succeeded`, all other holds are dropped as abandoned probes.

#### 3. Passthrough on the ticket

The `try_proxy_raw` branch in the pipeline now goes through
`Admission::admit` before `provider.proxy_raw`. The
`ConsumeOnFirstByteStream` wrapper consumes the ticket on first
`Poll::Ready(Some(Ok(_)))` (success) or `Poll::Ready(Some(Err(_)))`
(failure recorded against the breaker).

Plugin mutation safety guard: if H2 (`on_decoded_request`) or H3
(`on_resolved`) plugins mutate the canonical request, passthrough
is skipped (otherwise the byte body and the mutated canonical
diverge and upstream behavior drifts). The check is conditional on
the route actually having a subscriber for the anchor, so the
guard's deep-clone cost stays out of the hot path when no plugins
care.

The `crates/gateway/tests/rate_limit_per_key_envelope.rs` test now
exercises a true Anthropic→Anthropic passthrough route — the per-key
bucket triggers on the third request, proving the passthrough path
honors the same gate the canonical path does.

#### What got removed

- `Router::find_retry_policy` / `find_breaker_policy` methods.
- `ResilientCaller::complete_with_cost_filter`.
- `CostFilterInputs<'a>`.
- `PolicyVec<T>` length-invariant wrapper (replaced by ticket-internal
  per-element cloning).
- The KNOWN GAP comment block at `pipeline.rs` L469-482 explaining
  why passthrough bypassed rate-limit + breaker. Both gates now apply
  uniformly.

### First-byte consume rule

[ADR-0009 §3](adr/0009-unified-admission.md) names the rule:
budget-deducting gates commit on first-byte-from-upstream rather than
at admit time or at stream-end. The rationale:

- Gates protect upstreams, not user fairness. A request killed by
  capability, cost, or auth before any upstream packet is sent should
  not deduct upstream-protection budget.
- Cancellations after first byte still count (preserves the v0.4
  ResilientCaller semantics so a fast-failing upstream cannot poison
  its own retry budget without any signal).
- Stream errors on the first event count as failures — `StreamEvent::
  Error` in-band events as well as transport-level `Err`. The breaker
  records failure rather than succeeding when the upstream connects
  but immediately delivers an error chunk.

### Rich Copilot `/models` metadata

`agent-shim copilot models` now prints a fixed-width table:

```text
$ agent-shim copilot models
MODEL                FAMILY    CTXWIN     MAXOUT   EFFORT             ADAPT  V  T
claude-opus-4-7      claude    1000000    32000    adaptive            ✓    ✓  ✓
claude-sonnet-4-5    claude     200000    32000    adaptive            ✓    ✓  ✓
gpt-5.5              gpt        1048576    65536   low,medium,high            ✓  ✓
gemini-3-pro         gemini     1048576    32768   low,medium,high            ✓  ✓
```

`--format json` preserves the previous shape (now richer — every
new field is `serde(default, skip_serializing_if = "Option::is_none")`
so v0.8 catalog JSON clients see the new fields appear when the
upstream supplies them, never when it doesn't).

The new `ModelMetadata` fields:

- `supports.reasoning_effort_values: Option<Vec<String>>` — upstream-
  advertised effort list (`["low","medium","high","xhigh"]` on
  GPT-5/Gemini families; `None` on models without it).
- `adaptive_thinking: Option<bool>` — Anthropic family adaptive-
  thinking flag.
- `thinking_budget_{min,max}: Option<u32>` — Copilot's numeric range
  for budget-quantified models (1024–32000 for Claude, 128–32768
  for Gemini).

These fields flow through `GET /v1/models`, `GET /admin/catalog`, and
`agent-shim show-catalog` automatically — every catalog consumer
benefits without code changes.

### Code-review-driven fixes

Three Important findings surfaced by the post-PR-2 review landed in
PR 3 alongside the passthrough work:

- **Capability gate over the full chain.** Previously checked only
  `chain[0]`; now iterates every cost-filter survivor. A vision request
  whose chain head breaker is open no longer surfaces as an upstream
  4xx — admission rejects it as `CapabilityMismatch` before any
  upstream call.
- **First-byte stream-error discrimination.** The per-byte wrapper
  distinguishes `Ok(_)` from `Err(_)` (and `Ok(StreamEvent::Error)`)
  on the first event. Breaker records failure rather than success
  when the upstream's first event is an error.
- **Identity threading.** `AdmissionTicket` carries `AgentIdentity`
  through to `ResilientCaller`, restoring the real identity field
  (`anonymous` / `sha256:...`) on `request.completed` events instead
  of the placeholder `"admitted"`.

## Accepted trade-off: rate-limit reservation asymmetry

[ADR-0010](adr/0010-accept-rate-limit-asymmetry.md) cancels the
originally planned PR 4 of the ADR-0009 landing sequence. The
`governor` rate-limiter consumes on `check()` with no refund path,
so `RateLimitReservation::consume()` and `Drop` are no-ops. The
ticket's call to `reservation.consume()` on first byte is correct
at the interface level and intentionally a no-op at the
implementation level.

The bounded over-charge: one token per request that passes rate-limit
and is then rejected at cost-filter, capability gate, all-breakers-
open, or first-byte-fail. ADR-0010 names the revisit triggers that
would reopen the decision (operator-reported budget under-serving,
a test we can't write because of the asymmetry, a v0.9+ feature
needing reservation semantics beyond Drop-without-consume, or a
breaking `governor` change forcing a swap anyway).

## Workspace stats

|                       | v0.8.0 | v0.9.0 | Δ     |
|-----------------------|--------|--------|-------|
| Crates                | 11     | 11     |   0   |
| Source files          | ~281   | ~286   | +~5   |
| LoC (excluding tests) | ~24 k  | ~26 k  | +~2 k |
| Tests                 | ~1001  | ~1024  | +~23  |
| Clippy warnings       | 0      | 0      |   —   |

(Test count after the fix-up pass: the canonical `complete_records_
failure_on_empty_stream`, passthrough `admission_ticket_byte_stream`,
capability-gate `vision_capability_mismatch_rejects_text_only_
fallback_even_when_head_breaker_open`, plugin-registry
`subscriber_predicates_report_route_hook_presence`, and a passthrough
rate-limit envelope test all newly land in v0.9.)

## Documentation

- [`README.md`](../README.md) — top-level overview, capability matrix,
  reasoning effort + mapping examples, model catalog usage
- [`docs/configuration.md`](configuration.md) — full YAML reference
- [`docs/observability.md`](observability.md) — metrics + tracing
  surface
- [`CONTEXT.md`](../CONTEXT.md) — domain glossary, including new
  entries: "Admission", "Admission ticket", "First-byte consume rule",
  "Byte-identity dialect skip", "Routes-table reload split"
- [`CHANGELOG.md`](../CHANGELOG.md) — full per-feature changelog
- ADRs:
  - [`docs/adr/0009-unified-admission.md`](adr/0009-unified-admission.md)
    — the unified-admission design
  - [`docs/adr/0010-accept-rate-limit-asymmetry.md`](adr/0010-accept-rate-limit-asymmetry.md)
    — the accepted trade-off on the rate-limit side

## What's next

v0.10+ candidates (explicitly deferred from v0.9):

- **`POST /admin/discover` atomic catalog refresh.** Still a 501 stub;
  needs ArcSwap of ModelIndex to do safely under hot reload. Carried
  over from v0.8+ deferrals.
- **Learned realised-cost tracking (rolling EWMA).** Observed token
  counts feed back into the cost filter.
- **Distributed cost-filter state.** Multi-instance deployments
  share filter counts.
- **Distributed breaker / rate-limit state.**

ADR-0010's revisit triggers could also reopen reservation-aware rate-
limit work in a future release — but only on real signal, not on
sunk-cost grounds.

No removals, no breaking config changes planned for v0.10 — the
frozen-core invariant (ADR-0007) continues to apply to `crates/core/`.
