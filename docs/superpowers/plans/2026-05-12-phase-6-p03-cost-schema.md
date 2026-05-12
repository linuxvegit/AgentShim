# Plan 03 — Cost / tier / latency schema + validation rules 15-18 (Phase 6)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-12-phase-6-cost-aware-gateway-design.md`](../specs/2026-05-12-phase-6-cost-aware-gateway-design.md) (§5 configuration schema, §5.4 breaking change, §5.5 rules 15-18, §5.6 new ValidationError variants).

**Goal:** Add the four cost-aware routing fields to the config schema (`tier`, `cost`, `p95_latency_budget_ms` on every upstream; `min_tier`, `max_cost_usd` on every route) and enforce validation rules 15-18. No routing behaviour change yet — P03 is pure plumbing for P04.

**Architecture:** New `Tier` enum + `UpstreamCost` struct in `crates/config/src/schema.rs`. Every `UpstreamConfig` variant struct gains 3 fields. `RouteEntry` gains 2 fields. `validate()` gains rules 15-17; `validate_for_reload()` gains rules 17-18. New `ValidationError` variants: `IncompleteCost`, `NegativeCost`, `InvalidTier`, `ImpossibleMinTier`.

**Tech stack:** Pure config + validation. No new crates. Uses existing `serde`, `thiserror` patterns from v0.5.

**Frozen-core impact:** Touches `crates/config/` only. Frozen core (core+frontends) untouched. `providers/src/` untouched.

**⚠ Breaking change:** `tier` is required on every upstream. v0.5 configs that don't declare `tier` will fail to parse. Spec §5.4 documents this.

**Test target:** 644 (after P02) → 650 (+6 unit).

---

## File Structure

`crates/config/src/`:
- Modify: `schema.rs` — new `Tier` enum, new `UpstreamCost` struct. Every `UpstreamConfig` variant gets `tier`, `cost`, `p95_latency_budget_ms`. `RouteEntry` gets `min_tier`, `max_cost_usd`.
- Modify: `validation.rs` — rules 15-17 in `validate()`, rule 18 honored in `validate_for_reload()` (means: NOT in immutable set), 4 new `ValidationError` variants.
- Modify: `lib.rs` — re-export `Tier`, `UpstreamCost` if not auto-exported via `pub use schema::*;`.

`crates/gateway/tests/` and various existing integration tests:
- **Many** existing test fixtures will fail to parse because they don't declare `tier`. P03 T5 sweeps the codebase and adds `tier: standard` to every existing test YAML fixture. Same for any v0.5 plan example YAMLs.

---

## Tasks

### Task 1: Add `Tier` enum + `UpstreamCost` struct

**Files:**
- Modify: `crates/config/src/schema.rs`

- [ ] **Step 1: Add Tier enum**

