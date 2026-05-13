# Plan 05 — Docs, ADR-0006, CHANGELOG, version 0.5.0 → 0.6.0, smoke test (Phase 6)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-12-phase-6-cost-aware-gateway-design.md`](../specs/2026-05-12-phase-6-cost-aware-gateway-design.md) (§9 done-when, §10 risks, §11 locked decisions).

**Goal:** Ship v0.6.0. ADR-0006 records the cost-aware routing decisions; CHANGELOG documents the breaking `tier` requirement; README/docs gain a v0.6 capability row; version bumps from `0.6.0-dev` to `0.6.0`; end-to-end smoke test exercises all three v0.6 pillars in one flow.

**Architecture:** Mirrors Phase 5 P05 — pure docs + release ceremony, no production code change beyond the version bump and the smoke test.

**Tech stack:** No code changes other than the smoke test. No new dependencies.

**Frozen-core impact:** None — docs and version metadata.

**Test target:** 659 (after P04) → 660 (+1 integration: phase6_smoke.rs).

---

## File Structure

`docs/adr/`:
- Create: `0006-cost-aware-routing.md` — captures D1-D12 from spec §11 + rejected alternatives.

`docs/`:
- Modify: `observability.md` — remove "outbound traceparent deferred" + "rate-limit reload deferred" from Known limitations. Add cost-filter operator-facing notes.
- Modify: `configuration.md` — document the new `tier`, `cost`, `p95_latency_budget_ms`, `min_tier`, `max_cost_usd` fields and validation rules 15-18.
- Modify: `architecture.md` — add a "Cost-aware routing" subsection.
- Modify: `contributing.md` — add "How to add a new tier" or "How to add a routing axis" recipe if appropriate.

`README.md`:
- Modify: v0.5 capability row → v0.6 capability row (add "cost-aware" column or row). Update "What's NOT in v0.6" section.

`CHANGELOG.md`:
- Modify: add `[0.6.0]` entry with **Added** / **Changed** / **Breaking** / **Known limitations** / **Frozen-core invariant** sections.

`Cargo.toml`:
- Modify: workspace version `0.6.0-dev` → `0.6.0`.

`crates/gateway/tests/`:
- Create: `phase6_smoke.rs` — boot gateway with all 3 pillars active, hit endpoints, assert the three behaviours land together.

---

## Tasks

### Task 1: ADR-0006 — cost-aware routing model

**Files:**
- Create: `docs/adr/0006-cost-aware-routing.md`

- [ ] **Step 1: Write the ADR**

The ADR follows the established shape of ADR-0001 through ADR-0005. Open `docs/adr/0005-hot-reload-snapshot-model.md` to see the format, then create `docs/adr/0006-cost-aware-routing.md` with the following sections:

```markdown
# ADR-0006: Cost-aware routing — static tags + filter pass

**Status:** Accepted
**Date:** 2026-05-12
**Phase:** 6 / v0.6.0

## Context

[1-2 paragraphs on why cost-aware routing — operators running mixed
upstream portfolios need to constrain spend, latency, and tier
selection per route without bypassing the v0.4 fallback chain.]

## Decision

We chose **static cost tags + four-axis filter pass** over the
candidates surveyed during the Phase 6 brainstorm.

The four axes:

1. **Tier label** — `economy` / `standard` / `premium` per upstream;
   `min_tier` per route. Operator-readable, no dollar amounts on a
   per-route basis required.
2. **Per-token cost** — `cost.input_per_million_usd` + `cost.output_per_million_usd`
   per upstream. Used together with the request's max_tokens to
   compute a pessimistic upper-bound cost estimate.
3. **Per-upstream p95 latency budget** — `p95_latency_budget_ms` per
   upstream. Compared against the v0.5
   `agent_shim_upstream_duration_seconds` histogram via a
   `LatencyProbe` trait.
4. **Per-route cost cap** — `max_cost_usd` per route. Estimated cost
   above the cap causes the upstream to be filtered out of that
   route's chain.

The filter pass runs:

- **after** rate-limit gate (request admission already decided),
- **before** the v0.4 ResilientCaller's chain walk (filtered chain is
  what walks, so retry/fallback only see survivors).

When the filter empties the chain, the gateway returns
`HTTP 503 NoEligibleUpstream` with a JSON body listing each skipped
upstream and the per-axis reason.

The four axes operate as **filters**, not as a re-sorter. The
operator's chain order from v0.4 is preserved. This is the
"filter then chain-order" decision from the brainstorm — picked
because it keeps operator intent visible and predictable.

## Consequences

### Positive

- Operators can constrain cost spend declaratively, with no
  per-request hint headers needed (the cost is per-request from the
  estimator's perspective, but the configuration is static).
- The filter axes are independently testable — `cost_filter.rs` is a
  pure function over chain + route + probe + config.
- Reuses the v0.5 metrics histogram for the latency axis. No new
  measurement infrastructure.
- Cost estimate is conservative — operators set `max_cost_usd` with
  the natural semantic "reject if it COULD cost more than this".
- `tier` is required on every upstream → no implicit defaults that
  silently misconfigure routing.

### Negative

- `tier` becomes a breaking config change. Operators upgrading from
  v0.5 must add one line per upstream. CHANGELOG calls this out.
- The cost estimate uses `tiktoken-rs`'s `cl100k_base` encoder, which
  is a frontier-model tokenizer — not exact for every backend. The
  estimate is "good enough for filtering" but not "good enough for
  billing reconciliation".
- The filter pass introduces one extra step in the request path; the
  per-request overhead is a small synchronous traversal of the chain
  plus one tiktoken encoding pass (typically < 1ms).
- p95 reads parse the entire `/metrics` text scrape per query. P04 T7
  reviewer flagged this as a latency overhead candidate; if measured
  problematic, swap to a direct histogram-handle clone in a v0.6.1
  follow-up.

### Trade-offs vs alternatives

- **Learned EWMA** — rejected as v0.6 scope. Stateful, brings reload
  semantics complications, hides the policy decision. Static is
  enough for declarative cost management.
- **Agent-driven hints** (e.g. `agent-shim-budget: low` header) —
  rejected because the policy decision belongs to the operator, not
  the agent.
- **Re-sort the chain by predicted cost** — rejected because it
  breaks the operator's explicit chain order, which is the
  "preferred → fallback" intent from v0.4.
- **Per-bucket replace_policy hook in LimiterRegistry** — rejected
  for the rate-limit reload mechanism in favour of `ArcSwap` (P02).
  governor doesn't expose retuning anyway.
- **Defer cost router to v0.7** — rejected because it would leave
  Phase 6 as a debt-payoff-only release, which doesn't match the
  "Phase N = one cohesive theme" cadence.

## Alternatives considered (full list)

[Same content as the brainstorm session's option tables in the spec
§11 + the rejected items list from the cost-routing-shape question.]

## Open questions

- v0.7: should the cost estimate move toward realised-cost tracking
  (rolling EWMA over observed token counts) to reduce false-positive
  filters when the estimate exceeds reality? Spec §10 risk table
  flags this as the most likely operator complaint.
- v0.7: distributed cost state (shared filter counter across N gateway
  replicas) — currently each instance applies the filter
  independently. Out of scope here.

## Related ADRs

- [ADR-0004](0004-resilient-caller.md) — the v0.4 ResilientCaller
  chain walk that this filter sits on top of.
- [ADR-0005](0005-hot-reload-snapshot-model.md) — the v0.5 hot-reload
  model that P02's ArcSwap<LimiterRegistry> mirrors.
```

The ADR ends up around 200-300 lines of prose. Keep it operator-facing.

- [ ] **Step 2: Commit**

```bash
git add docs/adr/0006-cost-aware-routing.md
git commit -m "docs(adr): cost-aware routing model — ADR-0006 (Plan 05 P05 T1)"
```

---

### Task 2: Update `docs/observability.md`

**Files:**
- Modify: `docs/observability.md`

- [ ] **Step 1: Remove the two v0.6 deferral notices**

Search the file for the v0.5 "Known limitations" section. Two bullets need to be **removed** or **rewritten**:

1. "Outbound `traceparent` propagation deferred to v0.6" — REMOVE. Add a new paragraph under the OTel section confirming inbound + outbound trace continuation work end-to-end.
2. "Rate-limit policy changes are inert until restart" — REMOVE. Add a new paragraph confirming reload now takes effect on the next request, with the spec §5.4 bucket-reset caveat (the existing bucket is replaced; that's the documented governor limitation).

- [ ] **Step 2: Add a "Cost-aware routing" operator section**

Append to `docs/observability.md` a new top-level section:

```markdown
## Cost-aware routing (v0.6)

Phase 6 adds four orthogonal axes the operator can apply per route:

1. **Tier label** — `tier: economy | standard | premium` on every
   upstream. Routes can declare `min_tier`. Upstreams below the
   route's `min_tier` are filtered out.
2. **Per-token cost** — `cost: {input_per_million_usd, output_per_million_usd}`
   on an upstream. Combined with the request's input token count
   estimate (via `tiktoken-rs`) and `max_tokens` ceiling, this
   produces a per-request cost estimate.
3. **Latency budget** — `p95_latency_budget_ms` on an upstream. The
   cost filter compares the budget against the recent p95 from
   `agent_shim_upstream_duration_seconds`.
4. **Per-route cost cap** — `max_cost_usd` on a route. Upstreams
   whose estimated cost exceeds this are filtered out.

When every upstream in a route's chain is filtered, the gateway
returns **HTTP 503 NoEligibleUpstream** with a body listing each
skipped upstream and the per-axis reason.

### Metrics

`agent_shim_cost_filtered_total{reason, upstream, route}` — per-axis
counter. Reasons:

| reason | meaning | upstream skipped? |
|---|---|---|
| `tier` | upstream.tier < route.min_tier | yes |
| `latency` | recent p95 > route.p95_latency_budget_ms | yes |
| `cap` | estimated cost > route.max_cost_usd | yes |
| `latency_unknown` | probe has no samples yet | **no** (passes through) |
| `tiktoken_fallback` | tiktoken-rs encode failure → heuristic estimate | **no** (passes through) |

### Tuning

- `tier` is **required** on every upstream. There is no default.
- Cost estimates are intentionally pessimistic (upper bound). A
  filter rate that surprises you is usually a sign the cap is set
  too tightly for the request shape, not that the gateway is
  miscalibrated.
- The latency probe parses the `/metrics` scrape on each query.
  For sub-millisecond latency-sensitive deployments, watch for the
  parse overhead in `agent_shim_request_duration_seconds`.
```

- [ ] **Step 3: Commit**

```bash
git add docs/observability.md
git commit -m "docs(observability): cost-aware routing operator section + remove v0.5 deferrals (Plan 05 P05 T2)"
```

---

### Task 3: Update `docs/configuration.md` + `architecture.md`

**Files:**
- Modify: `docs/configuration.md`
- Modify: `docs/architecture.md`

- [ ] **Step 1: Add cost fields to configuration.md**

Append to `docs/configuration.md` a new section:

```markdown
## Cost-aware routing fields (v0.6)

### Upstream-level

| Field | Type | Required | Description |
|---|---|---|---|
| `tier` | `economy / standard / premium` | **yes** | Service tier label. Routes can require `min_tier`. |
| `cost.input_per_million_usd` | `f64` | no | USD per million input tokens |
| `cost.output_per_million_usd` | `f64` | no | USD per million output tokens |
| `p95_latency_budget_ms` | `u64` | no | Maximum allowed p95 latency in milliseconds |

### Route-level

| Field | Type | Required | Description |
|---|---|---|---|
| `min_tier` | `economy / standard / premium` | no | Minimum upstream tier accepted by this route |
| `max_cost_usd` | `f64` | no | Per-request cost cap (estimate > cap → upstream filtered) |

### Validation rules 15-18

- **Rule 15** — `cost.*` fields must be non-negative when present
- **Rule 16** — `tier` must be `economy`, `standard`, or `premium`
- **Rule 17** — Every route with `min_tier` must have at least one
  upstream in its chain that meets `min_tier`. Startup + reload.
- **Rule 18** — `tier`, `cost`, `p95_latency_budget_ms`, `min_tier`,
  `max_cost_usd` are all reloadable. Changing any of these via
  `/admin/reload` takes effect on the next request.
```

- [ ] **Step 2: Add architecture.md subsection**

In `docs/architecture.md`, after the v0.5 "Observability layer" section, add:

```markdown
### Cost-aware routing (v0.6)

The Phase 6 cost filter sits BETWEEN the rate-limit gate and the
ResilientCaller chain walk. It's a pure pass over the
operator-defined fallback chain:

```
RateLimit → CostFilter → ResilientCaller (retry/breaker/fallback)
```

The filter applies four independent axes (tier / latency / cap /
estimated cost) as constraints, not as a re-sorter. Survivors keep
their original chain order — the operator's "preferred upstream"
intent from v0.4 is preserved.

Latency data is sourced from a `LatencyProbe` trait, with the
Prometheus-backed implementation in `crates/gateway/src/latency_probe.rs`
reading the `agent_shim_upstream_duration_seconds` histogram. The
trait lives in router; the impl lives in gateway, keeping router
free of an observability crate dependency.

When the filter empties the chain, the gateway short-circuits with
HTTP 503 `NoEligibleUpstream` before any provider is contacted.

See [ADR-0006](adr/0006-cost-aware-routing.md) for design context.
```

- [ ] **Step 3: Commit**

```bash
git add docs/configuration.md docs/architecture.md
git commit -m "docs: configuration.md + architecture.md gain cost-aware routing (Plan 05 P05 T3)"
```

---

### Task 4: README capability row + "What's NOT in v0.6"

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Bump capability matrix to v0.6**

In `README.md`, find the "Capability matrix (v0.5)" header. Change to "Capability matrix (v0.6)". Add a new row at the bottom (or update the existing observability row to include cost-aware semantics):

```markdown
| Cost-aware routing (v0.6) | tier filter · latency budget · cost cap · estimated cost | ✓ | ✓ | ✓ | ✓ | ✓ |
```

If the column header doesn't fit, just add a 1-line summary paragraph under the matrix explaining cost-aware routing applies equally to every provider.

- [ ] **Step 2: Update "What's NOT in v0.6"**

The v0.5 section "What's NOT in v0.5" needs to be renamed to "What's NOT in v0.6". Remove the two items that v0.6 closes:

- "Outbound `traceparent` propagation" — REMOVE
- "Rate-limit policy effect on reload" — REMOVE

Add cost-router non-features:

- "Learned realised-cost tracking (rolling EWMA)" — v0.7+
- "Distributed cost-filter state" — v0.7+
- "Agent-driven routing hints" — explicitly out of scope, rationale in ADR-0006

- [ ] **Step 3: Update Releases trail**

In README's `## Releases` section, add v0.6.0 as the new "Current" entry. Move v0.5.0 to "Previous". Format:

```markdown
**v0.6.0** — Phase 6: cost-aware gateway — outbound traceparent,
rate-limit reload, four-axis cost filter (tier/cost/latency/cap),
HTTP 503 NoEligibleUpstream
([ADR-0006](docs/adr/0006-cost-aware-routing.md)).
```

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs(readme): v0.6 capability row + What's NOT in v0.6 (Plan 05 P05 T4)"
```

---

### Task 5: CHANGELOG `[0.6.0]` entry

**Files:**
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Add the entry**

Add immediately above the existing `[0.5.0]` entry:

```markdown
## [0.6.0] — 2026-05-12

Phase 6 release: the gateway becomes **cost-aware**. Three subsystems
close: outbound traceparent propagation, hot-reloadable rate-limit
policy, and four-axis cost-aware routing. The two v0.5 "Known
limitations" are resolved.

### Added

#### Outbound `traceparent` propagation (Plan 01)

- New `agent_shim_providers::ProviderHttpClient` wraps every
  provider's `reqwest::Client`. Auto-injects W3C `traceparent` (and
  `tracestate`) from `Span::current()`'s OTel context on every
  outgoing request. End-to-end distributed traces work.
