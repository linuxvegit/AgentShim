# Plan 04 — CostFilter + ResilientCaller pass + 503 + metric (Phase 6)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-12-phase-6-cost-aware-gateway-design.md`](../specs/2026-05-12-phase-6-cost-aware-gateway-design.md) (§1 pillar 3, §3.1 module map, §4 data flow D1-D5, §6.3 NoEligibleUpstream).

**Goal:** Implement the four-axis cost filter as a pre-chain-walk pass inside `ResilientCaller`. When the filter empties the chain, return `HTTP 503 NoEligibleUpstream` with a per-axis reason envelope. Emit the new `agent_shim_cost_filtered_total{reason,upstream,route}` metric.

**Architecture:** Three new modules in `crates/router/src/`:

1. `cost_filter.rs` — pure-function `filter_chain(chain, route, request_estimate, latency_probe) -> FilterOutcome`. No I/O, no metrics emission here; the caller (ResilientCaller) emits.
2. `latency_probe.rs` — `LatencyProbe` trait + `MockLatencyProbe` for tests.
3. `cost_estimate.rs` — `estimate_request_cost(request, upstream) -> f64` using `tiktoken-rs` + `request.max_tokens`.

Gateway provides `PrometheusLatencyProbe` in `crates/gateway/src/latency_probe.rs` that reads the histogram via the `metrics-exporter-prometheus` snapshot handle.

`ResilientCaller::call` gains one new step at the top: filter the chain, emit metric per skip, return early with `NoEligibleUpstream` if the filtered chain is empty.

**Tech stack:** Uses `tiktoken-rs` (already a workspace dep from v0.3). Uses `metrics-exporter-prometheus` snapshot API (already in workspace from v0.5).

**Frozen-core impact:** Touches `crates/router/src/` (3 new modules + `resilient_caller.rs` modification) and `crates/gateway/src/` (1 new module + `state.rs` wiring). No `crates/providers/src/` change. Frozen core (core+frontends) untouched.

**Test target:** 650 (after P03) → 659 (+5 unit in cost_filter + 4 integration in gateway tests).

---

## File Structure

`crates/router/src/`:
- Create: `latency_probe.rs` — `LatencyProbe` trait, `MockLatencyProbe`, unit tests.
- Create: `cost_estimate.rs` — `estimate_request_cost(req, upstream) -> f64`, `tiktoken-rs` integration, fallback heuristic, unit tests.
- Create: `cost_filter.rs` — `filter_chain(chain, route, probe) -> FilterOutcome`, four-axis filter pass, unit tests.
- Modify: `lib.rs` — re-exports for the three new modules.
- Modify: `resilient_caller.rs` — pre-chain-walk filter step, metric emission, new error variant `NoEligibleUpstream`.
- Modify: `errors.rs` (or wherever the resilience errors live) — `NoEligibleUpstream { filtered: Vec<FilterReason> }`.

`crates/observability/src/metrics/`:
- Modify: `names.rs` — new const `COST_FILTERED_TOTAL`.

`crates/gateway/src/`:
- Create: `latency_probe.rs` — `PrometheusLatencyProbe` reading the metrics-exporter-prometheus snapshot.
- Modify: `state.rs` — wire the probe into `ResilientCaller::new`.
- Modify: any frontend handler error translation — `NoEligibleUpstream` -> 503 with frontend-shaped error body.
- Modify: `pipeline.rs` if necessary — wire the filter call into the dispatch path.

`crates/gateway/tests/`:
- Create: `cost_filter_full_skip.rs` — chain entirely filtered → 503.
- Create: `cost_filter_tier_partial.rs` — partial filter, first survivor wins.
- Create: `cost_filter_metric_counter.rs` — `cost_filtered_total` increments.
- Modify: `phase5_smoke.rs` may need a `tier:` addition (already covered by P03 T5).

---

## Tasks

### Task 1: `LatencyProbe` trait + `MockLatencyProbe`

**Files:**
- Create: `crates/router/src/latency_probe.rs`
- Modify: `crates/router/src/lib.rs`

- [ ] **Step 1: Define the trait + mock**

Create `crates/router/src/latency_probe.rs`:

```rust
//! Latency probe trait + test mock. Plan 06 P04 T1.
//!
//! Cost-aware routing's latency axis asks: "what's the recent p95
//! latency for upstream X?" The probe abstracts the data source so
//! router can stay independent of observability (boundary rule from
//! v0.5 D11). Gateway provides the Prometheus-backed implementation
//! that reads the agent_shim_upstream_duration_seconds histogram.

use std::collections::HashMap;
use std::sync::Mutex;

/// Source of recent p95 latency data per upstream. The router uses
/// this to decide whether an upstream's `p95_latency_budget_ms` is
/// being met. None means "no sample data yet" — the cost filter
/// treats that as "let it through".
pub trait LatencyProbe: Send + Sync {
    /// Recent p95 latency for `upstream` in milliseconds, or None if
    /// no samples are available.
    fn recent_p95_ms(&self, upstream: &str) -> Option<u64>;
}

/// Test-only mock with hard-coded per-upstream values.
#[derive(Default)]
pub struct MockLatencyProbe {
    values: Mutex<HashMap<String, u64>>,
}

impl MockLatencyProbe {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(values: impl IntoIterator<Item = (&'static str, u64)>) -> Self {
        let m = values
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        Self {
            values: Mutex::new(m),
        }
    }

    pub fn set(&self, upstream: &str, ms: u64) {
        self.values.lock().unwrap().insert(upstream.to_string(), ms);
    }
}

impl LatencyProbe for MockLatencyProbe {
    fn recent_p95_ms(&self, upstream: &str) -> Option<u64> {
        self.values.lock().unwrap().get(upstream).copied()
    }
}

/// Always-None probe — every query returns None. Useful as the
/// default in code paths that don't need latency filtering.
pub struct DisabledLatencyProbe;

impl LatencyProbe for DisabledLatencyProbe {
    fn recent_p95_ms(&self, _: &str) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_returns_set_values() {
        let p = MockLatencyProbe::with([("a", 100), ("b", 500)]);
        assert_eq!(p.recent_p95_ms("a"), Some(100));
        assert_eq!(p.recent_p95_ms("b"), Some(500));
        assert_eq!(p.recent_p95_ms("c"), None);
    }

    #[test]
    fn disabled_always_returns_none() {
        let p = DisabledLatencyProbe;
        assert_eq!(p.recent_p95_ms("anything"), None);
    }
}
```

