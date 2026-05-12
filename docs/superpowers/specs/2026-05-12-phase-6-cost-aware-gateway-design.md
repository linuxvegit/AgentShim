# Phase 6 design spec — Cost-aware gateway (v0.6.0)

**Status:** Approved 2026-05-12. Implementation pending.
**Source phase:** v0.5.0 (master HEAD `44749de`).
**Author:** Phase 6 brainstorm session, 2026-05-12.

## 1. Goals

Phase 6 (v0.6.0) closes the two v0.5 deferrals and adds **cost-aware
routing** as the new operator-facing capability. Three pillars:

1. **Outbound trace continuation.** A new `crates/providers/src/client.rs`
   exposes a `ProviderClientFactory` that constructs every provider's
   `reqwest::Client` with a `traceparent`-injecting middleware. Inbound
   trace context (parsed by the v0.5 pipeline) flows out to upstreams,
   enabling end-to-end distributed traces.

2. **Reload-aware rate limiting.** `LimiterRegistry` moves to
   `Arc<ArcSwap<LimiterRegistry>>` on `AppCore`. The reload-applying task
   swaps the registry atomically alongside the existing
   `Arc<ArcSwap<AppSnapshot>>` swap. Rule §5.4 (rate-limit bucket reset
   on policy change) becomes fully implemented; the v0.5 deferral
   disappears.

3. **Cost-aware routing.** Four orthogonal axes act as **filters** over
   the existing v0.4 fallback chain (operator-defined chain order is
   preserved):
   - **Tier label** (`economy` / `standard` / `premium`) per upstream
     + `min_tier` per route
   - **Per-token cost** (USD per million input + output tokens) per
     upstream/model
   - **Latency budget** (`p95_latency_budget_ms`) per upstream,
     evaluated against the v0.5 `agent_shim_upstream_duration_seconds`
     histogram
   - **Per-request cost cap** (`max_cost_usd`) per route, evaluated
     pre-chain-walk

   If every chain element is filtered out, the gateway returns
   `HTTP 503 NoEligibleUpstream` with a per-axis reason list.

### 1.1 Non-goals

The following are explicitly out of scope and remain v0.7+ candidates:

- Learned / realised cost tracking (rolling EWMA)
- Dynamic re-ordering of the chain by predicted cost
- Agent-driven routing hints (e.g. `agent-shim-budget: low` header)
- Distributed breaker / rate-limit state (Redis-backed)
- k8s manifests / Helm chart
- Redacted request/response capture & replay

## 2. Frozen-core invariant changes for Phase 6

The Phase 5 invariant — **empty diff against v0.4.1 baseline for
`crates/core/`, `crates/frontends/`, `crates/providers/src/`** —
narrows in Phase 6.

### 2.1 New invariant

```
git diff 44749de -- crates/core/ crates/frontends/
```

must be empty. **`crates/providers/src/` is no longer covered by the
freeze.** P01 explicitly adds new pub API to that crate (the
`ProviderClientFactory`) and rewires every existing provider's
`reqwest::Client` construction site to use it.

### 2.2 Discipline for the unfrozen `providers/src/`

Lifting the freeze is not a license to refactor. The Phase 6
discipline is:

- Exactly **one** new module: `crates/providers/src/client.rs`
- Exactly **one** new line in `crates/providers/src/lib.rs`:
  `pub mod client; pub use client::ProviderClientFactory;`
- Every existing provider source file that diff-touches the v0.5
  baseline must have **only** mechanical call-site replacements:
  `reqwest::Client::new()` (or `Client::builder()...build()`) becomes
  a call through `ProviderClientFactory`. No signature changes. No
  behavioural changes.

P01's spec-compliance review enforces this rule via a side-by-side
diff against `44749de`.

### 2.3 v0.6 baseline for future phases

After Phase 6 closes, the v0.6.0 release tag becomes the new baseline
that future phases diff against. Phase 7+ specs will re-state the
frozen set.

