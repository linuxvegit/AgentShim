# Plan 01 — Config Schema + Per-Route Retries (Phase 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-08-phase-4-resiliency-design.md`](../specs/2026-05-08-phase-4-resiliency-design.md) (decisions D2, D3, D5, D12).

**Goal:** Land the new `RouteEntry` shape (singular `upstream`/`upstream_model` and array `upstreams` coexist) and a per-route retry policy with exponential backoff + jitter + total-time budget. **No fallback yet** — `upstreams` arrays of length > 1 are accepted by config but only the first element is called. Plan 02 builds on this.

**Architecture:** New `crates/router/src/retry.rs` with a pure `compute_backoff` function and a `retry_with_policy` helper that wraps a single `provider.complete()` call. `crates/router/src/fallback.rs` gains a private `fallback_eligibility` classifier (a fallback chain isn't walked yet, but the classifier is wired into the retry loop so non-retryable errors short-circuit cleanly). `crates/config/src/schema.rs` gains `upstreams`, `retry`, `breaker` fields on `RouteEntry`; both shapes coexist.

**Tech stack:** No new dependencies. `rand` (already a transitive dep) provides the jitter RNG.

**Frontend changes:** NONE.
**Provider code changes:** NONE (provider docs only).
**Core changes:** NONE.

---

## File Structure

`crates/config/src/`:
- Modify: `schema.rs` — `RouteEntry` gains `upstreams`, `retry`, `breaker` fields.
- Modify: `validation.rs` — new validation rules.

`crates/router/src/`:
- Create: `retry.rs` — `RetryPolicy`, `compute_backoff`, `retry_with_policy`.
- Modify: `fallback.rs` — replace stub with `FallbackEligibility` enum + `fallback_eligibility()` function.
- Modify: `lib.rs` — re-export `RetryPolicy`, `FallbackEligibility`.

`crates/gateway/src/`:
- Modify: `pipeline.rs` — replace direct `provider.complete()` call with `retry_with_policy(provider, target, req, &policy).await`.

`docs/providers/`:
- Modify: `anthropic.md`, `openai-compatible.md`, `gemini.md`, `deepseek.md`, `github-copilot.md` — each gains a "Resilience behavior" subsection.

---

## Tasks

### Task 1: schema additions

**Files:**
- Modify: `crates/config/src/schema.rs`
- Modify: `crates/config/src/validation.rs`

- [ ] **Step 1: Write failing schema test**

Add to `crates/config/src/schema.rs` tests module:

```rust
#[test]
fn route_entry_array_form_deserializes() {
    let yaml = r#"
        frontend: openai_chat
        model: gpt-4o
        upstreams:
          - {name: openai, model: gpt-4o-2024-11-20}
          - {name: copilot, model: gpt-4o}
        retry:
          max_attempts: 3
          total_budget_ms: 8000
    "#;
    let entry: RouteEntry = serde_yaml::from_str(yaml).expect("parses");
    assert_eq!(entry.upstreams.len(), 2);
    assert_eq!(entry.upstreams[0].name, "openai");
    assert_eq!(entry.retry.max_attempts, 3);
    assert_eq!(entry.upstream, None);
}

#[test]
fn route_entry_singular_form_still_works() {
    let yaml = r#"
        frontend: openai_chat
        model: gpt-4o
        upstream: openai
        upstream_model: gpt-4o-2024-11-20
    "#;
    let entry: RouteEntry = serde_yaml::from_str(yaml).expect("parses");
    assert_eq!(entry.upstream.as_deref(), Some("openai"));
    assert_eq!(entry.upstreams.len(), 0);  // empty when singular
    assert_eq!(entry.retry.max_attempts, 2); // default per D12
}
```

- [ ] **Step 2: Run test, expect failure**

```bash
rtk cargo test -p agent-shim-config --quiet
```

Expected: compile error (`upstreams`, `retry` fields don't exist on `RouteEntry`).

- [ ] **Step 3: Add new types and fields**

Replace the `RouteEntry` struct in `crates/config/src/schema.rs`:

```rust
/// A single route mapping. Supports two shapes:
/// - **Singular** (v0.3 compat): `upstream` + `upstream_model`.
/// - **Array** (v0.4): `upstreams: [{name, model}, ...]` for fallback chains.
/// Both shapes are mutually exclusive on the same route; validation enforces it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteEntry {
    pub frontend: String,
    pub model: String,

    // ── Singular shape (v0.3) ──────────────────────────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_model: Option<String>,

    // ── Array shape (v0.4) ─────────────────────────────────────────────────
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<UpstreamRef>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_beta: Option<String>,

    // ── Per-route resilience policy (v0.4) ─────────────────────────────────
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub breaker: BreakerConfig,
}

/// Reference to one upstream in a fallback chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamRef {
    pub name: String,
    pub model: String,
}

/// Per-route retry policy. All fields have defaults from §4.5 of the spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_initial_backoff_ms")]
    pub initial_backoff_ms: u64,
    #[serde(default = "default_multiplier")]
    pub multiplier: f64,
    #[serde(default = "default_jitter_pct")]
    pub jitter_pct: f64,
    #[serde(default = "default_total_budget_ms")]
    pub total_budget_ms: u64,
    #[serde(default = "default_retry_on")]
    pub retry_on: Vec<String>,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            initial_backoff_ms: default_initial_backoff_ms(),
            multiplier: default_multiplier(),
            jitter_pct: default_jitter_pct(),
            total_budget_ms: default_total_budget_ms(),
            retry_on: default_retry_on(),
        }
    }
}

fn default_max_attempts() -> u32 { 2 }
fn default_initial_backoff_ms() -> u64 { 100 }
fn default_multiplier() -> f64 { 2.0 }
fn default_jitter_pct() -> f64 { 25.0 }
fn default_total_budget_ms() -> u64 { 5000 }
fn default_retry_on() -> Vec<String> {
    vec!["network".into(), "upstream_5xx".into(), "upstream_429".into()]
}

/// Per-route circuit-breaker policy. Plan 03 wires the actual state machine;
/// Plan 01 just lands the schema fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BreakerConfig {
    #[serde(default = "default_breaker_enabled")]
    pub enabled: bool,
    #[serde(default = "default_failure_threshold_pct")]
    pub failure_threshold_pct: u32,
    #[serde(default = "default_min_requests")]
    pub min_requests: u32,
    #[serde(default = "default_window_secs")]
    pub window_secs: u64,
    #[serde(default = "default_open_cooldown_secs")]
    pub open_cooldown_secs: u64,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            enabled: default_breaker_enabled(),
            failure_threshold_pct: default_failure_threshold_pct(),
            min_requests: default_min_requests(),
            window_secs: default_window_secs(),
            open_cooldown_secs: default_open_cooldown_secs(),
        }
    }
}

fn default_breaker_enabled() -> bool { true }
fn default_failure_threshold_pct() -> u32 { 50 }
fn default_min_requests() -> u32 { 20 }
fn default_window_secs() -> u64 { 60 }
fn default_open_cooldown_secs() -> u64 { 30 }
```

- [ ] **Step 4: Run schema tests**

```bash
rtk cargo test -p agent-shim-config --quiet
```

Expected: PASS.

- [ ] **Step 5: Add validation rules**

In `crates/config/src/validation.rs`, add a function `validate_routes(cfg: &GatewayConfig) -> Result<(), String>` (or extend the existing one):

```rust
pub fn validate_routes(cfg: &GatewayConfig) -> Result<(), String> {
    for (i, route) in cfg.routes.iter().enumerate() {
        let has_singular = route.upstream.is_some() || route.upstream_model.is_some();
        let has_array = !route.upstreams.is_empty();

        // Rule 1: exactly one shape per route.
        match (has_singular, has_array) {
            (true, true) => {
                return Err(format!(
                    "route[{i}] '{}/{}' specifies both `upstream`/`upstream_model` and `upstreams`; pick one form",
                    route.frontend, route.model
                ));
            }
            (false, false) => {
                return Err(format!(
                    "route[{i}] '{}/{}' has no upstream configured; provide either `upstream`+`upstream_model` or `upstreams`",
                    route.frontend, route.model
                ));
            }
            (true, false) => {
                if route.upstream.is_none() || route.upstream_model.is_none() {
                    return Err(format!(
                        "route[{i}] '{}/{}' singular form requires both `upstream` and `upstream_model`",
                        route.frontend, route.model
                    ));
                }
            }
            (false, true) => {} // valid array form
        }

        // Rule 2: every referenced upstream name must be configured.
        let upstream_names: Vec<&str> = if has_array {
            route.upstreams.iter().map(|u| u.name.as_str()).collect()
        } else {
            vec![route.upstream.as_deref().unwrap()]
        };
        for name in upstream_names {
            if !cfg.upstreams.contains_key(name) {
                return Err(format!(
                    "route[{i}] '{}/{}' references unknown upstream '{}'",
                    route.frontend, route.model, name
                ));
            }
        }

        // Rule 3-6: retry policy sanity.
        if route.retry.max_attempts < 1 {
            return Err(format!("route[{i}] retry.max_attempts must be >= 1"));
        }
        if route.retry.total_budget_ms < route.retry.initial_backoff_ms {
            return Err(format!(
                "route[{i}] retry.total_budget_ms ({}) must be >= initial_backoff_ms ({})",
                route.retry.total_budget_ms, route.retry.initial_backoff_ms
            ));
        }
        if route.retry.multiplier <= 1.0 {
            return Err(format!(
                "route[{i}] retry.multiplier ({}) must be > 1.0 for exponential backoff",
                route.retry.multiplier
            ));
        }
        if !(1..=100).contains(&route.breaker.failure_threshold_pct) {
            return Err(format!(
                "route[{i}] breaker.failure_threshold_pct ({}) must be in 1..=100",
                route.breaker.failure_threshold_pct
            ));
        }
        if route.breaker.min_requests < 1 {
            return Err(format!("route[{i}] breaker.min_requests must be >= 1"));
        }
    }
    Ok(())
}
```

- [ ] **Step 6: Add validation tests**

In `crates/config/src/validation.rs` tests module:

```rust
#[test]
fn rejects_route_with_both_shapes() {
    let cfg = make_cfg_with_route_yaml(r#"
        frontend: openai_chat
        model: gpt-4o
        upstream: openai
        upstream_model: gpt-4o
        upstreams:
          - {name: copilot, model: gpt-4o}
    "#);
    let err = validate_routes(&cfg).unwrap_err();
    assert!(err.contains("specifies both"));
}

#[test]
fn rejects_route_with_no_upstream() {
    let cfg = make_cfg_with_route_yaml(r#"
        frontend: openai_chat
        model: gpt-4o
    "#);
    let err = validate_routes(&cfg).unwrap_err();
    assert!(err.contains("no upstream configured"));
}

#[test]
fn rejects_unknown_upstream_reference() {
    let cfg = make_cfg_with_route_yaml(r#"
        frontend: openai_chat
        model: gpt-4o
        upstreams:
          - {name: nonexistent, model: gpt-4o}
    "#);
    // make_cfg_with_route_yaml configures only "openai" in cfg.upstreams.
    let err = validate_routes(&cfg).unwrap_err();
    assert!(err.contains("nonexistent"));
}

#[test]
fn accepts_well_formed_array_route() {
    let cfg = make_cfg_with_route_yaml(r#"
        frontend: openai_chat
        model: gpt-4o
        upstreams:
          - {name: openai, model: gpt-4o-2024-11-20}
    "#);
    validate_routes(&cfg).expect("valid config");
}

#[test]
fn rejects_multiplier_le_one() {
    let cfg = make_cfg_with_route_yaml(r#"
        frontend: openai_chat
        model: gpt-4o
        upstream: openai
        upstream_model: gpt-4o
        retry:
          multiplier: 1.0
    "#);
    let err = validate_routes(&cfg).unwrap_err();
    assert!(err.contains("multiplier"));
}
```

The `make_cfg_with_route_yaml` helper builds a `GatewayConfig` with a single configured upstream `openai` and parses one route from the YAML snippet.

- [ ] **Step 7: Run all config tests**

```bash
rtk cargo test -p agent-shim-config --quiet
```

Expected: all PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/config/src/schema.rs crates/config/src/validation.rs
git commit -m "feat(config): RouteEntry gains upstreams array + retry/breaker blocks (Plan 04 P01 T1)

Adds the new v0.4 shape (per D2 + D5 + D12 of the Phase 4 spec):
- RouteEntry.upstreams: Vec<UpstreamRef> coexists with singular
  upstream/upstream_model. Validation rejects mixed configs.
- New RetryConfig and BreakerConfig structs with §4.5 defaults
  (max_attempts=2, initial_backoff_ms=100, multiplier=2.0,
  jitter_pct=25, total_budget_ms=5000; breaker enabled by default
  with sliding-window 50%/20/60s/30s).
- BreakerConfig fields land in v0.4.0 P01 but the state machine
  arrives in P03; validation accepts them in the meantime.
- 5 new validation tests pin every (singular, array, both, neither,
  multiplier) edge case (R5)."
```

---

### Task 2: RetryPolicy + compute_backoff

**Files:**
- Create: `crates/router/src/retry.rs`
- Modify: `crates/router/src/lib.rs`
- Modify: `crates/router/Cargo.toml` (add `rand` if not already present).

- [ ] **Step 1: Add rand dep if missing**

Inspect `crates/router/Cargo.toml`. If `rand` is not in `[dependencies]` or `[dev-dependencies]`, add to `[dependencies]`:

```toml
rand = { version = "0.8", default-features = false, features = ["std", "std_rng", "small_rng"] }
```

(`small_rng` is the seedable RNG we use in tests; `std_rng` is the production default.)

- [ ] **Step 2: Write failing tests**

Create `crates/router/src/retry.rs`:

```rust
//! Retry policy and exponential-backoff math (Plan 04 P01).
//!
//! `compute_backoff` is pure (deterministic given a seeded RNG) so it can be
//! property-tested. The retry loop itself lives in `retry_with_policy`, which
//! drives a `BackendProvider::complete()` call up to `policy.max_attempts`
//! times within `policy.total_budget_ms`.

use std::time::Duration;

use rand::Rng;

/// Effective per-route retry policy, derived from `agent_shim_config::RetryConfig`.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_backoff_ms: u64,
    pub multiplier: f64,
    pub jitter_pct: f64,
    pub total_budget_ms: u64,
    pub retry_on: Vec<String>,
}

impl From<&agent_shim_config::RetryConfig> for RetryPolicy {
    fn from(c: &agent_shim_config::RetryConfig) -> Self {
        Self {
            max_attempts: c.max_attempts,
            initial_backoff_ms: c.initial_backoff_ms,
            multiplier: c.multiplier,
            jitter_pct: c.jitter_pct,
            total_budget_ms: c.total_budget_ms,
            retry_on: c.retry_on.clone(),
        }
    }
}

/// Compute the backoff duration for a 1-indexed retry attempt under the
/// configured policy, applying jitter via the supplied RNG. Pure function;
/// deterministic given the same `rng` state.
///
/// Output is bounded to `[base × (1 - jitter%/100), base × (1 + jitter%/100)]`
/// where `base = initial_backoff_ms × multiplier^(attempt-1)`.
pub fn compute_backoff<R: Rng>(attempt: u32, policy: &RetryPolicy, rng: &mut R) -> Duration {
    debug_assert!(attempt >= 1, "attempt is 1-indexed");
    let exponent = (attempt - 1) as i32;
    let base_ms = policy.initial_backoff_ms as f64 * policy.multiplier.powi(exponent);
    let jitter_factor = if policy.jitter_pct == 0.0 {
        1.0
    } else {
        let span = policy.jitter_pct / 100.0;
        1.0 + rng.gen_range(-span..=span)
    };
    let final_ms = (base_ms * jitter_factor).round().max(0.0) as u64;
    Duration::from_millis(final_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rand::rngs::SmallRng;
    use rand::SeedableRng;

    fn default_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 2,
            initial_backoff_ms: 100,
            multiplier: 2.0,
            jitter_pct: 25.0,
            total_budget_ms: 5000,
            retry_on: vec!["network".into(), "upstream_5xx".into(), "upstream_429".into()],
        }
    }

    #[test]
    fn compute_backoff_with_zero_jitter_is_deterministic() {
        let policy = RetryPolicy {
            jitter_pct: 0.0,
            ..default_policy()
        };
        let mut rng = SmallRng::seed_from_u64(42);
        // attempt=1 → 100 × 2^0 = 100ms
        // attempt=2 → 100 × 2^1 = 200ms
        // attempt=3 → 100 × 2^2 = 400ms
        assert_eq!(compute_backoff(1, &policy, &mut rng), Duration::from_millis(100));
        assert_eq!(compute_backoff(2, &policy, &mut rng), Duration::from_millis(200));
        assert_eq!(compute_backoff(3, &policy, &mut rng), Duration::from_millis(400));
    }

    proptest! {
        #[test]
        fn compute_backoff_jitter_stays_within_bounds(
            attempt in 1u32..=10u32,
            seed in any::<u64>(),
        ) {
            let policy = default_policy();
            let mut rng = SmallRng::seed_from_u64(seed);
            let dur = compute_backoff(attempt, &policy, &mut rng);
            let base_ms = policy.initial_backoff_ms as f64
                * policy.multiplier.powi((attempt - 1) as i32);
            let lo = (base_ms * (1.0 - policy.jitter_pct / 100.0)).floor() as u64;
            let hi = (base_ms * (1.0 + policy.jitter_pct / 100.0)).ceil() as u64;
            let got = dur.as_millis() as u64;
            prop_assert!(
                got >= lo && got <= hi,
                "attempt={attempt} got={got}ms expected [{lo}, {hi}]"
            );
        }
    }
}
```

- [ ] **Step 3: Wire module into lib.rs**

Add to `crates/router/src/lib.rs`:

```rust
pub mod retry;