- New `agent_shim_observability::inject_context_into_headers` helper.

#### Rate-limit reload (Plan 02)

- `AppCore::limiter_registry` moves to
  `Arc<ArcSwap<LimiterRegistry>>`. The reload-applying task swaps
  it atomically after the `AppSnapshot` swap.
- A reload that changes `rate_limit.*` takes effect on the very
  next request, no process restart needed.

#### Cost-aware routing (Plan 03 + Plan 04)

- Four routing axes per route:
  - `tier` filter (`economy` / `standard` / `premium`)
  - `cost` per token (USD-denominated)
  - `p95_latency_budget_ms` per upstream
  - `max_cost_usd` per route
- New `crates/router/src/cost_filter.rs` runs as a pre-chain-walk
  pass inside `ResilientCaller`. Pure function over (chain, route,
  request, config, probe).
- New `LatencyProbe` trait in router; Prometheus-backed impl
  (`PrometheusLatencyProbe`) in gateway. Reads
  `agent_shim_upstream_duration_seconds` histogram via the
  metrics-exporter-prometheus snapshot.
- `tiktoken-rs` (already a v0.3 workspace dep) drives input-token
  estimation; the request's `max_tokens` (default 4096) drives
  output-token estimation. The estimate is intentionally pessimistic.
- When the filter empties the chain, the gateway returns
  HTTP 503 `NoEligibleUpstream` with a JSON body listing each skipped
  upstream and the per-axis skip reason.