## 3. Architecture & crate boundaries

The Phase 5 boundary rules continue:

- **Frontends and providers never import each other.**
- **Router does not depend on observability.** Latency probes are a
  trait defined in `router`; the Prometheus-backed implementation
  lives in `gateway` and is injected at `AppState::new`.
- **Gateway is still the sole wiring point.**

Phase 6 adds exactly one new crate-level dependency edge:
**providers → observability** (so `ProviderClientFactory` can call
`agent_shim_observability::otel::inject_traceparent_into_headers`).

### 3.1 Module map

```
crates/config/
├── schema.rs           UpstreamConfig: + tier (required) + cost (optional)
│                                       + p95_latency_budget_ms (optional)
│                       RouteEntry:     + min_tier (optional) + max_cost_usd (optional)
│                       Tier enum:      economy | standard | premium
└── validation.rs       Validation rules 15–18 (see §5)

crates/router/
├── cost_filter.rs (new)     CostFilter module: pure-function filter pass
├── latency_probe.rs (new)   LatencyProbe trait + MockLatencyProbe for tests
├── rate_limit.rs            Unchanged public API; AppCore now holds ArcSwap
└── resilient_caller.rs      Chain walk gains a cost_filter::filter_chain() pass
                             before the existing resilience pipeline

crates/providers/
└── src/
    ├── client.rs (new)      ProviderClientFactory — reqwest::Client builder
    │                        with traceparent-injecting middleware
    └── lib.rs               + pub mod client; + pub use client::*;

crates/observability/
└── src/
    └── otel/
        └── inject.rs (new)  inject_traceparent_into_headers(&mut HeaderMap)
                             Promoted from v0.5 P03 T4 step 5 (deferred helper)

crates/gateway/
├── state.rs                 limiter_registry: Arc<ArcSwap<LimiterRegistry>>
├── latency_probe.rs (new)   PrometheusLatencyProbe — reads the
│                            agent_shim_upstream_duration_seconds histogram
│                            via metrics-exporter-prometheus snapshot API;
│                            injected into ResilientCaller at AppState::new
├── commands/serve.rs        handle_reload now rebuilds + swaps the limiter
│                            registry alongside AppSnapshot
└── tests/
    ├── cost_filter_full_skip.rs (new)
    ├── cost_filter_tier_partial.rs (new)
    ├── cost_filter_metric_counter.rs (new)
    ├── reload_rate_limit_buckets_reset.rs (new)
    ├── reload_tier_change.rs (new)
    ├── outbound_traceparent.rs (new)
    ├── outbound_traceparent_inherits_inbound.rs (new)
    └── phase6_smoke.rs (new)
```

### 3.2 Boundary rule rationale

The `providers → observability` edge is the price of in-process
outbound `traceparent` injection. The alternative (gateway-side
client factory + tower::Layer over reqwest) keeps providers frozen
forever but requires routing every provider's HTTP construction
through a gateway-owned factory — invasive and ugly. We chose the
cleaner provider-side edge with the discipline rules above (§2.2)
bounding the blast radius.

## 4. Data flow & request lifecycle

The v0.5 lifecycle stays intact. Phase 6 inserts one new step in the
request path and adds outbound injection at the provider layer.