pub use retry::{compute_backoff, RetryPolicy};
```

Add `proptest` to `crates/router/Cargo.toml` `[dev-dependencies]` (already used elsewhere in the workspace; if missing, add):

```toml
proptest.workspace = true
```

- [ ] **Step 4: Run tests**

```bash
rtk cargo test -p agent-shim-router --quiet
```

Expected: PASS (deterministic test + property test with ~256 cases).

- [ ] **Step 5: Commit**

```bash
git add crates/router/src/retry.rs crates/router/src/lib.rs crates/router/Cargo.toml
git commit -m "feat(router): RetryPolicy + compute_backoff (Plan 04 P01 T2)

New crates/router/src/retry.rs:
- RetryPolicy struct with From<&agent_shim_config::RetryConfig>
- compute_backoff(attempt, policy, rng) — pure exponential backoff
  with multiplicative jitter
- Property test asserts every output stays in
  [base × (1 - jitter%), base × (1 + jitter%)] for the §4.5 defaults
- Deterministic test pins zero-jitter sequence to 100/200/400ms"
```

---

### Task 3: fallback_eligibility classifier

**Files:**
- Modify: `crates/router/src/fallback.rs` (currently a one-line stub).
- Modify: `crates/router/src/lib.rs`.

- [ ] **Step 1: Replace stub with skeleton + failing tests**

Replace `crates/router/src/fallback.rs` (which is currently `// Stub for Phase 4 fallback routing.`):