- [ ] **Step 2: Re-export from router lib.rs**

Add to `crates/router/src/lib.rs`:

```rust
pub mod latency_probe;
pub use latency_probe::{DisabledLatencyProbe, LatencyProbe, MockLatencyProbe};
```

- [ ] **Step 3: Test**

```bash
cargo test -p agent-shim-router --lib latency_probe 2>&1 | tail -10
```

Expected: 2 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/router/src/latency_probe.rs crates/router/src/lib.rs
git commit -m "feat(router): LatencyProbe trait + MockLatencyProbe (Plan 04 P04 T1)"
```

---

### Task 2: `cost_estimate.rs` — token-aware request cost estimation

**Files:**
- Create: `crates/router/src/cost_estimate.rs`
- Modify: `crates/router/src/lib.rs`
- Modify: `crates/router/Cargo.toml` (add tiktoken-rs if not already)

- [ ] **Step 1: Verify tiktoken-rs is a router dep**

```bash
grep tiktoken crates/router/Cargo.toml
```

If absent, add to `[dependencies]`:

```toml
tiktoken-rs.workspace = true
```

- [ ] **Step 2: Write the estimator**

Create `crates/router/src/cost_estimate.rs`:

```rust
//! Per-request cost estimation. Plan 06 P04 T2.
//!
//! Estimates the upper-bound USD cost of routing a request to a given
//! upstream. Used by the cost filter to apply per-route `max_cost_usd`
//! caps. The estimate is intentionally pessimistic — operators set the
//! cap expecting "reject if it COULD cost more than $X", not "if it
//! WILL".
//!
//! Spec §4.1 D3:
//!   estimate = estimate_input_tokens(request) * input_price/1M
//!            + max_output_tokens(request)     * output_price/1M
//!
//! When `tiktoken-rs` fails to encode the request body (rare), the
//! input estimate falls back to `body_chars / 4` — a rough but always-
//! available heuristic. The fallback is metric-visible via the
//! `cost_filtered_total{reason=tiktoken_fallback}` counter (emitted by
//! the caller, not here).

use agent_shim_config::UpstreamCost;
use agent_shim_core::CanonicalRequest;

/// Conservative default cap for output tokens when the request doesn't
/// specify one. Picked to be roughly the median frontier-model default;
/// won't underestimate cost much in practice.
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4096;

/// Outcome of a single estimation. `cost_usd` is always populated;
/// `used_fallback_encoder` lets the caller emit the
/// `reason=tiktoken_fallback` metric without re-doing the work.
pub struct CostEstimate {
    pub cost_usd: f64,
    pub used_fallback_encoder: bool,
}

/// Estimate the USD cost of routing `request` to an upstream that has
/// the given `cost` schedule. When `cost` is None (e.g. Copilot
/// subscription), this returns `Some(0.0)` — there is no per-request
/// cost to estimate, so the cap never fires.
pub fn estimate_request_cost(
    request: &CanonicalRequest,
    cost: Option<&UpstreamCost>,
) -> Option<CostEstimate> {
    let cost = cost?;

    let (input_tokens, used_fallback_encoder) = estimate_input_tokens(request);
    let output_tokens = output_token_cap(request);

    let cost_usd = (input_tokens as f64 * cost.input_per_million_usd
        + output_tokens as f64 * cost.output_per_million_usd)
        / 1_000_000.0;

    Some(CostEstimate {
        cost_usd,
        used_fallback_encoder,
    })
}

/// Estimate input tokens using `tiktoken-rs`'s `cl100k_base` encoder
/// (the same one frontier models use). On encoder failure, falls back
/// to `len/4` heuristic and reports `used_fallback = true`.
fn estimate_input_tokens(request: &CanonicalRequest) -> (u32, bool) {
    // Concatenate all message text content. CanonicalRequest's exact
    // shape lives in `agent_shim_core`; this is the pattern v0.3
    // count_tokens module established.
    let text = canonical_request_text(request);
    match tiktoken_rs::cl100k_base() {
        Ok(bpe) => {
            let tokens = bpe.encode_with_special_tokens(&text);
            (tokens.len() as u32, false)
        }
        Err(_) => {
            // Fallback heuristic: ~4 chars per token (industry norm for
            // English-heavy text). Massively overestimates for code,
            // which is fine for a cost cap.
            ((text.len() / 4) as u32, true)
        }
    }
}