```
1. HTTP request → public listener
2. RequestIdLayer + MetricsLayer + inbound traceparent extraction
3. gateway.request root span created (frontend, route, request_id,
   inbound trace context attached)
4. FrontendProtocol::decode_request → CanonicalRequest
5. ModelResolver::resolve(frontend, model) → BackendTarget (chain)
6. AuthLayer validates API key (if enabled)
7. RateLimit gate
   └── Arc<ArcSwap<LimiterRegistry>>.load() — picks up the live snapshot
       ★ v0.6: a reload that changed rate_limit.* takes effect here
         on the *next* request after the swap
8. ★ NEW: CostFilter::filter_chain(chain, route, request_estimate,
                                    latency_probe) → filtered_chain
   For each upstream in chain:
     a. if upstream.tier < route.min_tier → skip, counter +1 ("tier")
     b. if probe.recent_p95(upstream) > route.p95_budget → skip ("latency")
     c. if estimated_cost(request, upstream) > route.max_cost_usd
         → skip ("cap")
   If filtered_chain is empty:
     return HTTP 503 NoEligibleUpstream
       body = {ok:false, errors:["no eligible upstream"],
               filtered: [{upstream, reason}, ...]}
9. ResilientCaller::call(filtered_chain) — unchanged v0.4 path
   ├── retry / breaker / fallback unchanged
   ├── provider.complete span per chain element
   └── ★ Outbound HTTP: every provider uses ProviderClientFactory.
       The factory's reqwest::Client has a middleware that reads
       Span::current()'s OTel context and injects traceparent
       + tracestate into outgoing HeaderMap.
10. CanonicalStream → FrontendProtocol::encode_stream → SSE bytes
11. Response → client
12. MetricsLayer records request_duration / status_class
    Root span closes → OTel exports (if enabled)
```

### 4.1 Key design decisions visible in the data flow

**(D1) CostFilter runs after RateLimit but before ResilientCaller.**

- *After RateLimit:* once a request has consumed a token bucket, we
  prefer not to "double-charge" by filtering it later — that would
  waste a slot. RateLimit is request *admission*; CostFilter is
  *chain selection*.
- *Before ResilientCaller:* CostFilter does not participate in retry
  decisions. A retry on the same upstream uses the same upstream (v0.4
  retry policy semantics). Fallback to the next chain element walks
  *only the filtered chain* because the chain was filtered upfront.
  No "filter recomputed at retry time" pathology.

**(D2) `LatencyProbe` is a trait — router does not depend on
observability.**

```rust
pub trait LatencyProbe: Send + Sync {
    /// Returns the recent p95 latency in milliseconds for the given
    /// upstream, or None if there is no sample data yet.
    fn recent_p95_ms(&self, upstream: &str) -> Option<u64>;
}
```

`router` defines the trait. `gateway` provides
`PrometheusLatencyProbe` (concrete impl living in
`crates/gateway/src/latency_probe.rs`, which reads the
`agent_shim_upstream_duration_seconds` histogram exposed by
metrics-rs through the `metrics-exporter-prometheus` snapshot API)
and injects it into `ResilientCaller` at construction. Tests use
`MockLatencyProbe { values: HashMap<String, u64> }` in
`crates/router/src/latency_probe.rs::tests`.

A `None` return means "no samples yet" — CostFilter treats this as
"let it through" and emits an info-level metric (see §4.3 D5) so
operators see warm-up periods.

**(D3) Cost estimation is intentionally pessimistic.**

```
estimated_cost(request, upstream) =
    estimate_input_tokens(request) * upstream.cost.input_per_million_usd / 1e6
  + max_output_tokens(request)     * upstream.cost.output_per_million_usd / 1e6
```

- `estimate_input_tokens` uses `tiktoken-rs` (already a workspace dep
  from v0.3). On encoder failure (rare), it falls back to
  `request.len() / 4` (rough heuristic) — request never fails on
  estimation alone.
- `max_output_tokens` is the request's `max_tokens` field (or
  per-frontend equivalent) if set, else **4096** (a conservative cap).

The estimate is an **upper bound, not a realised cost**. Operators set
`max_cost_usd` with this semantic in mind: "reject any request that
could plausibly cost more than $X". Realised costs are typically
lower and continue to be invisible in v0.6 (learned-cost is a v0.7
candidate).

**(D4) Outbound traceparent injection is set up at client construction
time, not per request.**