- New metric `agent_shim_cost_filtered_total{reason, upstream, route}`
  with reasons `tier / latency / cap / latency_unknown / tiktoken_fallback`.
- Validation rules 15-18 added (configuration.md documents).

### Changed

- **`AppCore::limiter_registry` field type** —
  `Arc<LimiterRegistry>` → `Arc<ArcSwap<LimiterRegistry>>`. Two
  integration tests that construct `AppCore` directly (from v0.5
  P01) need to wrap their `LimiterRegistry::disabled()` in
  `ArcSwap::from_pointee(...)`.
- **`ResilientCaller::new` signature** — now takes an extra
  `Arc<dyn LatencyProbe>` argument. Tests that construct it
  directly should pass `Arc::new(DisabledLatencyProbe)`.

### Breaking

⚠ **`tier` is required on every upstream.** Configs without a
`tier:` line on every upstream will fail to parse at startup.

**Upgrade action:** add one line per upstream in `gateway.yaml`:

```yaml
upstreams:
  my_upstream:
    # existing fields...
    tier: standard           # or economy / premium
```

This is the only breaking change in v0.6.

### Frozen-core invariant changes

The Phase 5 invariant covered `crates/core/`, `crates/frontends/`,
and `crates/providers/src/`. Phase 6 narrows it:

- **Phase 6 invariant** — empty diff against v0.5.0 baseline
  (master HEAD `44749de`) for `crates/core/` and `crates/frontends/`.
- **`crates/providers/src/` is unfrozen for Phase 6.** P01 adds one
  new module (`http_client.rs` modified — promoted from
  `pub(crate)` to `pub`, returns `ProviderHttpClient` instead of
  `reqwest::Client`) and a `client: ProviderHttpClient` field
  rename on five provider mods. No other diff hunks. See
  [ADR-0006](docs/adr/0006-cost-aware-routing.md) §2 for the
  discipline rule and the Phase 6 audit.

### Deprecated

None.

### Fixed

None — this release is additive.

### Documentation

- New [`docs/adr/0006-cost-aware-routing.md`](docs/adr/0006-cost-aware-routing.md)
  records the four-axis decision + rejected alternatives.
- [`docs/observability.md`](docs/observability.md) gains a "Cost-aware
  routing" operator section; removes the two v0.5 deferrals from
  Known limitations.
- [`docs/configuration.md`](docs/configuration.md) documents the
  five new schema fields and rules 15-18.
- [`docs/architecture.md`](docs/architecture.md) adds a "Cost-aware
  routing" subsection.

### Known limitations

- **Estimate is not realised cost.** The cost filter uses an upper
  bound from `tiktoken-rs` + `max_tokens`. Realised costs are
  usually lower; the operator-set `max_cost_usd` should be tuned with
  this semantic in mind ("reject if it COULD cost more than $X").
- **Single-instance state still applies.** Cost-filter counts are
  per-process; distributed cost-aware behaviour across N gateway
  replicas remains a v0.7+ candidate.
- **Tokeniser mismatch.** `tiktoken-rs`'s `cl100k_base` encoder is a
  frontier-model tokeniser and won't perfectly match every backend.
  Estimates are "good enough for filtering", not "good enough for
  billing reconciliation".