/// Pull the output cap from the request, falling back to a
/// conservative default. Adapt to whatever the actual CanonicalRequest
/// exposes — likely `request.max_tokens: Option<u32>` or similar.
fn output_token_cap(request: &CanonicalRequest) -> u32 {
    request.max_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
}

/// Concatenate every text content block in every message. Adapt the
/// exact accessor pattern to whatever `CanonicalRequest::messages`
/// exposes — see `crates/core/src/canonical.rs` for the type.
fn canonical_request_text(request: &CanonicalRequest) -> String {
    let mut out = String::new();
    for msg in &request.messages {
        for block in &msg.content {
            if let agent_shim_core::ContentBlock::Text { text } = block {
                out.push_str(text);
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{CanonicalRequest, ContentBlock, Message, MessageRole};

    fn req(text: &str, max_tokens: Option<u32>) -> CanonicalRequest {
        CanonicalRequest {
            model: "test".into(),
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text { text: text.into() }],
                ..Default::default()
            }],
            max_tokens,
            ..Default::default()
        }
    }

    #[test]
    fn cost_none_returns_none() {
        let r = req("hello", None);
        assert!(estimate_request_cost(&r, None).is_none());
    }

    #[test]
    fn cost_computes_when_inputs_present() {
        let cost = UpstreamCost {
            input_per_million_usd: 1.0,
            output_per_million_usd: 2.0,
        };
        let r = req("hello world", Some(100));
        let est = estimate_request_cost(&r, Some(&cost))
            .expect("cost present, request present");
        // Don't assert exact value (tokenizer-dependent); just that it's
        // a reasonable bound for a short text + tiny output cap.
        assert!(est.cost_usd >= 0.0);
        assert!(est.cost_usd < 0.001, "rough sanity: tiny request, tiny cost; got {}", est.cost_usd);
    }
}
```

**Critical note for the implementer:** the `CanonicalRequest` field accessors (`max_tokens`, `messages`, `ContentBlock::Text`) might not match the spec's assumptions verbatim. The implementer must read `crates/core/src/canonical.rs` (or wherever the actual types live) and adapt the field names. If `max_tokens` is named differently (`max_output_tokens`, `max_new_tokens`, etc.), substitute.

- [ ] **Step 3: Re-export**

Add to `crates/router/src/lib.rs`:

```rust
pub mod cost_estimate;
pub use cost_estimate::{estimate_request_cost, CostEstimate};
```

- [ ] **Step 4: Test**

```bash
cargo test -p agent-shim-router --lib cost_estimate 2>&1 | tail -10
```

Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/router/src/cost_estimate.rs crates/router/src/lib.rs crates/router/Cargo.toml
git commit -m "feat(router): per-request cost estimator with tiktoken-rs (Plan 04 P04 T2)"
```

---

### Task 3: `cost_filter.rs` — four-axis filter pass

**Files:**
- Create: `crates/router/src/cost_filter.rs`
- Modify: `crates/router/src/lib.rs`

- [ ] **Step 1: Write the filter**

Create `crates/router/src/cost_filter.rs`:

```rust
//! Four-axis chain filter for cost-aware routing. Plan 06 P04 T3.
//!
//! Spec §4 D1-D5. Takes a chain (the operator-ordered list of
//! BackendTarget candidates) and filters it by:
//!
//!   1. tier   — upstream.tier < route.min_tier
//!   2. latency — recent_p95 > route.p95_latency_budget_ms (per upstream)
//!   3. cap    — estimated_cost > route.max_cost_usd
//!
//! Returns the survivors in their original chain order (D6), plus per-
//! axis skip reasons so the caller can emit metrics + build the 503
//! NoEligibleUpstream response body when the survivor list is empty.

use std::sync::Arc;

use agent_shim_config::{GatewayConfig, RouteEntry, Tier, UpstreamCost};
use agent_shim_core::{BackendTarget, CanonicalRequest};

use crate::cost_estimate::estimate_request_cost;
use crate::latency_probe::LatencyProbe;

/// One axis on which an upstream was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FilterReason {
    Tier,
    Latency,
    Cap,
    /// Probe returned None — upstream was NOT skipped, but record so
    /// operators see warm-up periods (metric only; not a survival decision).
    LatencyUnknown,
    /// tiktoken-rs failed to encode the request — fallback used. Like
    /// LatencyUnknown, upstream is NOT skipped on this alone (metric only).
    TiktokenFallback,
}

impl FilterReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            FilterReason::Tier => "tier",
            FilterReason::Latency => "latency",
            FilterReason::Cap => "cap",
            FilterReason::LatencyUnknown => "latency_unknown",
            FilterReason::TiktokenFallback => "tiktoken_fallback",
        }
    }
}

/// One skipped upstream + the reason it was skipped.
#[derive(Debug, Clone)]
pub struct Skip {
    pub upstream: String,
    pub reason: FilterReason,
}

/// One observed event that did NOT cause a skip — for metrics.
#[derive(Debug, Clone)]
pub struct Note {
    pub upstream: String,
    pub reason: FilterReason,
}

/// Outcome of filtering a chain. `survivors` retains original order;
/// `skipped` lists the rejections; `notes` lists the non-skip events
/// (latency_unknown, tiktoken_fallback) so the caller can still emit
/// counters.
pub struct FilterOutcome {
    pub survivors: Vec<BackendTarget>,
    pub skipped: Vec<Skip>,
    pub notes: Vec<Note>,
}

/// Filter a chain. Pure function: no I/O, no metrics emission. Caller
/// emits metrics from the returned `skipped` + `notes` lists.
pub fn filter_chain(
    chain: Vec<BackendTarget>,
    route: &RouteEntry,
    request: &CanonicalRequest,
    config: &GatewayConfig,
    probe: &dyn LatencyProbe,
) -> FilterOutcome {
    let mut survivors = Vec::with_capacity(chain.len());
    let mut skipped = Vec::new();
    let mut notes = Vec::new();

    for target in chain {
        let upstream_name = target.upstream_name().to_string();
        let Some(upstream_cfg) = config.upstreams.get(&upstream_name) else {
            // Upstream config missing — earlier validation should have
            // caught this. Keep as a survivor; ResilientCaller will
            // surface the error consistently with v0.4 semantics.
            survivors.push(target);
            continue;
        };

        // Axis 1: tier
        if let Some(min_tier) = route.min_tier {
            let upstream_tier = upstream_config_tier(upstream_cfg);
            if upstream_tier < min_tier {
                skipped.push(Skip {
                    upstream: upstream_name,
                    reason: FilterReason::Tier,
                });
                continue;
            }
        }

        // Axis 2: latency
        let p95_budget = upstream_config_latency_budget(upstream_cfg);
        if let Some(budget) = p95_budget {
            match probe.recent_p95_ms(&upstream_name) {
                Some(measured) if measured > budget => {
                    skipped.push(Skip {
                        upstream: upstream_name,
                        reason: FilterReason::Latency,
                    });
                    continue;
                }
                None => {
                    // Probe has no data — let through, but note it.
                    notes.push(Note {
                        upstream: upstream_name.clone(),
                        reason: FilterReason::LatencyUnknown,
                    });
                }
                Some(_) => {}
            }
        }

        // Axis 3: cost cap
        if let Some(cap) = route.max_cost_usd {
            let upstream_cost = upstream_config_cost(upstream_cfg);
            if let Some(est) = estimate_request_cost(request, upstream_cost) {
                if est.used_fallback_encoder {
                    notes.push(Note {
                        upstream: upstream_name.clone(),
                        reason: FilterReason::TiktokenFallback,
                    });
                }
                if est.cost_usd > cap {
                    skipped.push(Skip {
                        upstream: upstream_name,
                        reason: FilterReason::Cap,
                    });
                    continue;
                }
            }
            // No cost schedule on this upstream — cap effectively
            // doesn't apply. Pass through (consistent with the
            // "Copilot has no per-token cost" case).
        }

        survivors.push(target);
    }

    FilterOutcome {
        survivors,
        skipped,
        notes,
    }
}

// ---- helpers (per-variant config accessors) ----

fn upstream_config_tier(u: &agent_shim_config::UpstreamConfig) -> Tier {
    use agent_shim_config::UpstreamConfig::*;
    match u {
        OpenAiCompatible(c) => c.tier,
        GithubCopilot(c) => c.tier,
        Anthropic(c) => c.tier,
        Deepseek(c) => c.tier,
        Gemini(c) => c.tier,
    }
}

fn upstream_config_cost(u: &agent_shim_config::UpstreamConfig) -> Option<&UpstreamCost> {
    use agent_shim_config::UpstreamConfig::*;
    match u {
        OpenAiCompatible(c) => c.cost.as_ref(),
        GithubCopilot(c) => c.cost.as_ref(),
        Anthropic(c) => c.cost.as_ref(),
        Deepseek(c) => c.cost.as_ref(),
        Gemini(c) => c.cost.as_ref(),
    }
}

fn upstream_config_latency_budget(u: &agent_shim_config::UpstreamConfig) -> Option<u64> {
    use agent_shim_config::UpstreamConfig::*;
    match u {
        OpenAiCompatible(c) => c.p95_latency_budget_ms,
        GithubCopilot(c) => c.p95_latency_budget_ms,
        Anthropic(c) => c.p95_latency_budget_ms,
        Deepseek(c) => c.p95_latency_budget_ms,
        Gemini(c) => c.p95_latency_budget_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // ... 5 tests per spec §7.1: tier_filters_below_min, latency_filter_uses_probe,
    //     cost_cap_filter_uses_estimate, all_filtered_returns_empty,
    //     partial_filter_preserves_chain_order
    //
    // Each test builds a tiny chain + mock probe + tiny config and asserts
    // the FilterOutcome contents. See the spec §7.1 table for exact names.
    //
    // Implementer note: the tests need `BackendTarget` fixtures — look at
    // existing router tests in `resilient_caller.rs::tests` for the
    // helper pattern (likely a `target(name)` helper that constructs a
    // minimal BackendTarget).
}
```

**Implementer responsibility:** the 5 unit tests in the `mod tests` block are spelled out in spec §7.1. Write them following the established `resilient_caller.rs::tests` shape. Each test should:

1. `tier_filters_below_min` — chain `[economy, premium]`, route `min_tier=standard`. Survivors = `[premium]`. Skipped = `[(economy, Tier)]`.
2. `latency_filter_uses_probe` — mock probe returns 500ms for "slow", None for "unknown". route `p95_latency_budget_ms=300`. "slow" gets skipped (Latency), "unknown" passes through with a Note(LatencyUnknown).
3. `cost_cap_filter_uses_estimate` — chain has one upstream with `cost: {input: 1000, output: 1000}` (very expensive) and a request that will obviously exceed `max_cost_usd: 0.01`. Survivor list is empty, skipped contains Cap reason.
4. `all_filtered_returns_empty` — pathological config that fails every axis on every upstream. Survivors empty; skipped list contains every upstream with the right reason.
5. `partial_filter_preserves_chain_order` — chain `[a, b, c, d]`, only `b` fails. Survivors are `[a, c, d]` in that order.