`ProviderClientFactory` constructs a `reqwest::Client` with a
middleware-style hook that, on every outgoing request, reads
`tracing::Span::current()`'s OTel context and writes `traceparent`
+ (if present) `tracestate` into the outgoing `HeaderMap`. The
provider code itself doesn't see this layer — every existing
provider's `reqwest::Client` construction just goes through the
factory.

The concrete mechanism (P01 picks between two options):

- **Option A:** add the `reqwest-middleware` crate (`https://docs.rs/reqwest-middleware/`)
  as a workspace dep; the factory builds a `ClientWithMiddleware`.
  Standard pattern, well-supported, but adds one dep.
- **Option B:** custom `RequestBuilder` wrapper at the factory layer
  that intercepts `.send()` to inject headers right before the
  network call. No new dep, but a thicker wrapper.

P01 chooses based on whether `reqwest-middleware` adds friction in
the existing reqwest call sites. Both options satisfy the spec.

Malformed or absent inbound contexts produce an empty OTel context
in `Span::current()`; injection becomes a no-op for those requests.
No 5xx errors come from injection failures.

**(D5) New metric: `agent_shim_cost_filtered_total`.**

Labels: `reason in {tier, latency, cap, latency_unknown, tiktoken_fallback}`,
`upstream`, `route`.

- `reason="tier"` — upstream filtered because tier was below
  route's `min_tier`.
- `reason="latency"` — probe returned a value above the budget.
- `reason="cap"` — estimated cost exceeded `max_cost_usd`.
- `reason="latency_unknown"` — probe returned None; the upstream is
  **not** skipped (passes through), but the event is recorded so
  operators see warm-up behaviour.
- `reason="tiktoken_fallback"` — `tiktoken-rs` failed to encode the
  request; the cost estimate fell back to `body.len() / 4` heuristic.
  The upstream is **not** skipped on this reason alone; recorded so
  operators see the failure rate.

Cardinality budget: `route` is bounded by config; `upstream` is
bounded by config. `reason` is a fixed 5-value enum. Safe within
Prometheus best practices.

## 5. Configuration schema (Rust + YAML)

Phase 5 schema unchanged; Phase 6 adds new fields. Every new field
that touches the existing `UpstreamConfig` enum lives on the
specific variant struct, not on the enum tag.

### 5.1 New types

```rust
// crates/config/src/schema.rs

#[derive(Debug, Clone, Copy, Serialize, Deserialize,
         PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Tier { Economy, Standard, Premium }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UpstreamCost {
    pub input_per_million_usd: f64,
    pub output_per_million_usd: f64,
}
```

### 5.2 Field additions

Every `UpstreamConfig` variant struct (the five existing ones:
`OpenAiCompatibleUpstream`, `GithubCopilotUpstream`,
`AnthropicUpstream`, `DeepseekUpstream`, `GeminiUpstream`) gains:

```rust
pub tier: Tier,                            // required — see §5.4
pub cost: Option<UpstreamCost>,            // optional
pub p95_latency_budget_ms: Option<u64>,    // optional
```

`RouteEntry` gains:

```rust
pub min_tier: Option<Tier>,                // optional
pub max_cost_usd: Option<f64>,             // optional
```

### 5.3 Example YAML (annotated)

```yaml
upstreams:
  ds:
    type: open_ai_compatible
    base_url: https://api.deepseek.com/v1
    api_key: ${DEEPSEEK_API_KEY}
    tier: economy                                    # v0.6 REQUIRED
    cost:                                            # v0.6 optional
      input_per_million_usd: 0.14
      output_per_million_usd: 0.28
    p95_latency_budget_ms: 8000                      # v0.6 optional
  anthropic:
    type: anthropic
    api_key: ${ANTHROPIC_API_KEY}
    tier: premium                                    # v0.6 REQUIRED
    cost:
      input_per_million_usd: 3.00
      output_per_million_usd: 15.00
    p95_latency_budget_ms: 12000

routes:
  - frontend: anthropic_messages
    model: claude-sonnet-4
    upstreams: [anthropic, ds]   # operator chain (v0.4 fallback order)
    upstream_model: claude-sonnet-4-20250514
    min_tier: standard           # v0.6 optional — ds (economy) is filtered out
    max_cost_usd: 0.05           # v0.6 optional — reject if estimate > $0.05
```