```

- [ ] **Step 2: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs(changelog): [0.6.0] entry (Plan 05 P05 T5)"
```

---

### Task 6: Workspace version bump 0.6.0-dev → 0.6.0

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

- [ ] **Step 1: Bump the workspace version**

In `Cargo.toml`:

```toml
version = "0.6.0"
```

- [ ] **Step 2: Refresh Cargo.lock**

```bash
cargo build --workspace 2>&1 | tail -5
```

Expected: clean. Cargo.lock auto-updates to `0.6.0`.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: bump workspace version 0.6.0-dev → 0.6.0 (Plan 05 P05 T6)"
```

---

### Task 7: End-to-end Phase 6 smoke test

**Files:**
- Create: `crates/gateway/tests/phase6_smoke.rs`

- [ ] **Step 1: Write the smoke test**

```rust
//! Plan 06 P05 T7: end-to-end smoke test exercising all three v0.6
//! pillars in one flow:
//!
//!   1. Outbound traceparent injection (P01) — verified via mockito
//!      matcher.
//!   2. Hot-reload of rate_limit (P02) — verified by triggering a
//!      reload mid-test and observing the next request is 429.
//!   3. Cost filter (P04) — verified by submitting a request that
//!      hits the `max_cost_usd` cap and gets 503.
//!
//! This is the v0.6 sibling of v0.5's `phase5_smoke.rs`.

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

async fn pick_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
        .local_addr().unwrap().port()
}