```rust
//! Fallback chain support (Plan 04 P01 T3, P02 T*).
//!
//! P01 lands the error classifier (`fallback_eligibility`) used by the
//! retry loop to short-circuit non-retryable errors. The chain walker
//! itself arrives in P02 when `ResilientCaller` is introduced.

use agent_shim_providers::ProviderError;

/// Whether a `ProviderError` should trigger a fallback to the next upstream
/// in the chain (or, in the P01 single-upstream case, whether the retry
/// loop should give up).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackEligibility {
    /// The error is transient or load-related; try the next upstream.
    Eligible,
    /// The error is terminal; surface it to the client without retry/fallback.
    Terminal,
}

/// Default mapping per §5.4 of the Phase 4 design (D3). Operators can
/// override this per route via `retry.retry_on`.
pub fn fallback_eligibility(e: &ProviderError) -> FallbackEligibility {
    use FallbackEligibility::*;
    match e {
        ProviderError::Network(_) => Eligible,
        ProviderError::Upstream { status, .. } if *status >= 500 => Eligible,
        ProviderError::Upstream { status, .. } if *status == 429 => Eligible,
        ProviderError::Upstream { .. } => Terminal,           // 4xx (non-429)
        ProviderError::Decode(_) => Terminal,                 // upstream is broken;
                                                              // next upstream wouldn't help
        ProviderError::Encode(_) => Terminal,                 // gateway-side
        ProviderError::CapabilityMismatch(_) => Terminal,
        ProviderError::UnknownProvider(_) => Terminal,        // config bug
    }
}

/// Same logic but consulting a per-route override list. The override list
/// uses string tags matching the canonical YAML names: `network`,
/// `upstream_5xx`, `upstream_429`, `decode`, `encode`, `capability`.
pub fn fallback_eligibility_with_overrides(
    e: &ProviderError,
    retry_on: &[String],
) -> FallbackEligibility {
    use FallbackEligibility::*;
    let tag = error_tag(e);
    if retry_on.iter().any(|t| t == tag) {
        Eligible
    } else {
        Terminal
    }
}

fn error_tag(e: &ProviderError) -> &'static str {
    match e {
        ProviderError::Network(_) => "network",
        ProviderError::Upstream { status, .. } if *status >= 500 => "upstream_5xx",
        ProviderError::Upstream { status, .. } if *status == 429 => "upstream_429",
        ProviderError::Upstream { .. } => "upstream_4xx",
        ProviderError::Decode(_) => "decode",
        ProviderError::Encode(_) => "encode",
        ProviderError::CapabilityMismatch(_) => "capability",
        ProviderError::UnknownProvider(_) => "unknown_provider",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FallbackEligibility::*;

    #[test]
    fn classifier_default_mapping() {
        assert_eq!(fallback_eligibility(&ProviderError::Network("x".into())), Eligible);
        assert_eq!(
            fallback_eligibility(&ProviderError::Upstream { status: 500, body: "x".into() }),
            Eligible
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Upstream { status: 503, body: "x".into() }),
            Eligible
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Upstream { status: 429, body: "x".into() }),
            Eligible
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Upstream { status: 400, body: "x".into() }),
            Terminal
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Upstream { status: 401, body: "x".into() }),
            Terminal
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Decode("x".into())),
            Terminal
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::Encode("x".into())),
            Terminal
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::CapabilityMismatch("x".into())),
            Terminal
        );
        assert_eq!(
            fallback_eligibility(&ProviderError::UnknownProvider("x".into())),
            Terminal
        );
    }

    #[test]
    fn override_list_promotes_decode_to_eligible() {
        let retry_on = vec!["decode".to_string(), "network".into()];
        assert_eq!(
            fallback_eligibility_with_overrides(&ProviderError::Decode("x".into()), &retry_on),
            Eligible
        );
        // 5xx no longer in override → terminal under override semantics.
        assert_eq!(
            fallback_eligibility_with_overrides(
                &ProviderError::Upstream { status: 500, body: "x".into() },
                &retry_on
            ),
            Terminal
        );
    }
}
```