- [ ] **Step 2: Re-export**

Add to `crates/router/src/lib.rs`:

```rust
pub mod cost_filter;
pub use cost_filter::{filter_chain, FilterOutcome, FilterReason, Note, Skip};
```

- [ ] **Step 3: Build & test**

```bash
cargo test -p agent-shim-router --lib cost_filter 2>&1 | tail -10
```

Expected: 5 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/router/src/cost_filter.rs crates/router/src/lib.rs
git commit -m "feat(router): cost_filter four-axis chain pass (Plan 04 P04 T3)"
```

---

### Task 4: Wire CostFilter into ResilientCaller + emit metric + 503 path

**Files:**
- Modify: `crates/router/src/resilient_caller.rs`
- Modify: `crates/router/src/errors.rs` (or wherever the router error enum lives)
- Modify: `crates/observability/src/metrics/names.rs`

- [ ] **Step 1: Add the metric name constant**

In `crates/observability/src/metrics/names.rs`, append (following the established pattern for the existing 14 metric names):

```rust
/// Plan 06 P04 T4: per-axis cost-filter skip/note counter.
/// Labels: reason ∈ {tier, latency, cap, latency_unknown, tiktoken_fallback},
///         upstream, route.
pub const COST_FILTERED_TOTAL: &str = "agent_shim_cost_filtered_total";
```

- [ ] **Step 2: Add `NoEligibleUpstream` error variant**

In whichever module holds the router's user-visible error type (likely `crates/router/src/errors.rs` or inside `resilient_caller.rs`), add a variant:

```rust
    /// Plan 06 P04 T4: cost filter emptied the entire chain.
    /// `filtered` lists each skipped upstream + the per-axis reason.
    #[error("no eligible upstream after cost filter: {filtered:?}")]
    NoEligibleUpstream {
        filtered: Vec<crate::cost_filter::Skip>,
    },
```

If the router currently surfaces errors as anyhow-wrapped strings, follow the existing convention — the spec only requires that the gateway frontend handler can map this case to HTTP 503.

- [ ] **Step 3: Inject the probe into ResilientCaller**

Change `ResilientCaller` to hold an `Arc<dyn LatencyProbe>`:

```rust
pub struct ResilientCaller {
    // ... existing fields ...
    probe: Arc<dyn crate::latency_probe::LatencyProbe>,
}

impl ResilientCaller {
    pub fn new(
        // ... existing args ...
        probe: Arc<dyn crate::latency_probe::LatencyProbe>,
    ) -> Self {
        Self { /* ... */, probe }
    }
}
```

Update every call site in `resilient_caller.rs::tests` to pass a `MockLatencyProbe` or `DisabledLatencyProbe`. Existing tests don't exercise latency-filter; pass `Arc::new(DisabledLatencyProbe) as Arc<dyn LatencyProbe>`.

- [ ] **Step 4: Pre-chain filter inside `ResilientCaller::call`**

Find the entry point of `ResilientCaller::call` (or whichever method walks the chain). At the very top of the chain-walk, before the existing iteration:

```rust
        // Plan 06 P04 T4: cost filter pass. Skip upstreams that don't
        // meet the route's tier / latency / cost constraints. If the
        // entire chain is filtered, return NoEligibleUpstream before
        // any provider is contacted.
        let outcome = crate::cost_filter::filter_chain(
            chain,
            route,
            request,
            config,
            self.probe.as_ref(),
        );
        for skip in &outcome.skipped {
            metrics::counter!(
                agent_shim_observability::metrics::names::COST_FILTERED_TOTAL,
                "reason" => skip.reason.as_str(),
                "upstream" => skip.upstream.clone(),
                "route" => route_label(route).to_string(),
            )
            .increment(1);
        }
        for note in &outcome.notes {
            metrics::counter!(
                agent_shim_observability::metrics::names::COST_FILTERED_TOTAL,
                "reason" => note.reason.as_str(),
                "upstream" => note.upstream.clone(),
                "route" => route_label(route).to_string(),
            )
            .increment(1);
        }
        if outcome.survivors.is_empty() {
            return Err(/* whichever variant */ ::NoEligibleUpstream {
                filtered: outcome.skipped,
            });
        }
        let chain = outcome.survivors;
        // ... existing chain walk continues here ...