#[tokio::test]
async fn phase6_pillars_compose() {
    let public = pick_port().await;
    let admin = pick_port().await;

    // Mockito upstream — accepts the request, asserts outbound traceparent.
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/v1/chat/completions")
        .match_header(
            "traceparent",
            mockito::Matcher::Regex(
                r"^00-[0-9a-f]{32}-[0-9a-f]{16}-(00|01)$".to_string(),
            ),
        )
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("data: [DONE]\n\n")
        .create_async()
        .await;

    let initial_yaml = format!(
        r#"
server: {{bind: 127.0.0.1, port: {public}}}
admin: {{bind: 127.0.0.1, port: {admin}}}
metrics: {{enabled: true}}
upstreams:
  m:
    type: open_ai_compatible
    base_url: {}
    api_key: dummy
    tier: standard
    cost:
      input_per_million_usd: 1.0
      output_per_million_usd: 1.0
routes:
  - frontend: openai_chat
    model: x
    upstream: m
    upstream_model: x
"#,
        upstream.url(),
    );

    let cfg: agent_shim_config::GatewayConfig =
        serde_yaml::from_str(&initial_yaml).unwrap();
    let (state, mut reload_rx) = agent_shim_gateway::state::AppState::new(cfg).await;
    let state_for_task = state.clone();
    tokio::spawn(async move {
        while let Some(req) = reload_rx.recv().await {
            let out = agent_shim_gateway::commands::serve::handle_reload(
                &state_for_task,
                req.source,
            )
            .await;
            let _ = req.respond_to.send(out);
        }
    });

    let public_addr: SocketAddr = format!("127.0.0.1:{public}").parse().unwrap();
    let admin_addr: SocketAddr = format!("127.0.0.1:{admin}").parse().unwrap();
    let pl = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let al = tokio::net::TcpListener::bind(admin_addr).await.unwrap();
    let pa = agent_shim_gateway::server::build_router(state.clone());
    let aa = agent_shim_gateway::admin::build_router(state);
    tokio::spawn(async move { let _ = axum::serve(pl, pa).await; });
    tokio::spawn(async move { let _ = axum::serve(al, aa).await; });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    let body = r#"{"model":"x","messages":[{"role":"user","content":"hi"}],"stream":true}"#;

    // Pillar 1: outbound traceparent. The first request goes through;
    // mockito's match_header fires its matcher.
    let r1 = client
        .post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    // 200 is the mockito-served happy path; if the route goes through
    // any 4xx/5xx (e.g. SSE parse, capability gate), the test is still
    // valid as long as the outbound was made — the mock.assert() is
    // the real check.
    assert!(r1.status().is_success() || r1.status().is_server_error());
    mock.assert_async().await;

    // Pillar 2: rate-limit reload. Reload with rate_limit { burst=1 }.
    let updated_yaml = initial_yaml.replace(
        "metrics: {enabled: true}",
        "metrics: {enabled: true}\nrate_limit:\n  enabled: true\n  per_route:\n    \"openai_chat/x\":\n      rate_per_sec: 1\n      burst: 1",
    );
    let r2 = client
        .post(format!("http://{}/admin/reload", admin_addr))
        .header("content-type", "application/yaml")
        .body(updated_yaml)
        .send()
        .await
        .unwrap();
    assert_eq!(r2.status(), 200);

    // First post-reload request consumes the token.
    let r3 = client.post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .body(body)
        .send().await.unwrap();
    assert_ne!(r3.status(), 429);

    // Second post-reload request must be 429.
    let r4 = client.post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .body(body)
        .send().await.unwrap();
    assert_eq!(r4.status(), 429,
        "rate-limit reload didn't take effect; got {}", r4.status());

    // Pillar 3: cost filter. Reload again with extreme cost cap.
    let extreme_yaml = initial_yaml.replace(
        "upstream: m\n    upstream_model: x",
        "upstream: m\n    upstream_model: x\n    max_cost_usd: 0.0000001",
    );
    let r5 = client
        .post(format!("http://{}/admin/reload", admin_addr))
        .header("content-type", "application/yaml")
        .body(extreme_yaml)
        .send()
        .await
        .unwrap();
    assert_eq!(r5.status(), 200);

    // Now ANY request should be filtered out by the cap.
    // Sleep enough to let the rate-limit bucket from the earlier
    // pillar (if reload reset it back to disabled) recover, OR allow
    // for the 429 below. Just check for either 503 (cost cap fired)
    // OR 429 (rate-limit bucket hadn't recovered yet).
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let r6 = client.post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .body(body)
        .send().await.unwrap();
    assert_eq!(r6.status(), 503,
        "cost filter didn't fire; got {}", r6.status());

    // Quick sanity: /metrics shows agent_shim_cost_filtered_total{reason="cap"}.
    let metrics_body = client.get(format!("http://{}/metrics", admin_addr))
        .send().await.unwrap()
        .text().await.unwrap();
    assert!(metrics_body.contains("agent_shim_cost_filtered_total"),
        "cost_filtered_total must be present after a filter event");
}
```

The above test is **realistically fragile** — combining three pillars in one test is a smoke test by nature. The implementer should:

- Iterate to get each pillar individually passing inline before composing them.
- If one pillar's interaction with another causes flakes (e.g. the rate-limit bucket from pillar 2 still has a token when pillar 3 reloads), simplify the test to either skip the rate-limit assertion in this combined test (covered already by `reload_rate_limit_buckets_reset.rs`) or shape the reload sequence to avoid interaction.

The accepted simplification: keep pillars 1 and 3 in this smoke test; skip pillar 2 reload assertion (covered by the dedicated P02 T5 integration). The smoke test then becomes:

1. Outbound traceparent works
2. Cost filter 503 works
3. Metrics body has the relevant counter

That's the actual `phase6_smoke.rs` shape — simpler and reliable.

- [ ] **Step 2: Run the test**

```bash
cargo test -p agent-shim --test phase6_smoke 2>&1 | tail -10
```

Expected: 1 passed.

- [ ] **Step 3: Workspace total**

```bash
cargo test --workspace --quiet 2>&1 | tee /tmp/p05.out > /dev/null
grep -E "^test result:" /tmp/p05.out | awk -F'[ ;]' '{sum+=$4} END {print "TOTAL:", sum}'
rm /tmp/p05.out
```

Expected: 660 (was 659 after P04, +1 smoke).

- [ ] **Step 4: Commit**

```bash
git add crates/gateway/tests/phase6_smoke.rs
git commit -m "test(gateway): end-to-end Phase 6 smoke (Plan 05 P05 T7)"
```

---

### Task 8: Final pre-merge sweep

- [ ] **Step 1: Workspace clippy + fmt clean**

```bash
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -10
cargo fmt --all -- --check 2>&1 | tail -10
```

Both must be clean.

- [ ] **Step 2: Workspace test pass**

```bash
cargo test --workspace --quiet 2>&1 | grep -E "^test result:" | grep -v "0 failed" | head -5
echo "---"
echo "Any failures? (empty = all green)"
```

Expected: empty.

- [ ] **Step 3: Frozen-core diff**

```bash
git diff a0139fc -- crates/core/ crates/frontends/
```

Expected: empty.

- [ ] **Step 4: Providers diff audit**

```bash
git diff a0139fc -- crates/providers/src/ | head -100
```

Expected: only hunks falling into the three categories from spec §2.2:

1. New/modified `http_client.rs` (P01 T2)
2. One-line `lib.rs` re-export (P01 T1)
3. Mechanical `client: reqwest::Client` → `client: ProviderHttpClient` field renames in 5 provider mods (P01 T3)

If any other diff hunk is present, flag it.

- [ ] **Step 5: If fmt or clippy drift, commit a fmt sweep**

```bash
cargo fmt --all
git status -s
git add -u
git commit -m "style: cargo fmt sweep after Phase 6 close (Plan 05 P05 T8)"
```

---

### Task 9: Spec compliance + code quality review

- [ ] **Step 1: Reviewer dispatch (spec compliance)**

> Final spec compliance review for Phase 6. Read `docs/superpowers/specs/2026-05-12-phase-6-cost-aware-gateway-design.md` and the full Phase 6 commit log:
>
> ```bash
> git log master..HEAD --oneline
> ```
>
> Verify EVERY item in spec §9 "Done when":
>
> - [ ] Workspace test count ≥ 660 — quote the actual number.
> - [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
> - [ ] `cargo fmt --all -- --check` clean.
> - [ ] `git diff a0139fc -- crates/core/ crates/frontends/` empty.
> - [ ] `git diff a0139fc -- crates/providers/src/` classifies — quote each hunk and bin it into the three allowed categories.
> - [ ] Outbound traceparent injected at all 5 providers (mockito-verified for at least 2).
> - [ ] Reload of `rate_limit.*` works on next request (test passes).
> - [ ] CostFilter has positive + negative unit coverage for all 4 axes.
> - [ ] `agent_shim_cost_filtered_total{reason,upstream,route}` exposed.
> - [ ] CHANGELOG's "Breaking" section calls out `tier`.
> - [ ] ADR-0006 lands with rejected-alternatives section.
> - [ ] Version is `0.6.0`.
> - [ ] v0.5 "Known limitations" items removed from README + observability.md + CHANGELOG running list.

- [ ] **Step 2: Reviewer dispatch (code quality)**

> Final code quality review for Phase 6. Read every commit:
>
> ```bash
> git log master..HEAD --oneline
> ```
>
> Audit for integration drift across plans:
>
> 1. Cross-plan API: any name renamed in one plan that left stale references in another? (Phase 5 had `handle_reload_for_test` → `handle_reload` as a classic example. Watch for similar in P03/P04.)
> 2. `#[allow(dead_code)]` markers — any that should fall off now that downstream consumers exist?
> 3. Helper duplication: P03's `upstream_cost/tier/latency_budget` helpers in validation.rs vs P04's same helpers in cost_filter.rs — DRY violation or intentional?
> 4. Test isolation: every new test uses `MockLatencyProbe::with(...)` or `DisabledLatencyProbe`. None should accidentally read the global metrics handle.
> 5. Frontend error envelope: confirm 503 NoEligibleUpstream renders as the right JSON shape for Anthropic, OpenAI Chat, AND OpenAI Responses (three frontends, each with its own envelope).
>
> End with APPROVE / CONDITIONAL / BLOCK per the v0.5 P05 T10 pattern.

- [ ] **Step 3: Apply CRITICAL/HIGH findings**

If reviewers find blockers, fix them and commit. Final commit before merge:

```bash
git commit -m "fix: address Phase 6 final review findings"
```

---

## Done when

- [ ] Workspace test count ≥ 660.
- [ ] All v0.5 deferral items removed from CHANGELOG / README / observability.md.
- [ ] ADR-0006 lands.
- [ ] Workspace version is `0.6.0`.
- [ ] T9 final reviews APPROVE (no CRITICAL / HIGH).
- [ ] Phase 6 worktree ready for merge to master via `finishing-a-development-branch`.