- [ ] **Step 2: Wire module exports**

In `crates/router/src/lib.rs`, replace the existing `pub mod fallback;` line (or add if missing) and add re-export:

```rust
pub mod fallback;

pub use fallback::{fallback_eligibility, fallback_eligibility_with_overrides, FallbackEligibility};
```

- [ ] **Step 3: Add agent-shim-providers dep to router Cargo.toml**

Inspect `crates/router/Cargo.toml`. If `agent-shim-providers` is not in `[dependencies]`, add:

```toml
agent-shim-providers = { path = "../providers" }
```

This pulls in `ProviderError`. (Router today depends on agent-shim-core only; Phase 4 adds the providers dep so the resilience layer can match on provider errors.)

- [ ] **Step 4: Run tests**

```bash
rtk cargo test -p agent-shim-router --quiet
```

Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/router/src/fallback.rs crates/router/src/lib.rs crates/router/Cargo.toml
git commit -m "feat(router): fallback_eligibility classifier (Plan 04 P01 T3)

Replaces the v0.3 fallback.rs stub with FallbackEligibility +
fallback_eligibility() + fallback_eligibility_with_overrides()
implementing D3 from the Phase 4 design:
- Network → Eligible
- Upstream 5xx → Eligible
- Upstream 429 → Eligible
- Upstream other 4xx → Terminal
- Decode / Encode / CapabilityMismatch / UnknownProvider → Terminal