### 5.4 Breaking change: `tier` is required

**This is the only deliberate breaking change in v0.6.**

Every upstream must now declare a `tier`. Rationale:

- The cost filter needs to know each upstream's tier — an implicit
  default would let operators silently misconfigure routes.
- Failing loudly at startup is the right move: the operator's
  intent becomes visible in the config file.

**Upgrade action:** add one line per upstream:

```yaml
upstreams:
  ds:
    # existing fields...
    tier: standard       # or economy / premium
```

The CHANGELOG's "Breaking" section must call this out explicitly.

### 5.5 Validation rules (continuing from v0.5 rules 11–14)

| # | Time | Rule |
|---|---|---|
| 15 | Startup + reload | If `cost` is present on any upstream, both `input_per_million_usd` and `output_per_million_usd` must be ≥ 0. Partial declaration → `IncompleteCost`. |
| 16 | Startup + reload | `tier` value must be exactly `economy`, `standard`, or `premium`. Schema-layer enum enforces this; only fails on hand-crafted invalid YAML. |
| 17 | Startup + reload | For every route with `min_tier` set, at least one upstream in the route's chain must have `tier ≥ min_tier`. Otherwise the route is guaranteed to produce 503 — `ImpossibleMinTier { route, min_tier, chain_tiers }`. |
| 18 | Reload | `tier`, `cost`, `p95_latency_budget_ms`, `min_tier`, `max_cost_usd` are ALL reloadable. The v0.5 §5.5 immutable-field set (rules 11–14) does **not** expand. |

### 5.6 New ValidationError variants

```rust
pub enum ValidationError {
    // ... existing variants ...
    IncompleteCost { upstream: String },
    NegativeCost { upstream: String, field: &'static str },
    InvalidTier { upstream: String, value: String },   // rarely-hit; schema usually catches
    ImpossibleMinTier {
        route: String,
        min_tier: Tier,
        chain_tiers: Vec<(String, Tier)>,   // (upstream_name, its_tier)
    },
}
```

### 5.7 Environment overlay

Sticks with the v0.5 `AGENT_SHIM__` prefix + double-underscore
nesting (Figment-backed). Example:

```
AGENT_SHIM__UPSTREAMS__DS__TIER=economy
AGENT_SHIM__UPSTREAMS__DS__COST__INPUT_PER_MILLION_USD=0.14
AGENT_SHIM__ROUTES__0__MAX_COST_USD=0.05
```

No new mechanism — pure field extension.

## 6. Error handling & failure modes

### 6.1 Startup failures (new in v0.6)

| Failure | Error | Effect |
|---|---|---|
| `cost` field partial | `IncompleteCost` | Process exits with code 2 |
| `tier` value invalid | `InvalidTier` (or schema parse error) | Process exits with code 2 |
| `cost.*` negative | `NegativeCost` | Process exits with code 2 |
| `min_tier` unsatisfiable by chain | `ImpossibleMinTier` | Process exits with code 2 |

All go through the existing `validate()` function. No new "warn-and-
continue" paths.

### 6.2 Reload failures (new in v0.6)

| Failure | ReloadOutcome | HTTP / SIGHUP behaviour |
|---|---|---|
| Any 6.1 condition | `ValidationError` | POST → 400; SIGHUP → metric counter, log |
| `ImpossibleMinTier` on reload | `ValidationError` | Same |
| LimiterRegistry rebuild panics | `Io("limiter rebuild failed: ...")` (caught) | 500 / log |

**Atomicity invariant:** failed reload touches neither `AppSnapshot`
nor `limiter_registry`. Order:

```
1. validate_for_reload(candidate, baseline)?
2. new_snapshot = build_snapshot(candidate)
3. new_limiter = LimiterRegistry::from_config(&candidate.rate_limit)
4. state.snapshot.store(Arc::new(new_snapshot))    // commit point 1
5. state.core.limiter_registry.store(Arc::new(new_limiter))   // commit point 2
6. emit metric + log
```

**Window between commits 1 and 2:** very short (two atomic stores).
A request arriving in between sees the new snapshot but the old
limiter. This is benign — both are valid; the next request sees the
fully new state. ArcSwap's documented property; v0.5 spec §5.2
already accepts this.

### 6.3 Request-time failures (new in v0.6)

| Failure | HTTP status | Body |
|---|---|---|
| CostFilter empties the chain | `503 NoEligibleUpstream` | `{ok:false, errors:["no eligible upstream"], filtered:[{upstream, reason}, ...]}` |
| `tiktoken-rs` encode failure | (no fail) — fallback to `len/4` estimate | metric `cost_filtered_total{reason=tiktoken_fallback}` +1 (operator-visible) |
| `latency_probe.recent_p95(upstream) = None` | (no fail) — let request through | metric `cost_filtered_total{reason=latency_unknown}` +1 |
| Outbound traceparent injection malformed (`HeaderValue::try_from` error, theoretically unreachable) | (no fail) — request continues without trace | warn log only |

**Alignment with v0.4:** The 503 status code joins the existing
`HandlerError` enum (which already includes `RateLimited`(429) and
`BreakerOpen`(503) variants from v0.4). Each frontend's `IntoResponse`
implementation translates `NoEligibleUpstream` to its native error
envelope shape — OpenAI Chat → OpenAI shape, Anthropic → Anthropic
shape — same discipline v0.4 established.

### 6.4 Observability degradation rules (continuing v0.5)

- Outbound traceparent injection failure → warn log, request unaffected
- Latency probe data missing → "let it through", request unaffected
- Metrics emit failures (theoretical) → silent

The slogan stays: **observability is diagnostics, not admission
control. Never let metrics / tracing / cost-estimation kill a
legitimate request.**

## 7. Test strategy

### 7.1 Unit tests (~11 new)

| Module | Test | What it pins |
|---|---|---|
| `validation.rs` | `cost_partial_field_rejected` | `IncompleteCost` on missing output |
| | `negative_cost_rejected` | `NegativeCost` on negative input |
| | `impossible_min_tier_rejected` | `ImpossibleMinTier` when chain is all-economy + min=premium |
| | `reload_allows_tier_change` | rule 18: tier change accepted |
| | `reload_allows_cost_change` | rule 18: cost change accepted |
| | `reload_rule_17_rechecks_on_tier_change` | reload changing tier → `ImpossibleMinTier` |
| `cost_filter.rs` | `tier_filters_below_min` | tier-below-min upstream is filtered, reason="tier" |
| | `latency_filter_uses_probe` | mock probe over budget → skip; None → pass |
| | `cost_cap_filter_uses_estimate` | estimate > cap → skip |
| | `all_filtered_returns_empty` | empty chain + reason list |
| | `partial_filter_preserves_chain_order` | post-filter survivors keep original order |

### 7.2 Integration tests (~8 new files)

```
crates/gateway/tests/
├── cost_filter_full_skip.rs                503 + reason list
├── cost_filter_tier_partial.rs             first survivor hit
├── cost_filter_metric_counter.rs           cost_filtered_total increments
├── reload_rate_limit_buckets_reset.rs      reload of rate_limit takes effect
├── reload_tier_change.rs                   reload changing tier alters routing
├── outbound_traceparent.rs                 outbound request carries traceparent
├── outbound_traceparent_inherits_inbound.rs  outbound trace_id == inbound trace_id
└── phase6_smoke.rs                         all 3 pillars end-to-end
```

### 7.3 Test count budget