```

`route_label(route)` is a tiny helper that returns `format!("{}/{}", route.frontend, route.model)` — the same format Phase 5's metrics use for the route label.

**Implementer note:** the exact signature of `ResilientCaller::call` may not currently take `&RouteEntry` and `&GatewayConfig` as args — those might need to be added or threaded through. If so, this is an invasive change. Track impact and update tests accordingly.

- [ ] **Step 5: Build & run router tests**

```bash
cargo build -p agent-shim-router 2>&1 | tail -10
cargo test -p agent-shim-router 2>&1 | tail -10
```

Expected: clean. Existing tests pass with `DisabledLatencyProbe` (filter is a no-op when route has no `min_tier`/`max_cost_usd` set and probe returns None for unmeasured upstreams).

- [ ] **Step 6: Commit**

```bash
git add crates/router/src/resilient_caller.rs crates/router/src/errors.rs crates/observability/src/metrics/names.rs
git commit -m "feat(router): wire CostFilter into ResilientCaller + emit cost_filtered_total + NoEligibleUpstream (Plan 04 P04 T4)"
```

---

### Task 5: PrometheusLatencyProbe in gateway

**Files:**
- Create: `crates/gateway/src/latency_probe.rs`
- Modify: `crates/gateway/src/state.rs`
- Modify: `crates/gateway/src/lib.rs`

- [ ] **Step 1: Write the Prometheus probe**

Create `crates/gateway/src/latency_probe.rs`:

```rust
//! Prometheus-backed latency probe. Plan 06 P04 T5.
//!
//! Reads the recent p95 of `agent_shim_upstream_duration_seconds` from
//! the metrics-exporter-prometheus snapshot handle and converts it to
//! milliseconds. Used by ResilientCaller's cost filter when evaluating
//! per-upstream `p95_latency_budget_ms`.

use std::sync::Arc;

use agent_shim_router::LatencyProbe;

pub struct PrometheusLatencyProbe {
    handle: Arc<agent_shim_observability::MetricsHandle>,
}

impl PrometheusLatencyProbe {
    pub fn new(handle: Arc<agent_shim_observability::MetricsHandle>) -> Self {
        Self { handle }
    }
}

impl LatencyProbe for PrometheusLatencyProbe {
    fn recent_p95_ms(&self, upstream: &str) -> Option<u64> {
        // metrics-exporter-prometheus exposes a render() method that
        // returns the current text-format snapshot. We parse out the
        // upstream's _bucket lines and compute p95 in-process.
        //
        // For a tighter integration (avoiding text parse), the
        // alternative is to clone the underlying recorder and snapshot
        // its histogram registry directly. text-parse keeps this
        // implementation isolated from metrics-rs internals and is
        // fast enough — a single regex over a ~10KB scrape body per
        // request is < 1ms.
        //
        // First implementation: text-parse via prometheus-parse.
        let body = self.handle.render();
        compute_p95_from_scrape(&body, upstream)
    }
}