Adds agent-shim-providers as a router dep so we can match on
ProviderError. Per-route retry_on overrides will be consulted in
T5 when integrating into the retry loop.

Chain walker comes in P02 with ResilientCaller."
```

---

### Task 4: retry_with_policy helper

**Files:**
- Modify: `crates/router/src/retry.rs`.
- Modify: `crates/router/Cargo.toml` (add `tokio` and `tracing` if not present).

- [ ] **Step 1: Append failing test to retry.rs**

Add to `crates/router/src/retry.rs` tests module:

```rust
    use agent_shim_core::{
        BackendTarget, CanonicalRequest, CanonicalStream, ContentBlock, ExtensionMap,
        FrontendInfo, FrontendKind, FrontendModel, GenerationOptions, Message, RequestId,
        ResolvedPolicy, request::RequestMetadata,
    };
    use agent_shim_providers::{BackendProvider, ProviderCapabilities, ProviderError};
    use async_trait::async_trait;
    use futures::stream;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// MockProvider that returns scripted results in order.
    struct MockProvider {
        results: Arc<Vec<Result<&'static str, ProviderError>>>,
        cursor: Arc<AtomicUsize>,
        capabilities: ProviderCapabilities,
    }

    impl MockProvider {
        fn new(scripted: Vec<Result<&'static str, ProviderError>>) -> Self {
            Self {
                results: Arc::new(scripted),
                cursor: Arc::new(AtomicUsize::new(0)),
                capabilities: ProviderCapabilities {
                    streaming: true,
                    tool_use: false,
                    vision: false,
                    json_mode: false,
                },
            }
        }

        fn calls(&self) -> usize {
            self.cursor.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl BackendProvider for MockProvider {
        fn name(&self) -> &'static str { "mock" }
        fn capabilities(&self) -> &ProviderCapabilities { &self.capabilities }
        async fn complete(
            &self,
            _req: CanonicalRequest,
            _target: BackendTarget,
        ) -> Result<CanonicalStream, ProviderError> {
            let idx = self.cursor.fetch_add(1, Ordering::SeqCst);
            // Clone the scripted result; each call consumes one slot.
            let result = self
                .results
                .get(idx)
                .ok_or_else(|| ProviderError::Network("mock script exhausted".into()))?;
            match result {
                Ok(_text) => Ok(Box::pin(stream::iter(vec![]))),
                Err(e) => Err(e.clone()),
            }
        }
    }

    fn dummy_request() -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::OpenAiChat,
                requested_model: FrontendModel::from("gpt-4o"),
            },
            model: FrontendModel::from("gpt-4o"),
            system: vec![],
            messages: vec![Message::user(vec![ContentBlock::text("hi")])],
            tools: vec![],
            tool_choice: Default::default(),
            generation: GenerationOptions::default(),
            response_format: None,
            stream: false,
            metadata: RequestMetadata::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: ResolvedPolicy::default(),
            extensions: ExtensionMap::new(),
        }
    }

    fn dummy_target() -> BackendTarget {
        BackendTarget {
            provider: "mock".into(),
            model: "gpt-4o".into(),
            policy: Default::default(),
        }
    }

    fn fast_policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            initial_backoff_ms: 1,    // tests must be fast
            multiplier: 2.0,
            jitter_pct: 0.0,           // deterministic
            total_budget_ms: 1000,
            retry_on: vec!["network".into(), "upstream_5xx".into(), "upstream_429".into()],
        }
    }

    #[tokio::test]
    async fn retry_with_policy_succeeds_on_first_attempt() {
        let provider = Arc::new(MockProvider::new(vec![Ok("first")]));
        let result = retry_with_policy(
            provider.clone(),
            dummy_target(),
            dummy_request(),
            &fast_policy(3),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(provider.calls(), 1);
    }

    #[tokio::test]
    async fn retry_with_policy_retries_eligible_error_then_succeeds() {
        let provider = Arc::new(MockProvider::new(vec![
            Err(ProviderError::Upstream { status: 502, body: "bad".into() }),
            Ok("after retry"),
        ]));
        let result = retry_with_policy(
            provider.clone(),
            dummy_target(),
            dummy_request(),
            &fast_policy(3),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(provider.calls(), 2);
    }

    #[tokio::test]
    async fn retry_with_policy_does_not_retry_terminal_error() {
        let provider = Arc::new(MockProvider::new(vec![
            Err(ProviderError::Upstream { status: 401, body: "auth".into() }),
            Ok("never reached"),
        ]));
        let result = retry_with_policy(
            provider.clone(),
            dummy_target(),
            dummy_request(),
            &fast_policy(3),
        )
        .await;
        assert!(matches!(result, Err(ProviderError::Upstream { status: 401, .. })));
        assert_eq!(provider.calls(), 1);
    }

    #[tokio::test]
    async fn retry_with_policy_exhausts_max_attempts() {
        let provider = Arc::new(MockProvider::new(vec![
            Err(ProviderError::Upstream { status: 502, body: "bad".into() }),
            Err(ProviderError::Upstream { status: 502, body: "bad".into() }),
            Err(ProviderError::Upstream { status: 502, body: "bad".into() }),
        ]));
        let result = retry_with_policy(
            provider.clone(),
            dummy_target(),
            dummy_request(),
            &fast_policy(3),
        )
        .await;
        assert!(matches!(result, Err(ProviderError::Upstream { status: 502, .. })));
        assert_eq!(provider.calls(), 3);
    }

    #[tokio::test]
    async fn retry_with_policy_respects_total_budget() {
        // Budget 50ms, initial backoff 100ms → second attempt's sleep would
        // overshoot; should give up after the first failure (1 call).
        let policy = RetryPolicy {
            max_attempts: 5,
            initial_backoff_ms: 100,
            multiplier: 2.0,
            jitter_pct: 0.0,
            total_budget_ms: 50,
            retry_on: vec!["upstream_5xx".into()],
        };
        let provider = Arc::new(MockProvider::new(vec![
            Err(ProviderError::Upstream { status: 502, body: "bad".into() }),
            Err(ProviderError::Upstream { status: 502, body: "bad".into() }),
        ]));
        let result = retry_with_policy(
            provider.clone(),
            dummy_target(),
            dummy_request(),
            &policy,
        )
        .await;
        assert!(matches!(result, Err(ProviderError::Upstream { status: 502, .. })));
        assert_eq!(provider.calls(), 1);
    }
```

- [ ] **Step 2: Run tests, expect failure**

```bash
rtk cargo test -p agent-shim-router --quiet
```

Expected: compile error (`retry_with_policy` doesn't exist).

- [ ] **Step 3: Implement retry_with_policy**

Append to `crates/router/src/retry.rs`:

```rust
use std::sync::Arc;

use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream};
use agent_shim_providers::{BackendProvider, ProviderError};
use rand::rngs::SmallRng;
use rand::SeedableRng;

use crate::fallback::{fallback_eligibility_with_overrides, FallbackEligibility};

/// Drive a single `provider.complete()` call under the supplied retry policy.
///
/// On success: returns Ok(stream) on the first attempt that succeeds.
/// On retryable error: sleeps `compute_backoff(attempt, ...)`, increments
/// the attempt counter, and tries again — until `max_attempts` is reached
/// OR the next backoff would exceed `total_budget_ms`.
/// On terminal error (per `retry_on` override of the default classifier):
/// returns the error immediately without further attempts.
///
/// This helper is **single-upstream**. Plan 02's `ResilientCaller` walks
/// the chain across multiple upstreams.
pub async fn retry_with_policy(
    provider: Arc<dyn BackendProvider>,
    target: BackendTarget,
    req: CanonicalRequest,
    policy: &RetryPolicy,
) -> Result<CanonicalStream, ProviderError> {
    let mut rng = SmallRng::from_entropy();
    let mut attempt: u32 = 1;
    let mut total_elapsed_ms: u64 = 0;
    let mut last_err: Option<ProviderError> = None;

    loop {
        let result = provider.complete(req.clone(), target.clone()).await;
        match result {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                let eligibility = fallback_eligibility_with_overrides(&e, &policy.retry_on);
                if eligibility == FallbackEligibility::Terminal {
                    return Err(e);
                }
                last_err = Some(e);
                if attempt >= policy.max_attempts {
                    break;
                }
                let backoff = compute_backoff(attempt, policy, &mut rng);
                let backoff_ms = backoff.as_millis() as u64;
                if total_elapsed_ms + backoff_ms > policy.total_budget_ms {
                    break;
                }
                tracing::info!(
                    attempt,
                    backoff_ms,
                    total_elapsed_ms,
                    "retrying provider call after eligible error"
                );
                tokio::time::sleep(backoff).await;
                total_elapsed_ms += backoff_ms;
                attempt += 1;
            }
        }
    }

    Err(last_err.expect("loop only breaks after recording an error"))
}
```

Add to `crates/router/Cargo.toml` `[dependencies]` if not present:

```toml
async-trait = "0.1"
tracing = "0.1"
tokio = { workspace = true, features = ["time"] }
agent-shim-core = { path = "../core" }
```

- [ ] **Step 4: Run tests**

```bash
rtk cargo test -p agent-shim-router --quiet
```

Expected: 5 retry tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/router/src/retry.rs crates/router/Cargo.toml
git commit -m "feat(router): retry_with_policy single-upstream loop (Plan 04 P01 T4)

Drives a single provider.complete() through the configured retry
policy. Terminal errors short-circuit; eligible errors retry under
exponential backoff with jitter, capped by max_attempts AND
total_budget_ms (R7).

5 tests cover: success-first-attempt, retry-then-succeed,
terminal-no-retry, max-attempts-exhaustion, total-budget-cap.
MockProvider helper records call count via AtomicUsize."
```