| Source | Delta |
|---|---|
| Unit tests | +11 |
| Integration tests (~1 test/file × 8 files) | +8 |
| **Total** | **+19** |

Current workspace count: **641**. Phase 6 target: **≥ 660**.

### 7.4 Mockito for outbound traceparent verification

```rust
let upstream = mockito::Server::new();
let _m = upstream.mock("POST", "/v1/chat/completions")
    .match_header("traceparent",
        Matcher::Regex(r"^00-[0-9a-f]{32}-[0-9a-f]{16}-01$".to_string()))
    .with_status(200)
    .with_body("...")
    .create();

let inbound_trace_id = "0af7651916cd43dd8448eb211c80319c";
let inbound_traceparent =
    format!("00-{inbound_trace_id}-b7ad6b7169203331-01");

client.post(format!("{public}/v1/chat/completions"))
    .header("traceparent", inbound_traceparent)
    .json(&body).send().await?;

upstream.assert(); // mockito asserts the outbound request matched
```

For the "trace_id continuity" test (`outbound_traceparent_inherits_inbound`),
a custom `mockito` matcher captures the outbound `traceparent` header
and asserts its 32-char trace_id segment equals the inbound one. This
is the genuinely-new end-to-end coverage that v0.5 P03 T5
(in-memory exporter) stopped short of.

### 7.5 Frozen-core verification (per commit)

```bash
git diff 44749de -- crates/core/ crates/frontends/
```

Each Phase 6 commit must produce empty output. `providers/src/` is
**not** in the diff command — it's now an unfrozen, disciplined
crate per §2.2.

At Phase 6 close, an additional providers audit:

```bash
git diff 44749de -- crates/providers/src/
```

This will be non-empty (P01 adds `client.rs` and rewires call sites),
but every diff hunk must be either:

1. The new `client.rs` module
2. The one-line `lib.rs` re-export
3. A mechanical `reqwest::Client::*` → factory replacement

No other categories. P01's spec-compliance review reads this diff and
classifies every hunk.

## 8. Plan decomposition (5 plans, ~31 tasks)

| Plan | Name | Tasks | Test delta | Risk |
|---|---|---|---|---|
| **P01** | Outbound traceparent + `ProviderClientFactory` | ~6 | +2 integration | Med — touches providers/src |
| **P02** | `ArcSwap<LimiterRegistry>` + reload integration | ~5 | +1 integration | Low — mirrors v0.5 AppSnapshot |
| **P03** | Cost / tier / latency schema + rules 15–18 | ~5 | +6 unit | Low — additive config |
| **P04** | CostFilter + ResilientCaller pass + 503 + metric | ~7 | +5 unit + 4 integration | Med — router internals |
| **P05** | Docs / ADR-0006 / CHANGELOG / version 0.5.0 → 0.6.0 / smoke | ~8 | +1 integration | Low |

Each plan ends with two reviews — **spec compliance** + **code
quality** — using the same dual-reviewer pattern Phase 5 established.

### 8.1 Plan ordering rationale

- P01 first because outbound traceparent requires breaking the v0.5
  freeze; doing it first locks the new providers/src API early.
- P02 second because it's near-mechanical (mirrors v0.5 P01 AppCore
  split). Closes the larger of the two v0.5 debt items.
- P03 third because P04 needs the schema to be in place to express
  the filter logic against typed fields.
- P04 fourth — the core new feature. All dependencies (probe trait,
  schema, factory) are in place.
- P05 last — docs / release ceremony.

## 9. Done-when (Phase 6 close criteria)

