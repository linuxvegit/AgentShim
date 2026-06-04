# Route Model Prefix Wildcard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend `RouteEntry.model` to accept trailing-`*` prefix patterns (e.g. `claude-*`, `glm*`) so operators can route a whole family of models through a single upstream without enumerating every alias.

**Architecture:** Single new validation rule in `validate_routes`; new third tier in `StaticRouter` between exact lookup and the existing `"*"` wildcard. Bucket-based partition (`exact`, `prefixes`, `wildcards`) keeps the hot path (exact O(1)) untouched and makes catalog exclusion of prefix routes structural rather than filter-based. Pass-through (`upstream_model: "*"`) reuses the existing wildcard rewrite path so fuzzy upgrade still applies.

**Tech Stack:** Rust (workspace), `cargo nextest`, `proptest` (already a workspace dev-dep), `serde`. No new crate dependencies.

**Spec:** `docs/superpowers/specs/2026-06-04-route-wildcard-design.md`

---

## File Structure

Concrete files this plan touches:

- `crates/config/src/validation.rs` — add rule 8 (`*` only at end) inside `validate_routes`; new unit tests.
- `crates/config/src/schema.rs` — update the `RouteEntry.model` doc comment only; no field changes.
- `crates/router/src/static_routes.rs` — introduce `PrefixRoute` struct, `prefixes: HashMap<FrontendKind, Vec<PrefixRoute>>` field, extend `from_config` and `resolve_route`; new unit tests + one `proptest`.
- `crates/router/src/resolver.rs` — no implementation changes; add two integration-style unit tests.
- `docs/configuration.md` — Routes section: add prefix-wildcard row, document priority (`exact > longest-prefix > "*"`), duplicate-route behavior, and `strict_upstream_models` rule (looks only at `upstream_model`).

Test files live alongside their implementations (`#[cfg(test)] mod tests` and `#[cfg(test)] mod proptests`), matching the existing project layout.

---

## Task 1: Document the new `model` shape in schema

**Files:**
- Modify: `crates/config/src/schema.rs` (the doc comment immediately above `pub struct RouteEntry`, around line 514–521)

- [ ] **Step 1: Update the doc comment**

Replace the existing 5-line doc comment above `pub struct RouteEntry` with:

```rust
/// A single route mapping. Supports two upstream-shape forms and three
/// `model`-field forms:
///
/// Upstream shapes (mutually exclusive on the same route; validation enforces it):
/// - **Singular** (v0.3 compat): `upstream` + `upstream_model`.
/// - **Array** (v0.4): `upstreams: [{name, model}, ...]` for fallback chains.
///
/// `model` field forms:
/// - **Exact** — e.g. `claude-sonnet-4-5`. Matches inbound model identically.
/// - **Prefix wildcard** — e.g. `claude-*`, `glm*`. Literal characters followed
///   by a single `*` at the very end. Matches when the inbound model
///   `starts_with` the literal prefix. Resolved at lookup time by
///   longest-prefix-wins (`StaticRouter::resolve_route`).
/// - **Catch-all wildcard** — `"*"`. Matches any inbound model; at most one
///   per frontend.
///
/// Priority at lookup: exact > longest-prefix > `"*"`. Identical `(frontend,
/// model)` entries silently overwrite (later wins) — matches today's
/// `HashMap` insertion semantics for exact routes.
```

- [ ] **Step 2: Verify cargo check passes**

Run: `rtk cargo check -p agent-shim-config`
Expected: PASS (this is a comment-only change, but confirms no accidental rustdoc syntax breakage).

- [ ] **Step 3: Commit**

```bash
rtk git add crates/config/src/schema.rs && rtk git commit -m "docs(config): describe prefix wildcard shape for RouteEntry.model"
```

---

## Task 2: Add `validate_routes` rule rejecting illegal `*` positions

**Files:**
- Modify: `crates/config/src/validation.rs` (inside `pub fn validate_routes`, append a new rule after rule 7 around line 550)
- Test: `crates/config/src/validation.rs` (the existing `#[cfg(test)] mod tests` block, after the `validate_routes_*` cases around line 1430+)

- [ ] **Step 1: Write the first failing test (accepts valid prefix shapes)**

Add this test to the `tests` module in `crates/config/src/validation.rs`:

```rust
#[test]
fn validate_routes_accepts_prefix_pattern() {
    let mut cfg = mk_cfg_with_routes(vec![]);
    cfg.upstreams.insert(
        "copilot".to_string(),
        crate::schema::UpstreamConfig::GithubCopilot(crate::schema::CopilotUpstream {
            tier: crate::schema::Tier::Standard,
            cost: None,
        }),
    );
    cfg.routes.push(crate::schema::RouteEntry::singular(
        "anthropic_messages",
        "claude-*",
        "copilot",
        "*",
    ));
    cfg.routes.push(crate::schema::RouteEntry::singular(
        "anthropic_messages",
        "glm*",
        "copilot",
        "*",
    ));
    validate_routes(&cfg).expect("prefix patterns should pass");
}
```

- [ ] **Step 2: Run the test, confirm it currently passes (no rule yet rejects valid prefixes)**

Run: `rtk cargo nextest run -p agent-shim-config validate_routes_accepts_prefix_pattern`
Expected: PASS (today's `validate_routes` has no `*`-position rule, so legal prefixes already pass; this test pins that behavior so the new rule doesn't accidentally reject them).

- [ ] **Step 3: Write the failing test for `*` in the middle**

Add to the same test module:

```rust
#[test]
fn validate_routes_rejects_star_in_middle() {
    let mut cfg = mk_cfg_with_routes(vec![]);
    cfg.upstreams.insert(
        "copilot".to_string(),
        crate::schema::UpstreamConfig::GithubCopilot(crate::schema::CopilotUpstream {
            tier: crate::schema::Tier::Standard,
            cost: None,
        }),
    );
    cfg.routes.push(crate::schema::RouteEntry::singular(
        "anthropic_messages",
        "claude-*-thinking",
        "copilot",
        "*",
    ));
    let err = validate_routes(&cfg).unwrap_err();
    assert!(
        err.contains("'*' is only allowed at the end"),
        "unexpected error message: {err}"
    );
}
```

- [ ] **Step 4: Run, expect FAIL**

Run: `rtk cargo nextest run -p agent-shim-config validate_routes_rejects_star_in_middle`
Expected: FAIL — `unwrap_err()` panics because today's `validate_routes` accepts any string.

- [ ] **Step 5: Implement the new rule in `validate_routes`**

In `crates/config/src/validation.rs`, inside the body of `pub fn validate_routes(cfg: &GatewayConfig) -> Result<(), String>` (currently ends at line ~552, just before `Ok(())`), insert this block AFTER rule 7 (the `reasoning_mapping` loop):

```rust
        // Rule 8 (2026-06-04): `model` may contain `*` only as the sole
        // character (the full catch-all) or as a single trailing character
        // (a prefix pattern like `claude-*`). Anything else is malformed.
        //
        // Predicate: model is valid iff at least one of
        //   (a) model == "*"
        //   (b) model has no '*' at all
        //   (c) model has exactly one '*', it's the final character, and the
        //       prefix (everything before it) is non-empty.
        if route.model != "*" {
            let star_count = route.model.matches('*').count();
            let star_at_end = route.model.ends_with('*');
            let prefix_non_empty = route.model.len() > 1;
            let valid_no_star = star_count == 0;
            let valid_prefix = star_count == 1 && star_at_end && prefix_non_empty;
            if !valid_no_star && !valid_prefix {
                return Err(format!(
                    "route[{i}] '{}/{}': '*' is only allowed at the end of the model pattern (e.g. \"claude-*\") or as the sole character (\"*\")",
                    route.frontend, route.model
                ));
            }
        }
```

- [ ] **Step 6: Run both tests, expect PASS**

Run: `rtk cargo nextest run -p agent-shim-config validate_routes_accepts_prefix_pattern validate_routes_rejects_star_in_middle`
Expected: both PASS.

- [ ] **Step 7: Add the remaining `*`-position tests**

Append to the same test module:

```rust
#[test]
fn validate_routes_rejects_star_at_start() {
    let mut cfg = mk_cfg_with_routes(vec![]);
    cfg.upstreams.insert(
        "copilot".to_string(),
        crate::schema::UpstreamConfig::GithubCopilot(crate::schema::CopilotUpstream {
            tier: crate::schema::Tier::Standard,
            cost: None,
        }),
    );
    cfg.routes.push(crate::schema::RouteEntry::singular(
        "anthropic_messages",
        "*claude",
        "copilot",
        "*",
    ));
    let err = validate_routes(&cfg).unwrap_err();
    assert!(err.contains("'*' is only allowed at the end"), "got: {err}");
}

#[test]
fn validate_routes_rejects_star_in_two_positions() {
    let mut cfg = mk_cfg_with_routes(vec![]);
    cfg.upstreams.insert(
        "copilot".to_string(),
        crate::schema::UpstreamConfig::GithubCopilot(crate::schema::CopilotUpstream {
            tier: crate::schema::Tier::Standard,
            cost: None,
        }),
    );
    cfg.routes.push(crate::schema::RouteEntry::singular(
        "anthropic_messages",
        "*claude*",
        "copilot",
        "*",
    ));
    let err = validate_routes(&cfg).unwrap_err();
    assert!(err.contains("'*' is only allowed at the end"), "got: {err}");
}

#[test]
fn validate_routes_rejects_double_star_suffix() {
    let mut cfg = mk_cfg_with_routes(vec![]);
    cfg.upstreams.insert(
        "copilot".to_string(),
        crate::schema::UpstreamConfig::GithubCopilot(crate::schema::CopilotUpstream {
            tier: crate::schema::Tier::Standard,
            cost: None,
        }),
    );
    cfg.routes.push(crate::schema::RouteEntry::singular(
        "anthropic_messages",
        "claude-**",
        "copilot",
        "*",
    ));
    let err = validate_routes(&cfg).unwrap_err();
    assert!(err.contains("'*' is only allowed at the end"), "got: {err}");
}

#[test]
fn validate_routes_accepts_exact_and_full_wildcard_regression() {
    // Pins that today's two existing model-field shapes still validate after
    // the new rule. Defensive against an accidental over-tightening of the
    // predicate during refactors.
    let mut cfg = mk_cfg_with_routes(vec![]);
    cfg.upstreams.insert(
        "copilot".to_string(),
        crate::schema::UpstreamConfig::GithubCopilot(crate::schema::CopilotUpstream {
            tier: crate::schema::Tier::Standard,
            cost: None,
        }),
    );
    cfg.routes.push(crate::schema::RouteEntry::singular(
        "anthropic_messages",
        "claude-sonnet-4-5",
        "copilot",
        "claude-sonnet-4-5",
    ));
    cfg.routes.push(crate::schema::RouteEntry::singular(
        "anthropic_messages",
        "*",
        "copilot",
        "*",
    ));
    validate_routes(&cfg).expect("exact + catch-all should pass");
}

#[test]
fn validate_routes_allows_overlapping_prefixes() {
    // Documented in the spec: overlap is a feature; the router disambiguates
    // by longest-prefix-wins. Validation must NOT reject.
    let mut cfg = mk_cfg_with_routes(vec![]);
    cfg.upstreams.insert(
        "copilot".to_string(),
        crate::schema::UpstreamConfig::GithubCopilot(crate::schema::CopilotUpstream {
            tier: crate::schema::Tier::Standard,
            cost: None,
        }),
    );
    cfg.routes.push(crate::schema::RouteEntry::singular(
        "anthropic_messages",
        "claude-*",
        "copilot",
        "*",
    ));
    cfg.routes.push(crate::schema::RouteEntry::singular(
        "anthropic_messages",
        "claude-3-*",
        "copilot",
        "*",
    ));
    validate_routes(&cfg).expect("overlapping prefixes should pass");
}

#[test]
fn validate_routes_allows_duplicate_prefix_silent_override() {
    // Documented in the spec: identical (frontend, model) entries silently
    // overwrite (last wins) — mirrors today's behavior for duplicate exact
    // routes. Validation does NOT enforce uniqueness here.
    let mut cfg = mk_cfg_with_routes(vec![]);
    cfg.upstreams.insert(
        "copilot".to_string(),
        crate::schema::UpstreamConfig::GithubCopilot(crate::schema::CopilotUpstream {
            tier: crate::schema::Tier::Standard,
            cost: None,
        }),
    );
    cfg.routes.push(crate::schema::RouteEntry::singular(
        "anthropic_messages",
        "claude-*",
        "copilot",
        "*",
    ));
    cfg.routes.push(crate::schema::RouteEntry::singular(
        "anthropic_messages",
        "claude-*",
        "copilot",
        "claude-3-5-sonnet-20241022",
    ));
    validate_routes(&cfg).expect("duplicate prefix should pass at config level");
}
```

- [ ] **Step 8: Run the full validation test module, expect all green**

Run: `rtk cargo nextest run -p agent-shim-config`
Expected: all tests PASS, including the 4 new rejection tests and the 3 new acceptance tests.

- [ ] **Step 9: Commit**

```bash
rtk git add crates/config/src/validation.rs && rtk git commit -m "feat(config): reject illegal '*' positions in route model field"
```

---

## Task 3: Verify `strict_upstream_models` is correct under prefix routes (no code change)

**Files:**
- Test: `crates/config/src/validation.rs` (same `tests` module, near the existing `validate_with_index` tests around line 2520+)

This task confirms via regression tests that the existing `validate_with_index` function already does the right thing for prefix routes. Per spec §5, no implementation change.

- [ ] **Step 1: Add the literal-`upstream_model` regression test**

```rust
#[test]
fn validate_with_index_checks_literal_upstream_model_under_prefix_route() {
    // Per spec §5: validate_with_index reads only `upstream_model`, never
    // `model`. So a prefix route with a literal upstream_model that mistypes
    // the discovered catalog still fails at startup.
    let mut cfg = mk_cfg_with_routes(vec![]);
    cfg.upstreams.insert(
        "copilot".to_string(),
        crate::schema::UpstreamConfig::GithubCopilot(crate::schema::CopilotUpstream {
            tier: crate::schema::Tier::Standard,
            cost: None,
        }),
    );
    cfg.validation.strict_upstream_models = true;
    cfg.routes.push(crate::schema::RouteEntry::singular(
        "anthropic_messages",
        "claude-*",
        "copilot",
        "claude-typo-not-in-catalog",
    ));
    let discovered = vec![(
        "copilot".to_string(),
        vec!["claude-sonnet-4-5".to_string()],
    )];
    let err = validate_with_index(&cfg, &discovered).unwrap_err();
    assert!(matches!(
        err,
        crate::ValidationError::UnknownUpstreamModel { .. }
    ));
}

#[test]
fn validate_with_index_skips_wildcard_upstream_model_under_prefix_route() {
    // Per spec §5: upstream_model == "*" is the pass-through marker; the
    // checker skips it regardless of the model-field shape.
    let mut cfg = mk_cfg_with_routes(vec![]);
    cfg.upstreams.insert(
        "copilot".to_string(),
        crate::schema::UpstreamConfig::GithubCopilot(crate::schema::CopilotUpstream {
            tier: crate::schema::Tier::Standard,
            cost: None,
        }),
    );
    cfg.validation.strict_upstream_models = true;
    cfg.routes.push(crate::schema::RouteEntry::singular(
        "anthropic_messages",
        "claude-*",
        "copilot",
        "*",
    ));
    let discovered = vec![("copilot".to_string(), vec![])]; // empty catalog
    validate_with_index(&cfg, &discovered).expect("pass-through should skip the check");
}
```

- [ ] **Step 2: Run the new tests, expect PASS without touching `validate_with_index`**

Run: `rtk cargo nextest run -p agent-shim-config validate_with_index_checks_literal_upstream_model_under_prefix_route validate_with_index_skips_wildcard_upstream_model_under_prefix_route`
Expected: both PASS (they confirm the existing implementation is already correct for prefix routes).

- [ ] **Step 3: Commit**

```bash
rtk git add crates/config/src/validation.rs && rtk git commit -m "test(config): pin strict_upstream_models behavior under prefix routes"
```

---

## Task 4: Refactor `StaticRouter` to add the prefix-wildcard tier

This task is one atomic refactor: introduce `PrefixRoute` storage, rename
`routes` → `exact` and `route_entries` → `exact_entries`, extend
`from_config` to partition into three buckets (with last-wins dedup for
duplicate prefixes per spec §2.4), extend `resolve_route` with the new
middle tier, split the resilience-config helpers, and update the
`Router` impl. All in one commit so the build never breaks mid-task.

**Files:**
- Modify: `crates/router/src/static_routes.rs` — struct definition, `from_config`, `resolve_route`, helper functions, `Router for StaticRouter` impl

- [ ] **Step 1: Replace the `StaticRouter` struct definition and add `PrefixRoute`**

In `crates/router/src/static_routes.rs`, replace the existing
`pub struct StaticRouter { ... }` block (currently lines ~28–43) with:

```rust
/// A static router built from `GatewayConfig.routes`.
pub struct StaticRouter {
    /// Exact route table keyed by `(frontend, model)`. O(1) lookup; the hot
    /// path for well-tuned configs. Each entry holds the full fallback chain
    /// (`Vec<BackendTarget>` len 1 for singular config, len N for array).
    exact: HashMap<RouteKey, Vec<BackendTarget>>,
    /// Source `RouteEntry` for each exact route so resolution can recover
    /// per-route resilience policy (retry, breaker).
    exact_entries: HashMap<RouteKey, RouteEntry>,

    /// Prefix-wildcard route table, partitioned by frontend. Each per-frontend
    /// `Vec<PrefixRoute>` is sorted at startup by `prefix.len()` descending,
    /// so a linear scan's first hit IS the longest prefix match. Real-world
    /// per-frontend prefix counts are < 10; the linear scan is effectively
    /// O(1) at production scale.
    prefixes: HashMap<FrontendKind, Vec<PrefixRoute>>,

    /// Catch-all wildcard (`model: "*"`), at most one per frontend.
    wildcards: HashMap<FrontendKind, WildcardTarget>,
    /// `RouteEntry` source for wildcard routes, mirroring `exact_entries`.
    wildcard_entries: HashMap<FrontendKind, RouteEntry>,
}

/// A prefix-wildcard route (`model: "claude-*"`). Stored separately from both
/// exact and full-wildcard routes so:
///   - `list_routes()` ignores them structurally (catalog excludes prefix).
///   - The hot exact-lookup path is not touched.
///   - Longest-prefix-wins is a simple sorted-Vec scan.
struct PrefixRoute {
    /// `model` field with the trailing `*` stripped. Non-empty by validation.
    prefix: String,
    /// Same `Vec<BackendTarget>` shape exact routes produce.
    chain: Vec<BackendTarget>,
    /// True when the source `upstream_model` was `"*"` (singular form only);
    /// at lookup time the router overwrites `chain[0].model` with the
    /// inbound model string.
    upstream_model_passthrough: bool,
    /// Source `RouteEntry` so per-route retry/breaker propagate the same way
    /// they do for exact routes.
    entry: RouteEntry,
}
```

- [ ] **Step 2: Replace `from_config` with the three-bucket partition (final clean version)**

Replace the entire body of `pub fn from_config(cfg: &GatewayConfig) -> Self`
(currently lines ~46–158) with:

```rust
    pub fn from_config(cfg: &GatewayConfig) -> Self {
        let mut exact = HashMap::new();
        let mut exact_entries = HashMap::new();
        let mut prefixes: HashMap<FrontendKind, Vec<PrefixRoute>> = HashMap::new();
        let mut wildcards = HashMap::new();
        let mut wildcard_entries = HashMap::new();
        for entry in &cfg.routes {
            let frontend = match entry.frontend.as_str() {
                "anthropic_messages" | "anthropic" => FrontendKind::AnthropicMessages,
                "openai_chat" | "openai" => FrontendKind::OpenAiChat,
                "openai_responses" | "responses" => FrontendKind::OpenAiResponses,
                other => {
                    tracing::warn!("unknown frontend kind in route config: {other}");
                    continue;
                }
            };
            let default_reasoning_effort = entry
                .reasoning_effort
                .as_deref()
                .and_then(ReasoningEffort::parse);
            if entry.reasoning_effort.is_some() && default_reasoning_effort.is_none() {
                tracing::warn!(
                    value = ?entry.reasoning_effort,
                    "ignoring unknown reasoning_effort in route config (expected minimal/low/medium/high/xhigh)"
                );
            }
            let reasoning_mapping: Vec<MappingRule> = entry
                .reasoning_mapping
                .iter()
                .filter_map(|rule| {
                    let m = ReasoningEffort::parse(&rule.r#match)?;
                    let s = ReasoningEffort::parse(&rule.set)?;
                    Some(MappingRule { r#match: m, set: s })
                })
                .collect();
            let policy = RoutePolicy {
                default_reasoning_effort,
                default_anthropic_beta: entry.anthropic_beta.clone(),
                reasoning_mapping,
            };

            let targets: Vec<BackendTarget> = if !entry.upstreams.is_empty() {
                entry
                    .upstreams
                    .iter()
                    .map(|u| BackendTarget {
                        provider: u.name.clone(),
                        model: u.model.clone(),
                        policy: policy.clone(),
                    })
                    .collect()
            } else {
                let provider = entry
                    .upstream
                    .clone()
                    .expect("validate_routes guarantees singular routes have `upstream` set");
                let upstream_model = entry
                    .upstream_model
                    .clone()
                    .expect("validate_routes guarantees singular routes have `upstream_model` set");
                vec![BackendTarget {
                    provider,
                    model: upstream_model,
                    policy: policy.clone(),
                }]
            };

            // Partition by `model` field shape into exactly one of three
            // buckets. `validate_routes` rule 8 guarantees `*` is either
            // absent, the sole character, or the final character; this
            // matches that grammar.
            if entry.model == "*" {
                let head = &targets[0];
                wildcards.insert(
                    frontend,
                    WildcardTarget {
                        provider: head.provider.clone(),
                        upstream_model: head.model.clone(),
                        policy: policy.clone(),
                    },
                );
                wildcard_entries.insert(frontend, entry.clone());
            } else if let Some(prefix) = entry.model.strip_suffix('*') {
                // Prefix wildcard. validate_routes ensures `prefix` is
                // non-empty and contains no other `*`.
                //
                // Pass-through is only meaningful in the singular form; the
                // array form specifies per-element model strings explicitly.
                let upstream_model_passthrough = entry.upstreams.is_empty()
                    && entry.upstream_model.as_deref() == Some("*");
                let route = PrefixRoute {
                    prefix: prefix.to_string(),
                    chain: targets,
                    upstream_model_passthrough,
                    entry: entry.clone(),
                };
                let bucket = prefixes.entry(frontend).or_default();
                // Spec §2.4: identical (frontend, model) entries silently
                // overwrite — later wins, mirroring HashMap-replacement
                // semantics for exact routes. For the Vec-based prefix
                // bucket, achieve this by removing any earlier entry with
                // the same literal prefix before pushing.
                bucket.retain(|existing| existing.prefix != route.prefix);
                bucket.push(route);
            } else {
                let key = RouteKey {
                    frontend,
                    model: entry.model.clone(),
                };
                exact.insert(key.clone(), targets);
                exact_entries.insert(key, entry.clone());
            }
        }

        // Sort each frontend's prefix bucket by literal-prefix length
        // descending. Rust's sort is stable, so when two distinct prefixes
        // happen to be the same length the YAML appearance order is
        // preserved (first-in-YAML wins).
        for routes in prefixes.values_mut() {
            routes.sort_by(|a, b| b.prefix.len().cmp(&a.prefix.len()));
        }

        Self {
            exact,
            exact_entries,
            prefixes,
            wildcards,
            wildcard_entries,
        }
    }
```

- [ ] **Step 3: Replace `resolve_route` with the three-tier lookup**

Replace the existing `pub fn resolve_route(...)` body (currently lines ~160–198) with:

```rust
    pub fn resolve_route(
        &self,
        frontend: FrontendKind,
        model: &str,
    ) -> Result<ResolvedRoute, RouteError> {
        let key = RouteKey {
            frontend,
            model: model.to_string(),
        };

        // Tier 1: exact lookup. O(1) hot path; unchanged from pre-prefix.
        if let Some(targets) = self.exact.get(&key) {
            return Ok(ResolvedRoute {
                chain: targets.clone(),
                retry: self.retry_config_for_exact(&key, model),
                breaker: self.breaker_config_for_exact(&key, model),
                route_label: route_label(frontend, model),
            });
        }

        // Tier 2: prefix lookup. Sorted longest-first at startup, so the
        // first `starts_with` hit IS the longest-prefix match. Linear scan
        // over a per-frontend bucket (real-world size < 10).
        if let Some(bucket) = self.prefixes.get(&frontend) {
            for pr in bucket {
                if model.starts_with(&pr.prefix) {
                    tracing::debug!(
                        frontend = ?frontend,
                        inbound_model = %model,
                        matched_prefix = %pr.prefix,
                        provider = %pr.chain[0].provider,
                        "prefix route hit"
                    );
                    let mut chain = pr.chain.clone();
                    if pr.upstream_model_passthrough {
                        // Singular-form pass-through: rewrite chain head to
                        // the inbound model. Fuzzy upgrade (in ModelResolver)
                        // canonicalizes this against the upstream's
                        // discovered catalog.
                        chain[0].model = model.to_string();
                    }
                    return Ok(ResolvedRoute {
                        chain,
                        retry: pr.entry.retry.clone(),
                        breaker: pr.entry.breaker.clone(),
                        route_label: route_label(frontend, model),
                    });
                }
            }
        }

        // Tier 3: catch-all wildcard. Unchanged from pre-prefix.
        if let Some(wc) = self.wildcards.get(&frontend) {
            let upstream_model = if wc.upstream_model == "*" {
                model.to_string()
            } else {
                wc.upstream_model.clone()
            };
            return Ok(ResolvedRoute {
                chain: vec![BackendTarget {
                    provider: wc.provider.clone(),
                    model: upstream_model,
                    policy: wc.policy.clone(),
                }],
                retry: self.retry_config_for_wildcard(frontend, model),
                breaker: self.breaker_config_for_wildcard(frontend, model),
                route_label: route_label(frontend, model),
            });
        }

        Err(RouteError::NoRoute {
            frontend,
            model: model.to_string(),
        })
    }
```

- [ ] **Step 4: Split the resilience-config helpers**

The old helpers (`retry_config_for`, `breaker_config_for`) accepted both
a `RouteKey` AND a `FrontendKind`, falling back to wildcard. Now that the
prefix tier reads `pr.entry.retry` / `pr.entry.breaker` directly inline,
helpers only need to serve exact and wildcard tiers. Replace
`fn retry_config_for(...)` and `fn breaker_config_for(...)`
(currently lines ~200–237) with:

```rust
    fn retry_config_for_exact(&self, key: &RouteKey, model: &str) -> RetryConfig {
        match self.exact_entries.get(key) {
            Some(entry) => entry.retry.clone(),
            None => {
                tracing::debug!(
                    model = %model,
                    "no exact-route retry policy; using RetryConfig defaults"
                );
                RetryConfig::default()
            }
        }
    }

    fn breaker_config_for_exact(&self, key: &RouteKey, model: &str) -> BreakerConfig {
        match self.exact_entries.get(key) {
            Some(entry) => entry.breaker.clone(),
            None => {
                tracing::debug!(
                    model = %model,
                    "no exact-route breaker policy; using BreakerConfig defaults"
                );
                BreakerConfig::default()
            }
        }
    }

    fn retry_config_for_wildcard(&self, frontend: FrontendKind, model: &str) -> RetryConfig {
        match self.wildcard_entries.get(&frontend) {
            Some(entry) => entry.retry.clone(),
            None => {
                tracing::debug!(
                    model = %model,
                    "no wildcard-route retry policy; using RetryConfig defaults"
                );
                RetryConfig::default()
            }
        }
    }

    fn breaker_config_for_wildcard(&self, frontend: FrontendKind, model: &str) -> BreakerConfig {
        match self.wildcard_entries.get(&frontend) {
            Some(entry) => entry.breaker.clone(),
            None => {
                tracing::debug!(
                    model = %model,
                    "no wildcard-route breaker policy; using BreakerConfig defaults"
                );
                BreakerConfig::default()
            }
        }
    }
```

- [ ] **Step 5: Update the `Router for StaticRouter` impl for the renamed field**

Replace the `impl Router for StaticRouter` block (lines ~252–269) with:

```rust
impl Router for StaticRouter {
    fn resolve(
        &self,
        frontend: FrontendKind,
        model: &str,
    ) -> Result<Vec<BackendTarget>, RouteError> {
        self.resolve_route(frontend, model).map(|route| route.chain)
    }

    fn list_routes(&self) -> Vec<(FrontendKind, String)> {
        // Both wildcards (`*`) and prefix wildcards (`claude-*`) are
        // intentionally excluded: catalog surfaces enumerate concrete
        // aliases only. Prefix routes live in `self.prefixes` and not in
        // `self.exact_entries`, so this iteration excludes them
        // structurally — no `if pattern { skip }` needed.
        self.exact_entries
            .keys()
            .map(|k| (k.frontend, k.model.clone()))
            .collect()
    }
}
```

- [ ] **Step 6: Build and run all existing tests to confirm zero regression**

Run: `rtk cargo build -p agent-shim-router && rtk cargo nextest run -p agent-shim-router`
Expected: build PASS, all existing tests PASS. The renames are internal;
test bodies that construct configs via `cfg_with_route` (a test-private
helper) go through `from_config` and don't reference the renamed fields.

- [ ] **Step 7: Commit**

```bash
rtk git add crates/router/src/static_routes.rs && rtk git commit -m "feat(router): add prefix-wildcard tier with last-wins dedup"
```

---

## Task 5: Unit tests for prefix-route matching, priority, and edge cases

**Files:**
- Test: `crates/router/src/static_routes.rs` (extend the existing `#[cfg(test)] mod tests` block around line 271+)

- [ ] **Step 1: Add the pass-through test**

Append to the `tests` module:

```rust
    #[test]
    fn prefix_route_passes_inbound_model_through_when_upstream_model_is_star() {
        let cfg = cfg_with_route("anthropic_messages", "claude-*", "copilot", "*");
        let router = StaticRouter::from_config(&cfg);
        let chain = router
            .resolve(FrontendKind::AnthropicMessages, "claude-sonnet-4-5")
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].provider, "copilot");
        assert_eq!(chain[0].model, "claude-sonnet-4-5");
    }
```

- [ ] **Step 2: Add the literal-override test**

```rust
    #[test]
    fn prefix_route_overrides_model_when_upstream_model_is_literal() {
        let cfg = cfg_with_route(
            "anthropic_messages",
            "claude-*",
            "copilot",
            "claude-3-5-sonnet-20241022",
        );
        let router = StaticRouter::from_config(&cfg);
        let chain = router
            .resolve(FrontendKind::AnthropicMessages, "claude-anything-at-all")
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].provider, "copilot");
        assert_eq!(chain[0].model, "claude-3-5-sonnet-20241022");
    }
```

- [ ] **Step 3: Add the exact-beats-prefix test**

```rust
    #[test]
    fn exact_route_beats_prefix_route() {
        let mut cfg = cfg_with_route("anthropic_messages", "claude-sonnet-4-5", "copilot", "claude-sonnet-4-5");
        cfg.routes.push(RouteEntry::singular(
            "anthropic_messages",
            "claude-*",
            "copilot",
            "claude-3-5-sonnet-20241022",
        ));
        let router = StaticRouter::from_config(&cfg);
        let chain = router
            .resolve(FrontendKind::AnthropicMessages, "claude-sonnet-4-5")
            .unwrap();
        assert_eq!(chain.len(), 1);
        // Exact wins: upstream_model is the literal "claude-sonnet-4-5",
        // NOT the prefix route's "claude-3-5-sonnet-20241022".
        assert_eq!(chain[0].model, "claude-sonnet-4-5");
    }
```

- [ ] **Step 4: Add the longest-prefix-wins test**

```rust
    #[test]
    fn longest_prefix_wins() {
        let mut cfg = cfg_with_route("anthropic_messages", "claude-*", "copilot", "short");
        cfg.routes.push(RouteEntry::singular(
            "anthropic_messages",
            "claude-3-*",
            "copilot",
            "mid",
        ));
        cfg.routes.push(RouteEntry::singular(
            "anthropic_messages",
            "claude-3-5-*",
            "copilot",
            "long",
        ));
        let router = StaticRouter::from_config(&cfg);

        let hit_long = router
            .resolve(FrontendKind::AnthropicMessages, "claude-3-5-sonnet")
            .unwrap();
        assert_eq!(hit_long[0].model, "long");

        let hit_mid = router
            .resolve(FrontendKind::AnthropicMessages, "claude-3-haiku")
            .unwrap();
        assert_eq!(hit_mid[0].model, "mid");

        let hit_short = router
            .resolve(FrontendKind::AnthropicMessages, "claude-opus-4-7")
            .unwrap();
        assert_eq!(hit_short[0].model, "short");
    }
```

- [ ] **Step 5: Add the prefix-falls-back-to-wildcard test**

```rust
    #[test]
    fn prefix_falls_back_to_full_wildcard_on_miss() {
        let mut cfg = cfg_with_route("anthropic_messages", "claude-*", "copilot", "claude-target");
        cfg.routes.push(RouteEntry::singular(
            "anthropic_messages",
            "*",
            "copilot",
            "*",
        ));
        let router = StaticRouter::from_config(&cfg);
        let chain = router
            .resolve(FrontendKind::AnthropicMessages, "gpt-4o")
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].provider, "copilot");
        assert_eq!(chain[0].model, "gpt-4o"); // wildcard pass-through
    }
```

- [ ] **Step 6: Add the complete-miss test**

```rust
    #[test]
    fn prefix_route_returns_no_route_on_complete_miss() {
        let cfg = cfg_with_route("anthropic_messages", "claude-*", "copilot", "*");
        let router = StaticRouter::from_config(&cfg);
        let err = router
            .resolve(FrontendKind::AnthropicMessages, "gpt-4o")
            .unwrap_err();
        assert!(matches!(
            err,
            RouteError::NoRoute {
                frontend: FrontendKind::AnthropicMessages,
                model,
            } if model == "gpt-4o"
        ));
    }
```

- [ ] **Step 7: Add the route_label test**

```rust
    #[test]
    fn prefix_route_carries_route_label_with_inbound_model() {
        let cfg = cfg_with_route("anthropic_messages", "claude-*", "copilot", "*");
        let router = StaticRouter::from_config(&cfg);
        let resolved = router
            .resolve_route(FrontendKind::AnthropicMessages, "claude-opus-4-7")
            .unwrap();
        // route_label uses the INBOUND model name (not the prefix pattern),
        // matching wildcard behavior so per-route metrics slice by real model
        // identity.
        assert_eq!(resolved.route_label.as_ref(), "anthropic_messages/claude-opus-4-7");
    }
```

- [ ] **Step 8: Add the per-route retry/breaker test**

```rust
    #[test]
    fn prefix_route_uses_route_specific_retry_and_breaker() {
        let mut cfg = cfg_with_route("anthropic_messages", "claude-*", "copilot", "*");
        cfg.routes[0].retry.max_attempts = 7;
        cfg.routes[0].breaker.failure_threshold_pct = 99;
        let router = StaticRouter::from_config(&cfg);
        let resolved = router
            .resolve_route(FrontendKind::AnthropicMessages, "claude-sonnet-4-5")
            .unwrap();
        assert_eq!(resolved.retry.max_attempts, 7);
        assert_eq!(resolved.breaker.failure_threshold_pct, 99);
    }
```

- [ ] **Step 9: Add the reasoning_mapping propagation test**

```rust
    #[test]
    fn prefix_route_carries_reasoning_mapping_to_policy() {
        use agent_shim_config::MappingRuleConfig;
        let mut cfg = cfg_with_route("anthropic_messages", "claude-*", "copilot", "*");
        cfg.routes[0].reasoning_mapping = vec![MappingRuleConfig {
            r#match: "max".into(),
            set: "xhigh".into(),
        }];
        let router = StaticRouter::from_config(&cfg);
        let chain = router
            .resolve(FrontendKind::AnthropicMessages, "claude-opus-4-7")
            .unwrap();
        let policy = &chain[0].policy;
        assert_eq!(policy.reasoning_mapping.len(), 1);
        assert_eq!(policy.reasoning_mapping[0].r#match, ReasoningEffort::Max);
        assert_eq!(policy.reasoning_mapping[0].set, ReasoningEffort::Xhigh);
    }
```

- [ ] **Step 10: Add the duplicate-prefix-last-wins test**

```rust
    #[test]
    fn duplicate_prefix_uses_last_definition() {
        // Spec §2.4: identical (frontend, model) entries silently overwrite.
        // Task 4 Step 2's from_config implements this for the prefix tier
        // via a `bucket.retain(|e| e.prefix != route.prefix)` call before
        // pushing — so the SECOND push wins, matching exact/wildcard
        // HashMap-replacement behavior.
        let mut cfg = cfg_with_route("anthropic_messages", "claude-*", "first", "first-model");
        // Add a second upstream so the second route's `upstream: "second"`
        // passes validation; cfg_with_route only registered "first".
        cfg.upstreams.insert(
            "second".to_string(),
            agent_shim_config::UpstreamConfig::GithubCopilot(
                agent_shim_config::CopilotUpstream {
                    tier: agent_shim_config::Tier::Standard,
                    cost: None,
                },
            ),
        );
        cfg.upstreams.insert(
            "first".to_string(),
            agent_shim_config::UpstreamConfig::GithubCopilot(
                agent_shim_config::CopilotUpstream {
                    tier: agent_shim_config::Tier::Standard,
                    cost: None,
                },
            ),
        );
        cfg.routes.push(RouteEntry::singular(
            "anthropic_messages",
            "claude-*",
            "second",
            "second-model",
        ));
        let router = StaticRouter::from_config(&cfg);
        let chain = router
            .resolve(FrontendKind::AnthropicMessages, "claude-anything")
            .unwrap();
        // Last-wins: the second push overwrites the first.
        assert_eq!(chain[0].provider, "second");
        assert_eq!(chain[0].model, "second-model");
    }
```

- [ ] **Step 11: Add the cross-frontend isolation test**

```rust
    #[test]
    fn cross_frontend_prefix_isolation() {
        let mut cfg = cfg_with_route("anthropic_messages", "claude-*", "copilot", "*");
        cfg.routes.push(RouteEntry::singular(
            "openai_chat",
            "gpt-*",
            "copilot",
            "*",
        ));
        let router = StaticRouter::from_config(&cfg);

        // anthropic_messages prefix should NOT capture openai_chat traffic.
        let err = router
            .resolve(FrontendKind::OpenAiChat, "claude-foo")
            .unwrap_err();
        assert!(matches!(err, RouteError::NoRoute { .. }));

        // And vice-versa.
        let err = router
            .resolve(FrontendKind::AnthropicMessages, "gpt-foo")
            .unwrap_err();
        assert!(matches!(err, RouteError::NoRoute { .. }));
    }
```

- [ ] **Step 12: Run all router tests, expect green**

Run: `rtk cargo nextest run -p agent-shim-router`
Expected: all PASS.

- [ ] **Step 13: Commit**

```bash
rtk git add crates/router/src/static_routes.rs && rtk git commit -m "test(router): cover prefix-route matching, priority, and edge cases"
```

---

## Task 6: Property test for longest-prefix determinism

**Files:**
- Test: `crates/router/src/static_routes.rs` (new `#[cfg(test)] mod proptests` block at the bottom of the file)

- [ ] **Step 1: Add the proptest module**

Append to `crates/router/src/static_routes.rs`, after the existing `mod tests`:

```rust
#[cfg(test)]
mod proptests {
    use super::*;
    use agent_shim_config::{GatewayConfig, RouteEntry, UpstreamConfig, CopilotUpstream, Tier};
    use proptest::prelude::*;

    fn cfg_with_prefixes(prefixes: &[&str]) -> GatewayConfig {
        let mut cfg = GatewayConfig {
            server: Default::default(),
            logging: Default::default(),
            upstreams: Default::default(),
            routes: vec![],
            plugins: ::std::collections::BTreeMap::new(),
            auth: Default::default(),
            rate_limit: Default::default(),
            copilot: None,
            admin: None,
            metrics: Default::default(),
            otel: None,
            shutdown: Default::default(),
            validation: Default::default(),
        };
        cfg.upstreams.insert(
            "copilot".to_string(),
            UpstreamConfig::GithubCopilot(CopilotUpstream {
                tier: Tier::Standard,
                cost: None,
            }),
        );
        for (i, p) in prefixes.iter().enumerate() {
            cfg.routes.push(RouteEntry::singular(
                "anthropic_messages",
                &format!("{p}*"),
                "copilot",
                &format!("mark-{i}"),
            ));
        }
        cfg
    }

    proptest! {
        /// For any set of distinct prefixes registered on one frontend, the
        /// router's choice for a given inbound model is the *longest* prefix
        /// among those that `starts_with` succeed against the inbound model.
        /// This guards against an accidental switch to BTreeMap iteration
        /// order or to unsorted bucket scans.
        #[test]
        fn longest_prefix_is_deterministic(
            prefixes in proptest::collection::btree_set("[a-z]{1,8}", 1..8usize),
            suffix in "[a-z]{0,8}",
        ) {
            let prefixes_vec: Vec<&str> = prefixes.iter().map(String::as_str).collect();
            let cfg = cfg_with_prefixes(&prefixes_vec);
            let router = StaticRouter::from_config(&cfg);

            // Build inbound by concatenating the LONGEST prefix with the
            // random suffix, so at least one prefix is guaranteed to match.
            let longest = prefixes_vec.iter().max_by_key(|p| p.len()).copied().unwrap();
            let inbound = format!("{longest}{suffix}");

            let resolved = router
                .resolve(FrontendKind::AnthropicMessages, &inbound)
                .expect("at least the longest prefix matches");

            // Compute the expected winner: longest prefix among matchers.
            // BTreeSet iteration is alphabetic; we sort by length descending,
            // and break ties by picking the lexicographically earliest (which
            // matches BTreeSet's natural order for same-length elements).
            let mut matchers: Vec<&&str> = prefixes_vec
                .iter()
                .filter(|p| inbound.starts_with(*p))
                .collect();
            matchers.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
            let expected_prefix = *matchers[0];
            let expected_idx = prefixes_vec.iter().position(|p| *p == expected_prefix).unwrap();
            let expected_mark = format!("mark-{expected_idx}");

            prop_assert_eq!(resolved[0].model, expected_mark);
        }
    }
}
```

> **Implementer note:** The "tie-break = lexicographic" assumption in the expected computation only holds because the test uses `BTreeSet` for input (which iterates sorted). If you ever switch the input to a `Vec` that preserves insertion order, also rewrite the tie-break to match the implementation's stable-sort behavior on push order.

- [ ] **Step 2: Run the proptest, expect PASS**

Run: `rtk cargo nextest run -p agent-shim-router longest_prefix_is_deterministic`
Expected: PASS (default proptest budget is 256 cases).

- [ ] **Step 3: Commit**

```bash
rtk git add crates/router/src/static_routes.rs && rtk git commit -m "test(router): proptest pinning longest-prefix determinism"
```

---

## Task 7: ModelResolver-level tests (prefix + fuzzy upgrade; catalog exclusion)

**Files:**
- Test: `crates/router/src/resolver.rs` (extend the existing `#[cfg(test)] mod tests` block)

- [ ] **Step 1: Add the prefix-route + fuzzy-upgrade integration test**

Append to the `tests` module in `crates/router/src/resolver.rs`:

```rust
    #[test]
    fn prefix_route_then_fuzzy_upgrades_per_chain_element() {
        // Prefix route with upstream_model passthrough: inbound model is
        // rewritten into chain[0].model in StaticRouter, then ModelResolver
        // upgrades it to the discovered canonical form in apply_fuzzy_upgrades.
        let cfg = cfg_with_route("anthropic_messages", "claude-*", "copilot", "*");
        let resolver = resolver_with(cfg, "copilot", &["claude-sonnet-4-5-20250514"]);
        let chain = resolver
            .resolve(FrontendKind::AnthropicMessages, "claude-sonnet-4-5")
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].provider, "copilot");
        // Pass-through gave "claude-sonnet-4-5"; fuzzy upgrade promoted to
        // the discovered canonical "claude-sonnet-4-5-20250514".
        assert_eq!(chain[0].model, "claude-sonnet-4-5-20250514");
    }
```

- [ ] **Step 2: Add the catalog-exclusion test**

```rust
    #[test]
    fn list_catalog_excludes_prefix_routes() {
        // A config with one prefix route and one exact route should yield a
        // catalog that contains ONLY the exact route's record. The prefix
        // route is structurally absent because StaticRouter::list_routes
        // iterates exact_entries.keys(); prefix routes live in self.prefixes.
        let mut cfg = cfg_with_route(
            "anthropic_messages",
            "claude-sonnet-4-5",
            "copilot",
            "claude-sonnet-4-5",
        );
        cfg.routes.push(RouteEntry::singular(
            "anthropic_messages",
            "claude-*",
            "copilot",
            "*",
        ));
        let resolver = resolver_with(cfg, "copilot", &["claude-sonnet-4-5"]);
        let catalog = resolver.list_catalog();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].id, "claude-sonnet-4-5");
        // And the prefix-pattern id must NOT appear.
        assert!(catalog.iter().all(|r| r.id != "claude-*"));
    }
```

- [ ] **Step 3: Run, expect PASS**

Run: `rtk cargo nextest run -p agent-shim-router prefix_route_then_fuzzy_upgrades_per_chain_element list_catalog_excludes_prefix_routes`
Expected: both PASS.

- [ ] **Step 4: Commit**

```bash
rtk git add crates/router/src/resolver.rs && rtk git commit -m "test(router): ModelResolver covers prefix-route fuzzy upgrade and catalog exclusion"
```

---

## Task 8: Update `docs/configuration.md` Routes section

**Files:**
- Modify: `docs/configuration.md` (Routes section, around lines 125–151)

- [ ] **Step 1: Replace the Routes section**

In `docs/configuration.md`, locate the `## Routes` heading (around line 125). Replace the block from `## Routes` through the paragraph that ends with `Wildcards (model: "*") are matched only when no exact route exists.` (the next blank line before `## Logging`) with:

```markdown
## Routes

```yaml
routes:
  - frontend: anthropic_messages   # the inbound API dialect
    model: claude-opus-4-7         # what the agent requests
    upstream: copilot              # which upstream serves it
    upstream_model: claude-opus-4-7  # what the upstream sees
    reasoning_effort: high         # optional: minimal | low | medium | high | xhigh | max
    anthropic_beta: context-1m-2025-08-07  # optional: default anthropic-beta header
    reasoning_mapping:             # optional: per-route effort rewrite table
      - match: max                 # canonical inbound effort
        set:   xhigh               # canonical outbound effort

  # Prefix-wildcard example: every claude-* alias maps to Copilot.
  - frontend: anthropic_messages
    model: claude-*                # trailing '*' = prefix pattern
    upstream: copilot
    upstream_model: "*"            # pass the inbound model through verbatim

  # Full catch-all (at most one per frontend).
  - frontend: anthropic_messages
    model: "*"
    upstream: copilot
    upstream_model: "*"
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `frontend` | enum | — | `anthropic_messages`, `openai_chat`, or `openai_responses` |
| `model` | string | — | What the agent asks for. Three forms: **exact** (`claude-sonnet-4-5`), **prefix wildcard** (`claude-*` — a literal followed by a single trailing `*`), or **full catch-all** (`*`). |
| `upstream` | string | — | A key under `upstreams:` |
| `upstream_model` | string | — | Model name sent to the upstream. `"*"` (in any wildcard route) means pass the inbound model through verbatim. |
| `reasoning_effort` | enum | — | Default thinking effort applied when the request omits one. One of `minimal/low/medium/high/xhigh/max`. |
| `anthropic_beta` | string | — | Default `anthropic-beta` header value applied when the request omits one |
| `reasoning_mapping` | array | `[]` | Ordered list of `{ match, set }` rules that rewrite the inbound canonical effort before forwarding to the upstream. First match wins; unmatched passes through. Both `match` and `set` must be canonical effort strings (`minimal/low/medium/high/xhigh/max`); unknown values fail validation at config-load. |

### Match priority

The route table is consulted by `agent_shim_router::StaticRouter` in this order:

1. **Exact match** — `(frontend, model)` equal to the inbound pair.
2. **Longest-prefix match** — among prefix-wildcard routes on the same frontend whose literal prefix is a `starts_with` match against the inbound model, the route with the **longest literal prefix** wins. Ties between same-length prefixes on the same frontend are broken by YAML appearance order (first wins).
3. **Full catch-all** — `model: "*"` for the frontend, if present.
4. **`NoRoute` error** if none of the above match.

`*` is allowed in `model` only as the sole character (the full catch-all) or as the **last** character (a prefix pattern like `claude-*`). Any other position fails config validation at startup.

### Duplicate routes

Identical `(frontend, model)` entries — whether exact, prefix, or full
catch-all — silently overwrite for exact and catch-all forms (later wins,
matching `HashMap` insertion semantics). For prefix duplicates, the
implementation currently keeps the **first** registered entry's chain
when two identical prefixes coexist on the same frontend; the duplicate
is harmless but inert. Either way, validation does not flag duplicates.

### `strict_upstream_models` under wildcard routes

`validation.strict_upstream_models: true` enforces that every literal
`upstream_model` referenced by a route appears in the upstream's
discovered catalog. The check looks **only at `upstream_model`** and is
independent of the `model` field's shape:

- `upstream_model: "*"` (pass-through) is always skipped — the runtime
  value is not known until a request arrives.
- A literal `upstream_model` is always checked, whether `model` is
  exact, a prefix wildcard, or the catch-all `*`.

So a route like `model: claude-*` + `upstream_model: claude-typo` will
still fail to start under strict mode if `claude-typo` is not in the
upstream's catalog — the prefix wildcard does not exempt it.
```

- [ ] **Step 2: Verify the markdown renders by reading the file**

Run: `rtk read docs/configuration.md`
Look for: the three-form `model` row, the new "Match priority", "Duplicate routes", and "`strict_upstream_models` under wildcard routes" subsections.

- [ ] **Step 3: Commit**

```bash
rtk git add docs/configuration.md && rtk git commit -m "docs(config): document prefix wildcard, priority, duplicates, and strict mode"
```

---

## Task 9: Final workspace verification

**Files:** none — this task only runs the canonical CI commands.

- [ ] **Step 1: `cargo fmt --all --check`**

Run: `rtk cargo fmt --all -- --check`
Expected: PASS (no formatting drift).

If FAIL: run `rtk cargo fmt --all`, review the diff, commit:
```bash
rtk git add -A && rtk git commit -m "chore(fmt): cargo fmt for route-wildcard changes"
```

- [ ] **Step 2: `cargo clippy --workspace --all-targets -- -D warnings`**

Run: `rtk cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS (CI treats warnings as errors).

If FAIL: fix each warning individually; commit per fix or as one polish commit:
```bash
rtk git add -A && rtk git commit -m "chore(clippy): address warnings on route-wildcard touch points"
```

- [ ] **Step 3: `cargo nextest run --workspace`**

Run: `rtk cargo nextest run --workspace`
Expected: ALL tests PASS, including the new prefix-wildcard tests in `agent-shim-config` and `agent-shim-router`.

- [ ] **Step 4: `cargo deny check`**

Run: `rtk cargo deny check`
Expected: PASS (no new dependency was introduced; the existing license/advisory state is unchanged).

- [ ] **Step 5: Verify git status is clean and review the commit log**

Run: `rtk git status && rtk git log --oneline -12`
Expected:
- `git status` shows working tree clean.
- The last commits include (in order):
  - `docs(config): describe prefix wildcard shape for RouteEntry.model`
  - `feat(config): reject illegal '*' positions in route model field`
  - `test(config): pin strict_upstream_models behavior under prefix routes`
  - `feat(router): add prefix-wildcard tier with last-wins dedup`
  - `test(router): cover prefix-route matching, priority, and edge cases`
  - `test(router): proptest pinning longest-prefix determinism`
  - `test(router): ModelResolver covers prefix-route fuzzy upgrade and catalog exclusion`
  - `docs(config): document prefix wildcard, priority, duplicates, and strict mode`
  - (optionally) `chore(fmt)` / `chore(clippy)` polish commits

The plan is complete. The feature is shippable as part of the current workspace version with no follow-up required.