---

### Task 5: integrate retry_with_policy into pipeline

**Files:**
- Modify: `crates/gateway/src/pipeline.rs` (around line 350-360 where `provider.complete` is called).
- Modify: `crates/gateway/src/state.rs` if a `RetryPolicy` lookup helper is needed.

- [ ] **Step 1: Read current pipeline call site**

```bash
rtk grep -n "provider.complete" crates/gateway/src/pipeline.rs
```

Expected output points to a call like `provider.complete(canonical, target).await`. Read the surrounding 30 lines to understand the context.

- [ ] **Step 2: Write integration test**

Create `crates/gateway/tests/retry_smoke.rs`:

```rust
//! Plan 04 P01 T5: gateway retry smoke. Mockito returns 502 once, then 200;
//! pipeline must produce the success body to the client.

use std::sync::Arc;

use agent_shim_core::request::RequestMetadata;
use bytes::Bytes;

#[tokio::test]
async fn retries_once_on_5xx_then_succeeds() {
    let mut server = mockito::Server::new_async().await;

    let _bad = server
        .mock("POST", "/v1/chat/completions")
        .with_status(502)
        .with_body("Bad Gateway")
        .expect(1)
        .create_async()
        .await;

    let _good = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"id":"x","object":"chat.completion","created":1700000000,"model":"gpt-4o",
                 "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},
                 "finish_reason":"stop"}],
                 "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        )
        .expect(1)
        .create_async()
        .await;

    let cfg_yaml = format!(
        r#"
upstreams:
  oai:
    type: openai_compatible
    base_url: {url}
    api_key: test-key
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstream: oai
    upstream_model: gpt-4o
    retry:
      max_attempts: 2
      initial_backoff_ms: 1
      total_budget_ms: 1000
"#,
        url = server.url()
    );

    let app = build_test_app(cfg_yaml).await;
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body_str = String::from_utf8(body.to_vec()).unwrap();
    assert!(body_str.contains("\"content\":\"hi\""));
}

// build_test_app builds an axum router from a YAML config string. If a
// helper already exists in crates/gateway/tests/, reuse it; otherwise
// create one inline mirroring the shape of crates/gateway/tests/responses_capability_gate_deepseek.rs.
```