- [ ] Workspace test count ≥ **660**
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo fmt --all -- --check` clean
- [ ] Frozen-core diff (`crates/core/`, `crates/frontends/`) against
      `44749de` is **empty**
- [ ] Unfrozen-core audit (`crates/providers/src/`) classifies every
      hunk per §2.2 / §7.5
- [ ] Outbound `traceparent` injected at all 5 providers
      (mockito end-to-end covers at least 2)
- [ ] Reload of `rate_limit.*` produces a new `LimiterRegistry` on the
      next request (`reload_rate_limit_buckets_reset.rs` passes)
- [ ] CostFilter has positive + negative unit coverage for all 4
      axes (tier / latency / cap / latency_unknown)
- [ ] `agent_shim_cost_filtered_total{reason,upstream,route}` exposed
      on `/metrics`
- [ ] CHANGELOG's "Breaking" section explicitly calls out the
      `tier` requirement
- [ ] ADR-0006 lands: cost-aware routing decisions + rejected
      alternatives (learned EWMA, agent hints)
- [ ] Workspace version `0.5.0` → **`0.6.0`**
- [ ] Both v0.5 "Known limitations" items (outbound traceparent +
      rate-limit reload effect) **removed** from README,
      `docs/observability.md`, and CHANGELOG running list

## 10. Risks & mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Tier requirement breaks existing operator configs at upgrade | High | Low (one-line fix) | CHANGELOG "Breaking" section + clear startup error |
| Cost estimation surprises operators (estimate > realised) | Medium | Medium | Doc the conservative-estimate semantics in `docs/configuration.md`; metric makes filter rate visible |
| `tiktoken-rs` encoder mismatch with upstream's tokeniser | Medium | Low (estimate is approximate anyway) | Document the disclaimer; treat as "good-enough for filtering" |
| Outbound traceparent middleware adds latency | Low | Low | Middleware is a single HeaderMap mutation per request; benchmark in P01 |
| LimiterRegistry swap window (commit 1 / commit 2) leaks observable behaviour | Very low | Very low | Document in observability.md as inherent ArcSwap property |
| `providers/src/` unfreeze opens the door to drift | Medium | High | Discipline rules §2.2 + per-commit diff audit + P01 spec compliance review |
| Cost filter cardinality blows up | Low | Medium | `route` and `upstream` both bounded by config; `reason` is a 4-value enum |

## 11. Locked decisions

| ID | Decision |
|---|---|
| D1 | Theme: close v0.5 debt + add cost-aware routing |
| D2 | Cost routing shape: static cost tags + budget knobs (not learned, not hint-driven) |
| D3 | Routing axes: all four — tier, cost-USD, p95-latency, request cap |
| D4 | Rate-limit reload: `ArcSwap<LimiterRegistry>` (replace whole registry) |
| D5 | Outbound traceparent: provider-side reqwest middleware (one new providers/src API) |
| D6 | Filter ordering vs chain: filter then chain-order (operator stays in control) |
| D7 | Plan shape: single phase, foundation-first, 5 plans (Approach A) |
| D8 | Frozen-core scope shrinks: only `crates/core/` + `crates/frontends/` for Phase 6 |
| D9 | `tier` is required (breaking change); intentional, documented |
| D10 | Cost estimation is conservative/pessimistic; uses `tiktoken-rs` + request's `max_tokens` |
| D11 | `LatencyProbe` is a trait in router; gateway provides Prometheus impl |
| D12 | New 503 status `NoEligibleUpstream` with per-axis reason envelope |

## 12. References

- v0.5 baseline commit: `44749de` (Phase 5 merge to master)
- v0.5 spec: `docs/superpowers/specs/2026-05-09-phase-5-observability-design.md`
- v0.5 ADR-0005: `docs/adr/0005-hot-reload-snapshot-model.md`
- v0.4 frozen-core invariant origin: `docs/adr/0004-resilient-caller.md`
- governor crate (rate limiting): `https://docs.rs/governor/`
- tiktoken-rs (token estimation): `https://docs.rs/tiktoken-rs/`
- reqwest-middleware (outbound injection mechanism): `https://docs.rs/reqwest-middleware/`
- W3C Trace Context: `https://www.w3.org/TR/trace-context/`