In `crates/config/src/schema.rs`, near other enums (e.g. next to `FrontendKind` or at the top of the file's type definitions), add:

```rust
/// Service tier label on an upstream. Used by Phase 6 cost-aware
/// routing: routes can specify `min_tier`, and upstreams below that
/// tier are filtered out of the chain before the resilience layer runs.
///
/// Derived ordering is the operator-intuitive one:
///   economy < standard < premium
///
/// Plan 06 P03 T1.
#[derive(
    Debug, Clone, Copy, serde::Serialize, serde::Deserialize,
    PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Tier {
    Economy,
    Standard,
    Premium,
}
```

- [ ] **Step 2: Add UpstreamCost struct**

In the same file, add:

```rust
/// Per-upstream USD cost per million tokens. Used by Phase 6
/// cost-aware routing to estimate request cost and reject upstreams
/// whose estimate exceeds a route's `max_cost_usd`.
///
/// Both fields must be present and non-negative; partial declarations
/// fail validation (rule 15). Plan 06 P03 T1.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UpstreamCost {
    pub input_per_million_usd: f64,
    pub output_per_million_usd: f64,
}
```

- [ ] **Step 3: Re-export from lib.rs if needed**

In `crates/config/src/lib.rs`, check whether `pub use schema::*;` already exists. If yes, no change. If schema items are re-exported individually, add:

```rust
pub use schema::{Tier, UpstreamCost};
```

- [ ] **Step 4: Build the config crate**

```bash
cargo build -p agent-shim-config 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 5: Don't commit yet**

Wait until Task 2 wires up the fields onto the existing config structs.

---

### Task 2: Add fields to every UpstreamConfig variant + RouteEntry

**Files:**
- Modify: `crates/config/src/schema.rs`

The `UpstreamConfig` enum has five variants today: `OpenAiCompatible`, `GithubCopilot`, `Anthropic`, `Deepseek`, `Gemini`. Each variant's struct gets three new fields. `RouteEntry` gets two new fields.

- [ ] **Step 1: Add fields to OpenAiCompatibleUpstream**

Find the struct definition in `schema.rs`. Add at the end of the field list (preserve `#[serde(default)]` style if neighbours use it):

```rust
    /// Tier label. Plan 06 P03 — required.
    pub tier: Tier,
    /// Per-token cost in USD. Plan 06 P03 — optional.
    /// If present, used by cost-aware routing to estimate per-request
    /// cost and apply `route.max_cost_usd` caps.
    #[serde(default)]
    pub cost: Option<UpstreamCost>,
    /// p95 latency budget in milliseconds. Plan 06 P03 — optional.
    /// If present, the cost filter compares against the recent p95
    /// from the metrics histogram. Upstreams over budget are skipped.
    #[serde(default)]
    pub p95_latency_budget_ms: Option<u64>,
```

- [ ] **Step 2: Same fields on GithubCopilotUpstream**

Find the struct (GithubCopilot variant body) and add the same three fields. GithubCopilot is a subscription product (no per-token cost), so realistic configs will leave `cost = None` — but the field is present for schema uniformity.

- [ ] **Step 3: Same fields on AnthropicUpstream**

Same edit.

- [ ] **Step 4: Same fields on DeepseekUpstream**

Same edit.

- [ ] **Step 5: Same fields on GeminiUpstream**

Same edit.

- [ ] **Step 6: Add fields to RouteEntry**

Find `RouteEntry`. Add at the end of the field list:

```rust
    /// Minimum tier required for an upstream in this route's chain to
    /// be considered. Upstreams with `tier < min_tier` are filtered
    /// out before the resilience layer runs. Plan 06 P03 — optional.
    /// When None, no tier filtering applies.
    #[serde(default)]
    pub min_tier: Option<Tier>,
    /// Per-request cost cap in USD. The cost filter estimates the cost
    /// (using `tiktoken-rs` for input tokens + `request.max_tokens` for
    /// output) and rejects upstreams whose estimate exceeds this cap.
    /// Plan 06 P03 — optional. When None, no cap applies.
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
```

- [ ] **Step 7: Build the config crate — EXPECT MANY ERRORS**

```bash
cargo build -p agent-shim-config 2>&1 | tail -20
```

Expected: errors at every existing test that constructs `OpenAiCompatibleUpstream { ... }` etc. as a struct literal — they're missing the new required `tier` field. Note the count and file list; that's Task 3's todo.

- [ ] **Step 8: Don't commit yet**

---

### Task 3: Update config-crate-internal tests to declare `tier`

**Files:**
- Modify: every test in `crates/config/src/` that constructs an upstream struct literal.

- [ ] **Step 1: List the failing test sites**

```bash
cargo build -p agent-shim-config --tests 2>&1 | grep "missing field" | sort -u
```

Each error names a file and line. Update each one.

- [ ] **Step 2: For each compile error, add `tier: Tier::Standard,`**

Convention: use `Tier::Standard` as the default when an existing test doesn't specifically care about tier. For tests that DO care (the new rule-17 tests in Task 4), pick deliberately.

- [ ] **Step 3: Build config tests**

```bash
cargo build -p agent-shim-config --tests 2>&1 | tail -10
```

Expected: clean.

- [ ] **Step 4: Run config crate tests**

```bash
cargo test -p agent-shim-config 2>&1 | tail -10
```

Expected: all existing tests pass.

- [ ] **Step 5: Don't commit yet** — Task 4's validation rules + Task 5's workspace-wide sweep should land together.

---

### Task 4: Validation rules 15-18 + new ValidationError variants

**Files:**
- Modify: `crates/config/src/validation.rs`

- [ ] **Step 1: Add new ValidationError variants**

Find the `ValidationError` enum in `validation.rs`. Add four new variants:

```rust
    /// Rule 15: cost.* fields must both be present or both absent.
    #[error("upstream `{upstream}` declares partial `cost` block; both `input_per_million_usd` and `output_per_million_usd` are required")]
    IncompleteCost { upstream: String },

    /// Rule 15: cost.* fields must be non-negative.
    #[error("upstream `{upstream}` has negative `cost.{field}` (must be >= 0)")]
    NegativeCost { upstream: String, field: &'static str },

    /// Rule 16: tier must be one of economy/standard/premium. Schema
    /// usually catches this; only triggers on hand-crafted invalid YAML.
    #[error("upstream `{upstream}` has invalid tier `{value}` (must be economy/standard/premium)")]
    InvalidTier { upstream: String, value: String },

    /// Rule 17: route's min_tier must be satisfied by at least one
    /// upstream in its chain — otherwise the route always 503s.
    #[error(
        "route `{route}` requires min_tier={min_tier:?} but no upstream in its chain meets it (chain tiers: {chain_tiers:?})"
    )]
    ImpossibleMinTier {
        route: String,
        min_tier: crate::schema::Tier,
        chain_tiers: Vec<(String, crate::schema::Tier)>,
    },
```

The `#[error(...)]` strings are user-facing in startup-failure messages and the reload 4xx response body. Make sure they're operator-grade.

- [ ] **Step 2: Add rule-15 enforcement**

Since `UpstreamCost` is a `struct` (not `Option<f64> + Option<f64>`), serde already enforces "both present or both absent" at the type level — Option<UpstreamCost> can only be None or Some({both fields}). The `IncompleteCost` variant is therefore reachable only via the alternative-parse path where someone provides one of the keys at the top level. In practice this variant primarily catches the **negative-value** case, but the spec lists it separately to give operators a clearer error string when only one of `input_*` / `output_*` is set in a less strict schema mode.

For Phase 6, leave `IncompleteCost` as a defined-but-rarely-fired variant. Rule 15 in code becomes the negative-value check:

In `validate()`, somewhere after the existing upstream walks, add a new loop:

```rust
    // Rule 15 (Plan 06 P03): cost.* fields must be non-negative when
    // declared. `Option<UpstreamCost>` already guarantees both fields
    // are present-or-absent together at the schema layer — we just
    // check sign here.
    for (name, upstream) in &cfg.upstreams {
        let cost_opt = match upstream {
            UpstreamConfig::OpenAiCompatible(c) => c.cost.as_ref(),
            UpstreamConfig::GithubCopilot(c) => c.cost.as_ref(),
            UpstreamConfig::Anthropic(c) => c.cost.as_ref(),
            UpstreamConfig::Deepseek(c) => c.cost.as_ref(),
            UpstreamConfig::Gemini(c) => c.cost.as_ref(),
        };
        if let Some(cost) = cost_opt {
            if cost.input_per_million_usd < 0.0 {
                return Err(ValidationError::NegativeCost {
                    upstream: name.clone(),
                    field: "input_per_million_usd",
                });
            }
            if cost.output_per_million_usd < 0.0 {
                return Err(ValidationError::NegativeCost {
                    upstream: name.clone(),
                    field: "output_per_million_usd",
                });
            }
        }
    }
```

Note: this snippet assumes `UpstreamConfig` variants are accessor-uniform. If the actual `UpstreamConfig` enum has different field names per variant (e.g. `c.cost` vs `c.cost_overrides`), adapt to whatever the struct actually exposes after Task 2. Read each variant after Task 2's edits and adjust the match arms.

For pattern-match brevity if shapes vary, extract a small helper:

```rust
fn upstream_cost(u: &UpstreamConfig) -> Option<&UpstreamCost> {
    match u {
        UpstreamConfig::OpenAiCompatible(c) => c.cost.as_ref(),
        UpstreamConfig::GithubCopilot(c) => c.cost.as_ref(),
        UpstreamConfig::Anthropic(c) => c.cost.as_ref(),
        UpstreamConfig::Deepseek(c) => c.cost.as_ref(),
        UpstreamConfig::Gemini(c) => c.cost.as_ref(),
    }
}

fn upstream_tier(u: &UpstreamConfig) -> Tier {
    match u {
        UpstreamConfig::OpenAiCompatible(c) => c.tier,
        UpstreamConfig::GithubCopilot(c) => c.tier,
        UpstreamConfig::Anthropic(c) => c.tier,
        UpstreamConfig::Deepseek(c) => c.tier,
        UpstreamConfig::Gemini(c) => c.tier,
    }
}
```

Add these helpers near the top of `validation.rs` (module-private).

- [ ] **Step 3: Add rule-17 enforcement (impossible min_tier)**

```rust
    // Rule 17 (Plan 06 P03): every route with min_tier set must have
    // at least one upstream in its chain that meets it. Otherwise the
    // route is guaranteed to produce 503 NoEligibleUpstream forever.
    for route in &cfg.routes {
        let Some(min_tier) = route.min_tier else { continue };
        // Build the route's effective chain: prefer `upstreams` (chain)
        // if non-empty, else fall back to single `upstream`.
        let chain_names: Vec<&str> = if !route.upstreams.is_empty() {
            route.upstreams.iter().map(String::as_str).collect()
        } else if let Some(u) = &route.upstream {
            vec![u.as_str()]
        } else {
            // No upstream declared — earlier validation already rejected
            // this; skip.
            continue;
        };
        let chain_tiers: Vec<(String, Tier)> = chain_names
            .iter()
            .filter_map(|n| {
                cfg.upstreams
                    .get(*n)
                    .map(|u| (n.to_string(), upstream_tier(u)))
            })
            .collect();
        let any_meets = chain_tiers.iter().any(|(_, t)| *t >= min_tier);
        if !any_meets && !chain_tiers.is_empty() {
            return Err(ValidationError::ImpossibleMinTier {
                route: format!("{}/{}", route.frontend, route.model),
                min_tier,
                chain_tiers,
            });
        }
    }
```

Adapt field names (`route.frontend`, `route.model`, `route.upstreams`, `route.upstream`) to match the actual `RouteEntry` definition.

- [ ] **Step 4: Rule 18 in validate_for_reload — no immutability addition**

`validate_for_reload` already exists from v0.5 P04 T1. It enforces rules 11-14 (immutable upstreams set, immutable server/admin, immutable otel.endpoint). Rule 18 says: tier / cost / p95_latency_budget_ms / min_tier / max_cost_usd are ALL reloadable. This means **no change** to the immutable-field set in `validate_for_reload`.

But: rule 17 must also fire at reload time, because reload can change `tier` values. Add to `validate_for_reload`:

```rust
    // Rule 17 (Plan 06 P03) re-fires at reload: changing tier on an
    // upstream might make a route's min_tier unsatisfiable.
    validate(candidate)?;
```

`validate(candidate)?;` is already called from `validate_for_reload` at the bottom — confirm by reading the existing function. If yes, rule 17 is automatically rechecked. If no, add the call.

- [ ] **Step 5: Add unit tests (5 new tests)**

Append to `crates/config/src/validation.rs::tests`:

```rust
    fn upstream_with_cost(
        input: f64,
        output: f64,
        tier: Tier,
    ) -> UpstreamConfig {
        UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: "http://x/v1".into(),
            api_key: Secret::new("a"),
            default_headers: BTreeMap::new(),
            request_timeout_secs: 120,
            tier,
            cost: Some(UpstreamCost {
                input_per_million_usd: input,
                output_per_million_usd: output,
            }),
            p95_latency_budget_ms: None,
        })
    }

    #[test]
    fn cost_negative_input_rejected() {
        let mut upstreams = BTreeMap::new();
        upstreams.insert("m".into(), upstream_with_cost(-1.0, 1.0, Tier::Standard));
        let cfg = GatewayConfig {
            server: ServerConfig::test_default(),
            upstreams,
            routes: vec![simple_route("m")],
            ..Default::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(
            err,
            ValidationError::NegativeCost { ref upstream, field: "input_per_million_usd" }
                if upstream == "m"
        ));
    }

    #[test]
    fn cost_negative_output_rejected() {
        let mut upstreams = BTreeMap::new();
        upstreams.insert("m".into(), upstream_with_cost(1.0, -1.0, Tier::Standard));
        let cfg = GatewayConfig {
            server: ServerConfig::test_default(),
            upstreams,
            routes: vec![simple_route("m")],
            ..Default::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(
            err,
            ValidationError::NegativeCost { ref upstream, field: "output_per_million_usd" }
                if upstream == "m"
        ));
    }

    #[test]
    fn impossible_min_tier_rejected() {
        let mut upstreams = BTreeMap::new();
        upstreams.insert("m".into(), upstream_with_cost(1.0, 1.0, Tier::Economy));
        let mut route = simple_route("m");
        route.min_tier = Some(Tier::Premium);
        let cfg = GatewayConfig {
            server: ServerConfig::test_default(),
            upstreams,
            routes: vec![route],
            ..Default::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(err, ValidationError::ImpossibleMinTier { .. }));
    }

    #[test]
    fn impossible_min_tier_chain_with_one_match_passes() {
        let mut upstreams = BTreeMap::new();
        upstreams.insert("eco".into(), upstream_with_cost(0.1, 0.2, Tier::Economy));
        upstreams.insert("std".into(), upstream_with_cost(1.0, 2.0, Tier::Standard));
        let mut route = simple_route("eco");
        route.upstreams = vec!["eco".into(), "std".into()];
        route.min_tier = Some(Tier::Standard);
        let cfg = GatewayConfig {
            server: ServerConfig::test_default(),
            upstreams,
            routes: vec![route],
            ..Default::default()
        };
        validate(&cfg).expect("chain has std which meets min_tier=standard");
    }

    #[test]
    fn reload_allows_tier_change() {
        // P04 T1 (v0.5) established reload semantics. This test verifies
        // rule 18: tier changes are reloadable, not immutable.
        let mut upstreams = BTreeMap::new();
        upstreams.insert("m".into(), upstream_with_cost(1.0, 2.0, Tier::Standard));
        let mut routes = vec![simple_route("m")];
        let cfg = GatewayConfig {
            server: ServerConfig::test_default(),
            upstreams,
            routes,
            ..Default::default()
        };
        let baseline = ReloadBaseline::from_running(&cfg);

        // Candidate changes the tier from Standard to Premium.
        let mut candidate = cfg.clone();
        if let UpstreamConfig::OpenAiCompatible(c) = candidate.upstreams.get_mut("m").unwrap() {
            c.tier = Tier::Premium;
        }
        validate_for_reload(&candidate, &baseline).expect("tier change must be reloadable");
    }

    #[test]
    fn reload_rejects_tier_change_that_breaks_rule_17() {
        // If tier drop makes the route's min_tier unsatisfiable,
        // validate(candidate) inside validate_for_reload fires rule 17.
        let mut upstreams = BTreeMap::new();
        upstreams.insert("m".into(), upstream_with_cost(1.0, 2.0, Tier::Premium));
        let mut route = simple_route("m");
        route.min_tier = Some(Tier::Premium);
        let cfg = GatewayConfig {
            server: ServerConfig::test_default(),
            upstreams,
            routes: vec![route],
            ..Default::default()
        };
        let baseline = ReloadBaseline::from_running(&cfg);

        let mut candidate = cfg.clone();
        if let UpstreamConfig::OpenAiCompatible(c) = candidate.upstreams.get_mut("m").unwrap() {
            c.tier = Tier::Economy; // breaks min_tier=Premium
        }
        let err = validate_for_reload(&candidate, &baseline).unwrap_err();
        assert!(matches!(
            err,
            ReloadValidationError::StartupError(ValidationError::ImpossibleMinTier { .. })
        ));
    }
```

These tests assume helper functions `ServerConfig::test_default()`, `simple_route(&str)`, `ReloadBaseline::from_running(&cfg)` exist in the test module. If they don't, the subagent dispatched to execute this task should look at neighbouring v0.5 P04 T1 tests in the same file (`reload_rejects_changed_server_port`, etc.) for the actual helper shapes and substitute accordingly.

- [ ] **Step 6: Run config tests**

```bash
cargo test -p agent-shim-config 2>&1 | tail -10
```

Expected: 80+ tests pass (was 80 after v0.5 P04 T1; +6 new ones).

- [ ] **Step 7: Don't commit yet** — Task 5 fixes the workspace-wide test sweep before commit.

---

### Task 5: Workspace test fixture sweep — add `tier:` to every YAML

**Files:** Many. Specifically every `tests/*.rs` and `tests/*.yaml` (if any) under `crates/gateway/`, `crates/router/`, `crates/protocol-tests/` that constructs a `GatewayConfig` from YAML.

The breaking change `tier: required` means every existing test fixture YAML now fails to parse. Sweep them.

- [ ] **Step 1: Identify failing tests**

```bash
cargo test --workspace --quiet 2>&1 | grep -E "missing field.*tier" | head -20
```

If serde errors fire at runtime (parse-time), each will name the test that crashed. Catalog them.

Alternatively, since `tier` is required on every upstream variant struct, grep statically:

```bash
grep -rln "type: open_ai_compatible" crates/gateway/tests/ crates/router/tests/ crates/protocol-tests/tests/ 2>&1 | sort -u
grep -rln "type: anthropic" crates/gateway/tests/ crates/router/tests/ crates/protocol-tests/tests/ 2>&1 | sort -u
grep -rln "type: deepseek" crates/gateway/tests/ crates/router/tests/ crates/protocol-tests/tests/ 2>&1 | sort -u
grep -rln "type: gemini" crates/gateway/tests/ crates/router/tests/ crates/protocol-tests/tests/ 2>&1 | sort -u
grep -rln "type: github_copilot" crates/gateway/tests/ crates/router/tests/ crates/protocol-tests/tests/ 2>&1 | sort -u
```

Every file listed needs a `tier: standard` line added immediately after the upstream's `api_key: ...` (or equivalent) line.

- [ ] **Step 2: Add `tier: standard` to every test YAML upstream**

For each file in the sweep above, edit every upstream block to include `tier: standard`. Example before:

```yaml
upstreams:
  m:
    type: open_ai_compatible
    base_url: http://x/v1
    api_key: a
```

After:

```yaml
upstreams:
  m:
    type: open_ai_compatible
    base_url: http://x/v1
    api_key: a
    tier: standard
```

Convention: use `tier: standard` everywhere. None of the existing v0.5 tests exercise tier-aware routing — they only need to satisfy the schema requirement.

- [ ] **Step 3: Update integration test YAMLs in inline format strings**

Many test files use `format!(...)` to construct YAML inline. Each needs the same `tier: standard` addition. The catalog from Step 1 covers these too.

Some specific known-affected files (from v0.5 spelunking; verify each):

- `crates/gateway/tests/admin_port.rs`
- `crates/gateway/tests/healthz.rs`
- `crates/gateway/tests/metrics_endpoint.rs`
- `crates/gateway/tests/auth_required_unconfigured_key.rs`
- `crates/gateway/tests/breaker_half_open_recovery.rs`
- `crates/gateway/tests/breaker_trip_skips_upstream.rs`
- `crates/gateway/tests/rate_limit_per_key_envelope.rs`
- `crates/gateway/tests/responses_capability_gate_deepseek.rs`
- `crates/gateway/tests/responses_fallback_oai_to_anthropic.rs`
- `crates/gateway/tests/retry_smoke.rs`
- `crates/gateway/tests/streaming_fallback_pre_stream_only.rs`
- `crates/gateway/tests/vision_capability_mismatch.rs`
- `crates/gateway/tests/count_tokens_smoke.rs`
- `crates/gateway/tests/e2e_openai_chat.rs`
- `crates/gateway/tests/responses_to_anthropic_e2e.rs`
- `crates/gateway/tests/reload_admin_post.rs`
- `crates/gateway/tests/reload_in_flight.rs`
- `crates/gateway/tests/reload_sighup.rs`
- `crates/gateway/tests/phase5_smoke.rs`
- `crates/gateway/tests/traceparent_propagation.rs`
- `crates/gateway/tests/outbound_traceparent.rs` (added in P01 — verify P01 omitted tier per its T4 note)
- `crates/gateway/tests/outbound_traceparent_inherits_inbound.rs` (same)
- `crates/router/tests/*.rs` — any that builds YAML

Also P01's `crates/gateway/tests/reload_rate_limit_buckets_reset.rs` from P02.

- [ ] **Step 4: Update the v0.4/v0.5 config tests too**

`crates/config/src/` test fixtures (in-line YAML inside `validation.rs::tests`, etc.) — Task 3 already covered the struct-literal tests, but any inline YAML strings need the same treatment.

- [ ] **Step 5: Build & run the workspace**

```bash
cargo build --workspace --tests 2>&1 | tail -10
cargo test --workspace --quiet 2>&1 | grep -E "^test result:" | grep -v "0 failed" | head -10
echo "---"
echo "Any failures above?"
```

Expected: empty. Workspace builds clean, no test failures, no `missing field tier` messages.

- [ ] **Step 6: Workspace total**

```bash
cargo test --workspace --quiet 2>&1 | tee /tmp/p03.out > /dev/null
grep -E "^test result:" /tmp/p03.out | awk -F'[ ;]' '{sum+=$4} END {print "TOTAL:", sum}'
rm /tmp/p03.out
```

Expected: 650 (was 644 after P02, +6 unit tests).

- [ ] **Step 7: Commit P03 T1-T5 as one atomic landing**

Because the schema change is breaking and the test sweep is non-optional, all five tasks land together so the workspace stays green between commits.

```bash
git add -A
git commit -m "feat(config): cost/tier/latency schema + rules 15-18 + test sweep (Plan 03 P03 T1-T5)"
```

---

### Task 6: Spec compliance + code quality review

- [ ] **Step 1: Reviewer dispatch (spec compliance)**

> Review commit Plan 03 P03 T1-T5 against spec `docs/superpowers/specs/2026-05-12-phase-6-cost-aware-gateway-design.md`. Verify:
>
> 1. **§5.1 new types** — `Tier` enum has exactly 3 variants (economy / standard / premium), serde-renamed to lowercase. `UpstreamCost` has exactly 2 f64 fields. Both use `#[serde(deny_unknown_fields)]`.
> 2. **§5.2 field additions** — every UpstreamConfig variant has `tier`, `cost: Option<UpstreamCost>`, `p95_latency_budget_ms: Option<u64>`. RouteEntry has `min_tier: Option<Tier>`, `max_cost_usd: Option<f64>`.
> 3. **§5.4 breaking change** — `tier` is genuinely required (no `#[serde(default)]` on it). Confirm.
> 4. **§5.5 rules 15-18** — rule 15 (negative cost): `NegativeCost` variant; rule 16 (invalid tier): schema-level enum enforces; rule 17 (impossible min_tier): `ImpossibleMinTier` variant fires from `validate()`; rule 18 (reload allows tier/cost changes): `validate_for_reload`'s immutable set unchanged, but `validate(candidate)` recheck fires rule 17 on tier changes.
> 5. **§5.6 ValidationError variants** — 4 new variants exist with operator-grade error strings.
> 6. **§2.1 frozen-core** — `git diff a0139fc -- crates/core/ crates/frontends/` empty.
> 7. **Sweep completeness** — `cargo test --workspace` is green; no `missing field tier` errors anywhere.

- [ ] **Step 2: Reviewer dispatch (code quality)**

> Review commit Plan 03 P03 T1-T5 for code quality.
>
> 1. The `upstream_cost()` and `upstream_tier()` helpers — repetitive match arms over UpstreamConfig variants. Consider a trait `UpstreamCommon` to reduce duplication, OR confirm this is consistent with how `validate()` already accesses upstream fields per variant.
> 2. `NegativeCost` uses a `&'static str` for the field name. Compare against neighbouring ValidationError variants — is this consistent?
> 3. The 5 new unit tests use `upstream_with_cost(...)` helper — is the helper general enough to be reused, or does each test bake too many assumptions?
> 4. Did the test-fixture sweep miss any file? Grep for `type: open_ai_compatible` or `type: anthropic` etc. that lack a `tier:` neighbor.
> 5. `IncompleteCost` is defined but never fires from the schema-driven path. Is its rustdoc clear that it's an alternative-path-only variant? Or remove it to reduce dead code?

- [ ] **Step 3: Apply CRITICAL/HIGH findings**

```bash
git commit -m "fix(config): address P03 review findings"
```

---

## Done when

- [ ] Workspace test count ≥ 650 (was 644, +6 unit).
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] `git diff a0139fc -- crates/core/ crates/frontends/` empty.
- [ ] `crates/providers/src/` diff is exactly what P01 set down — P03 didn't touch it.
- [ ] Every test fixture in the workspace has `tier: <value>` on every upstream block.
- [ ] All 4 new ValidationError variants exist with operator-grade error strings.
- [ ] Rule 17 fires from both `validate()` (startup) and `validate_for_reload()` (via its internal `validate(candidate)` call).
- [ ] CHANGELOG note for the breaking change is **deferred to P05** (the docs/release plan).
- [ ] T6 reviews clear of CRITICAL/HIGH findings.