If `build_test_app` doesn't already exist, create a small helper module (e.g. `crates/gateway/tests/common/mod.rs`) that loads `GatewayConfig` from a YAML string and builds the axum router via the existing `agent_shim::server::build_router` (or whatever the existing entry point is — check `crates/gateway/src/server.rs`).

- [ ] **Step 3: Modify pipeline to use retry_with_policy**

In `crates/gateway/src/pipeline.rs`, replace the direct `provider.complete(canonical, target).await` call with:

```rust
// Look up the per-route retry policy. The route entry is already in the
// resolver's static table; expose it via state for now (Plan 02 will move
// this into the resolved chain element).
let route_policy = state
    .static_routes
    .find_retry_policy(spec.frontend.kind(), &model_alias)
    .unwrap_or_default();

let retry_policy = agent_shim_router::RetryPolicy::from(&route_policy);
let stream = agent_shim_router::retry::retry_with_policy(
    provider.clone(),
    target.clone(),
    canonical,
    &retry_policy,
)
.await
.map_err(|e| {
    tracing::error!(error = %e, "provider.complete failed after retries");
    HandlerError::Provider(e)
})?;
```

Add a helper to `StaticRouter` (in `crates/router/src/static_routes.rs`):

```rust
impl StaticRouter {
    pub fn find_retry_policy(
        &self,
        frontend: FrontendKind,
        model: &str,
    ) -> Option<agent_shim_config::RetryConfig> {
        // Need to track RouteEntry in addition to BackendTarget. Update
        // StaticRouter struct to store the original RouteEntry alongside the
        // target so we can hand back retry/breaker config.
        // ...
    }
}
```

Updating `StaticRouter` to retain the source `RouteEntry` requires a small refactor: add a `route_entries: HashMap<RouteKey, RouteEntry>` field next to the existing `routes` field, populated in `from_config`. The lookup helper consults it.