fn compute_p95_from_scrape(body: &str, upstream: &str) -> Option<u64> {
    use prometheus_parse::{Scrape, Value};
    let scrape = Scrape::parse(body.lines().map(|l| Ok(l.to_string()))).ok()?;
    // Collect bucket entries for this upstream's duration histogram.
    let mut buckets: Vec<(f64, u64)> = scrape
        .samples
        .iter()
        .filter(|s| s.metric == "agent_shim_upstream_duration_seconds_bucket")
        .filter(|s| s.labels.get("upstream").map_or(false, |v| v == upstream))
        .filter_map(|s| {
            let le = s.labels.get("le")?.parse::<f64>().ok()?;
            let v = match s.value {
                Value::Counter(c) | Value::Untyped(c) => c as u64,
                _ => return None,
            };
            Some((le, v))
        })
        .collect();
    if buckets.is_empty() {
        return None;
    }
    buckets.sort_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap());
    // The +Inf bucket count = total observations.
    let total = buckets.last()?.1;
    if total == 0 {
        return None;
    }
    let target = (0.95 * total as f64).ceil() as u64;
    let p95 = buckets
        .iter()
        .find(|(_, count)| *count >= target)
        .map(|(le, _)| le)?;
    // p95 is in seconds; convert to ms.
    Some((p95 * 1000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_p95_from_simple_scrape() {
        let scrape = "\
agent_shim_upstream_duration_seconds_bucket{upstream=\"m\",le=\"0.1\"} 5
agent_shim_upstream_duration_seconds_bucket{upstream=\"m\",le=\"0.5\"} 10
agent_shim_upstream_duration_seconds_bucket{upstream=\"m\",le=\"1.0\"} 20
agent_shim_upstream_duration_seconds_bucket{upstream=\"m\",le=\"+Inf\"} 20
";
        let p95 = compute_p95_from_scrape(scrape, "m");
        assert_eq!(p95, Some(1000));
    }

    #[test]
    fn missing_upstream_returns_none() {
        let scrape = "agent_shim_other_metric 1\n";
        assert_eq!(compute_p95_from_scrape(scrape, "m"), None);
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/gateway/src/lib.rs`, add:

```rust
pub mod latency_probe;
```

- [ ] **Step 3: Wire into AppState::build**

In `crates/gateway/src/state.rs`, in `AppState::build`, after the existing `let metrics = ...` line and before the `ResilientCaller::new` call, add:

```rust
        // Plan 06 P04 T5: Prometheus-backed latency probe for the
        // cost filter's latency axis.
        let latency_probe: std::sync::Arc<dyn agent_shim_router::LatencyProbe> =
            std::sync::Arc::new(crate::latency_probe::PrometheusLatencyProbe::new(
                std::sync::Arc::clone(&metrics),
            ));
```

Then pass `Arc::clone(&latency_probe)` to `ResilientCaller::new` as the new probe argument.

- [ ] **Step 4: Update the AppCore-direct test constructors**

Same two test files as P02 T3 (`vision_capability_mismatch.rs`, `responses_capability_gate_deepseek.rs`) — they construct `ResilientCaller::new` directly with a literal arg list. They now need to also pass a probe. Use `Arc::new(agent_shim_router::DisabledLatencyProbe) as _`.

- [ ] **Step 5: Build the workspace**

```bash
cargo build --workspace --tests 2>&1 | tail -10
cargo test --workspace --quiet 2>&1 | grep -E "^test result:" | grep -v "0 failed" | head -5
echo "---"
echo "Any failures above?"
```

Expected: empty. Probe unit tests pass (2 new).

- [ ] **Step 6: Commit**

```bash
git add crates/gateway/src/latency_probe.rs crates/gateway/src/state.rs crates/gateway/src/lib.rs crates/gateway/tests/vision_capability_mismatch.rs crates/gateway/tests/responses_capability_gate_deepseek.rs
git commit -m "feat(gateway): PrometheusLatencyProbe + wire into ResilientCaller (Plan 04 P04 T5)"
```

---

### Task 6: Integration tests — end-to-end cost filter behaviour

**Files:**
- Create: `crates/gateway/tests/cost_filter_full_skip.rs`
- Create: `crates/gateway/tests/cost_filter_tier_partial.rs`
- Create: `crates/gateway/tests/cost_filter_metric_counter.rs`
- Create: `crates/gateway/tests/cost_filter_no_eligible_503.rs` (combined into _full_skip if cleaner)

- [ ] **Step 1: Write `cost_filter_full_skip.rs`**

Test that when the chain is fully filtered (all upstreams below `min_tier`), the gateway returns 503 with the per-axis reason list.

```rust
//! Plan 04 P04 T6: full-skip case — every upstream in the chain is
//! filtered out by min_tier, gateway returns 503 NoEligibleUpstream.

use std::net::SocketAddr;

async fn pick_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
        .local_addr().unwrap().port()
}

#[tokio::test]
async fn full_chain_filtered_returns_503() {
    let public = pick_port().await;
    let yaml = format!(
        r#"
server: {{bind: 127.0.0.1, port: {public}}}
upstreams:
  eco:
    type: open_ai_compatible
    base_url: http://localhost:9999/v1
    api_key: dummy
    tier: economy
  std:
    type: open_ai_compatible
    base_url: http://localhost:9999/v1
    api_key: dummy
    tier: standard
routes:
  - frontend: openai_chat
    model: x
    upstreams: [eco, std]
    upstream_model: x
    min_tier: premium
"#);
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(&yaml).unwrap();
    let (state, _reload_rx) = agent_shim_gateway::state::AppState::new(cfg).await;
    let public_addr: SocketAddr = format!("127.0.0.1:{public}").parse().unwrap();
    let listener = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let app = agent_shim_gateway::server::build_router(state);
    tokio::spawn(async move { let _ = axum::serve(listener, app).await; });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // ⚠ Validation: rule 17 should have caught this at startup.
    // This test only reaches the runtime path if the operator
    // *changed* tier values via reload such that no chain element
    // satisfies min_tier. Adapt: explicitly bypass rule 17 by setting
    // min_tier *after* AppState::new (manually via reload, or skip
    // this assertion in favour of a runtime-only path).
    //
    // Actually: rule 17 fires at startup, which means this test
    // configuration CANNOT load. The test only makes sense at the
    // reload path. So the assertion sequence is:
    //   1. start with valid config (min_tier: standard, both upstreams in chain)
    //   2. POST /admin/reload with a *new* min_tier: premium config
    //      → reload fails with 400 (rule 17 again)
    //   3. ... actually the only way to reach runtime full-filter is
    //      via a tier change that doesn't violate rule 17 statically
    //      but does at runtime (e.g. latency probe over budget on
    //      every upstream)
    //
    // Simpler: test the latency-filter full-skip case via mocking
    // latency budgets that exceed every probe value.
    //
    // [Implementer: pick latency-axis full-skip as the test rather
    // than tier-axis full-skip, because rule 17 prevents tier-axis
    // full-skip from being reachable at startup. Adapt the YAML.]
    todo!("see comment above — switch this test to latency-axis full-skip");
}
```

**Implementer:** as the comment notes, tier-axis full-skip can't be reached at startup (rule 17 prevents it). Change the test to drive a latency-axis full-skip:

- Configure both upstreams with `p95_latency_budget_ms: 1` (1ms budget — every real upstream blows past this).
- Force the metric exporter to record a sample for each upstream above 1ms before the test request (test setup detail).
- The cost filter sees `recent_p95_ms > 1`, skips both, returns 503.

OR — simpler — use cost-cap full-skip:

- Set both upstreams' `cost: {input: 1000.0, output: 1000.0}` (extremely expensive).
- Set route `max_cost_usd: 0.0001`.
- Any non-trivial request exceeds the cap → both upstreams skipped → 503.

The cost-cap path is simpler because it doesn't require pre-seeding the metrics handle. Use that.

```rust
    // Effective test: cost-cap forces every upstream over the cap.
    let yaml = format!(
        r#"
server: {{bind: 127.0.0.1, port: {public}}}
upstreams:
  a:
    type: open_ai_compatible
    base_url: http://localhost:9999/v1
    api_key: dummy
    tier: standard
    cost:
      input_per_million_usd: 1000000.0
      output_per_million_usd: 1000000.0
  b:
    type: open_ai_compatible
    base_url: http://localhost:9999/v1
    api_key: dummy
    tier: standard
    cost:
      input_per_million_usd: 1000000.0
      output_per_million_usd: 1000000.0
routes:
  - frontend: openai_chat
    model: x
    upstreams: [a, b]
    upstream_model: x
    max_cost_usd: 0.0001
"#);
    // ... same boot dance ...

    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .body(r#"{"model":"x","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503, "all upstreams over cost cap → 503");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], serde_json::Value::Bool(false));
    let filtered = body["filtered"].as_array().expect("filtered list present");
    assert_eq!(filtered.len(), 2, "both upstreams listed as filtered");
    assert!(filtered.iter().all(|e| e["reason"] == "cap"));
```

- [ ] **Step 2: Write `cost_filter_tier_partial.rs`**

Two upstreams: `eco` (economy) and `std` (standard). Route min_tier=standard. First upstream `eco` is filtered; `std` is the survivor and the chain walks there. The actual outbound HTTP fails (unreachable mock), but the test asserts that **eco** was never called and **std** was — by, say, two separate mockito servers that record their hits.

- [ ] **Step 3: Write `cost_filter_metric_counter.rs`**

After driving a filter-skip request, scrape `/metrics` and assert `agent_shim_cost_filtered_total{reason="cap"}` or similar is present in the body with a value ≥ 1.

- [ ] **Step 4: Run all 3 integration tests**

```bash
cargo test -p agent-shim --test cost_filter_full_skip --test cost_filter_tier_partial --test cost_filter_metric_counter 2>&1 | tail -15
```

Expected: 3 passing.

- [ ] **Step 5: Commit**

```bash
git add crates/gateway/tests/cost_filter_*.rs
git commit -m "test(gateway): cost filter end-to-end + metric counter (Plan 04 P04 T6)"
```

---

### Task 7: Spec compliance + code quality review

- [ ] **Step 1: Reviewer dispatch (spec compliance)**

> Review commits Plan 04 P04 T1..T6 against spec `docs/superpowers/specs/2026-05-12-phase-6-cost-aware-gateway-design.md`. Verify:
>
> 1. **§4 D1 ordering** — CostFilter runs AFTER RateLimit but BEFORE ResilientCaller's existing chain walk. Trace the call sequence in pipeline.rs / resilient_caller.rs to confirm.
> 2. **§4 D2 boundary** — router does not depend on observability. Confirm `crates/router/Cargo.toml` has no observability dep. `PrometheusLatencyProbe` lives in gateway, not router.
> 3. **§4 D3 conservative estimate** — `cost_estimate.rs` uses `tiktoken-rs` for input + request.max_tokens (defaulting 4096) for output. Verify.
> 4. **§4 D5 metric labels** — `agent_shim_cost_filtered_total{reason, upstream, route}` is the label set. Reason variants: tier, latency, cap, latency_unknown, tiktoken_fallback (5 values). Confirm via `cargo test --test cost_filter_metric_counter`.
> 5. **§6.3 503 envelope** — full-filter case returns 503 with `{ok:false, errors:[...], filtered:[{upstream, reason}, ...]}`. Confirm shape in the integration test.
> 6. **§3.2 boundary** — `providers/src/` is NOT touched by this plan. Diff is unchanged from P01.
> 7. **Tests** — 5 unit tests in `cost_filter.rs`, 2 unit tests in `cost_estimate.rs`, 2 unit tests in `latency_probe.rs` (router) + 2 unit tests in gateway's `latency_probe.rs` + 3 integration tests in gateway/tests. Total +9 from P03's 650. Workspace = 659.

- [ ] **Step 2: Reviewer dispatch (code quality)**

> Review commits Plan 04 P04 T1..T6 for code quality.
>
> 1. The `upstream_config_tier/cost/latency_budget` helpers in `cost_filter.rs` duplicate the variant match logic from P03's `validation.rs`. Worth a shared trait or extension impl?
> 2. `PrometheusLatencyProbe::recent_p95_ms` parses the entire scrape body on every call (typically per-request). That's ~1KB-10KB text parse per request — measure overhead.
> 3. The `tokio::time::sleep(100ms)` boot delay in the integration tests — establish whether the gateway is ready faster via `/healthz` poll instead.
> 4. `FilterReason::as_str` repeats the constants from `metrics::names::COST_FILTERED_TOTAL` reason labels. Ensure they match — the metric_names_match_observability.rs router test (from v0.5) should grow to cover these too.
> 5. `NoEligibleUpstream` error variant — does it surface cleanly through the v0.4 error envelope on each frontend (Anthropic, OpenAI Chat, OpenAI Responses)? Each frontend has its own error shape; confirm all three map to a 503 with the same JSON body.
> 6. The full-skip integration test still calls the gateway through real HTTP. Could a thinner integration that drives `pipeline::dispatch` directly catch the same bugs with less flakiness?

- [ ] **Step 3: Apply CRITICAL/HIGH findings**

```bash
git commit -m "fix(router): address P04 review findings"
```

---

## Done when

- [ ] Workspace test count ≥ 659 (was 650, +9: 5 cost_filter unit + 2 cost_estimate unit + 2 latency_probe unit (router) + 2 latency_probe unit (gateway) + 3 integration; double-count adjustment because some sub-totals overlap → settle on 659 actual).
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] `git diff a0139fc -- crates/core/ crates/frontends/` empty.
- [ ] `crates/providers/src/` diff equals P01's diff (no P04 change).
- [ ] `agent_shim_cost_filtered_total{reason,upstream,route}` visible on `/metrics` after a filtered request.
- [ ] Full-filter case returns HTTP 503 with the documented JSON envelope (`ok:false`, `errors`, `filtered`).
- [ ] Partial-filter case correctly skips filtered upstreams and walks the remaining chain.
- [ ] T7 reviews clear of CRITICAL/HIGH findings.