- [ ] **Step 4: Run integration test**

```bash
rtk cargo test -p agent-shim --test retry_smoke --quiet
```

Expected: PASS — both mocks see exactly 1 hit (`expect(1)`), final HTTP response is 200 with the success body.

- [ ] **Step 5: Run full workspace tests**

```bash
rtk cargo test --workspace --quiet
```

Expected: 477 + new tests PASS. No existing test regresses (R5).

- [ ] **Step 6: Commit**

```bash
git add crates/gateway/src/pipeline.rs crates/gateway/tests/retry_smoke.rs crates/router/src/static_routes.rs crates/gateway/tests/common
git commit -m "feat(gateway): integrate retry_with_policy into pipeline (Plan 04 P01 T5)

Pipeline now calls retry_with_policy() instead of provider.complete()
directly. Per-route RetryConfig is looked up via StaticRouter, which
gained a route_entries map alongside the existing routes table.

Smoke test: mockito returns 502 then 200, expect=1 each. Pipeline
retries on 502 (eligible), succeeds on 200, client sees the success
body. Confirms end-to-end wiring from config → resolver → retry
loop → provider → encoder.

R5 regression coverage: existing v0.3 routes (singular shape, no
retry block) work unchanged; default retry.max_attempts=2 means one
silent retry on transient errors per D12."
```

---

### Task 6: provider docs

**Files:**
- Modify: `docs/providers/anthropic.md`
- Modify: `docs/providers/openai-compatible.md`
- Modify: `docs/providers/gemini.md`
- Modify: `docs/providers/deepseek.md`

- [ ] **Step 1: Add a "Resilience behavior" subsection to each provider doc**

Each provider doc gains a section near the bottom (before "Lossy fields" if that section exists) titled:

```markdown
### Resilience behavior (v0.4+)

The Phase 4 retry/fallback layer treats this provider's errors as
follows:

| Error pattern | Default eligibility |
|---|---|
| Connect/DNS/TLS failures (`Network`) | **Eligible** (retry / fall back) |
| HTTP 5xx (`Upstream{status>=500}`) | **Eligible** |
| HTTP 429 (`Upstream{status=429}`) | **Eligible** |
| HTTP 401/403/422 etc. | **Terminal** (return to client) |
| Decode errors (malformed bytes) | **Terminal** |

Provider-specific notes:
[per-provider notes here]
```

For **Anthropic** (`docs/providers/anthropic.md`):
> Anthropic emits HTTP **529 (overloaded)** during sustained load. 529 is
> in the 5xx range so the default classifier treats it as eligible — fallback
> chains will move to the next upstream. Operators rarely need to override.

For **OpenAI-compatible** (`docs/providers/openai-compatible.md`):
> OpenAI-compatible upstreams (OpenAI, DeepSeek, Together, Fireworks, etc.)
> typically use HTTP 503 for overload and HTTP 429 for rate-limit. Both are
> eligible by default.

For **Gemini** (`docs/providers/gemini.md`):
> AI Studio occasionally returns HTTP 400 with a `SAFETY` block reason on
> prompts that fail Gemini's safety filter. These are **terminal** under the
> default classifier — the request itself violated policy, and the next
> upstream would likely reject it the same way.

For **DeepSeek** (already covered by openai-compatible above, but
deepseek.md should reference back). Add:
> DeepSeek uses the OpenAI-compatible wire shape; see "Resilience behavior"
> in `openai-compatible.md` for the full table. DeepSeek is `vision: false`,
> so image-bearing requests are rejected by the capability gate (v0.3) —
> these errors are terminal (no fallback).

For **GitHub Copilot** (`docs/providers/github-copilot.md`):
> Copilot's token-exchange flow can fail with HTTP 401 if the OAuth refresh
> fails. These are terminal under the default classifier — fallback to
> another provider would not refresh the Copilot token. Operators relying
> on Copilot should monitor token expiry separately.

- [ ] **Step 2: Verify the docs build cleanly**

No build step for markdown — just visually scan each file.

- [ ] **Step 3: Commit**

```bash
git add docs/providers/anthropic.md docs/providers/openai-compatible.md docs/providers/gemini.md docs/providers/deepseek.md docs/providers/github-copilot.md
git commit -m "docs(providers): add Resilience behavior subsection (Plan 04 P01 T6)

Each provider doc gains a 'Resilience behavior (v0.4+)' subsection
documenting the default fallback_eligibility mapping plus
provider-specific notes:
- Anthropic: HTTP 529 overloaded → eligible
- OpenAI-compat: 5xx and 429 → eligible (covers DeepSeek by reference)
- Gemini: SAFETY-block 400 → terminal (request itself violates policy)
- Copilot: token-exchange 401 → terminal (next upstream wouldn't help)

These notes guide operators when they consider per-route retry_on
overrides."
```

---

## Definition of Done

- [ ] All 6 tasks complete.
- [ ] `cargo nextest run --workspace` passes (or `cargo test --workspace`).
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] `cargo fmt --all` clean.
- [ ] v0.3 example configs in `config/` continue to validate.
- [ ] Workspace test count: 477 → ~485 (8 new tests in this plan).
- [ ] No changes to `crates/core/`, `crates/frontends/`, `crates/providers/src/`.
- [ ] CHANGELOG entry NOT yet added (Plan 05 consolidates).

After this plan merges, the gateway has:
- New config shape (singular and array forms coexist).
- Per-route retry policy with exponential backoff.
- A working classifier ready for Plan 02's chain walker.
- Documented resilience behavior per provider.
