# Plan 02 — ArcSwap<LimiterRegistry> + reload integration (Phase 6)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-12-phase-6-cost-aware-gateway-design.md`](../specs/2026-05-12-phase-6-cost-aware-gateway-design.md) (§1 pillar 2, §6.2 atomicity, D4).

**Goal:** Close the v0.5 §5.4 deferral. `LimiterRegistry` becomes hot-swappable: a reload that changes `rate_limit.*` produces a new registry on the next request, no process restart needed.

**Architecture:** `AppCore::limiter_registry` changes from `Arc<LimiterRegistry>` to `Arc<ArcSwap<LimiterRegistry>>`. `ResilientCaller::limiters` mirrors this. The reload-applying task in `commands::serve::handle_reload` rebuilds a fresh `LimiterRegistry::from_config(&candidate.rate_limit)` and stores it atomically after the `AppSnapshot` swap. Existing rate-limit semantics unchanged on the read path — `load_full()` returns an `Arc<LimiterRegistry>` indistinguishable from the previous `Arc<LimiterRegistry>`.

**Tech stack:** Uses `arc_swap::ArcSwap` already in workspace deps from v0.5. No new crates.

**Frozen-core impact:** Touches `crates/router/src/rate_limit.rs` and `crates/router/src/resilient_caller.rs`. Both are router-internal — the frozen core (core+frontends) is unaffected. providers/src/ is unchanged from P01 — this plan does NOT touch it.

**Test target:** 643 (after P01) → 644 (+1 integration: `reload_rate_limit_buckets_reset.rs`).

---

## File Structure

`crates/router/src/`:
- Modify: `resilient_caller.rs` — `limiters: Arc<ArcSwap<LimiterRegistry>>`. Every read goes through `self.limiters.load_full()`. Test helpers updated.

`crates/gateway/src/`:
- Modify: `state.rs` — `AppCore::limiter_registry: Arc<ArcSwap<LimiterRegistry>>`. `AppState::build` wraps the initial registry. Compile-time pin test updated.
- Modify: `commands/serve.rs` — `handle_reload` rebuilds the limiter registry and stores it atomically after the `AppSnapshot` swap (commit point 2 per spec §6.2).

`crates/gateway/tests/`:
- Create: `reload_rate_limit_buckets_reset.rs` — POST /admin/reload with a `rate_limit.per_route` change, then prove the new bucket parameters are in effect on the next request.
- Modify: any existing test that constructs `AppCore` directly (the two from v0.5: `vision_capability_mismatch.rs`, `responses_capability_gate_deepseek.rs`) — change `limiter_registry: Arc<LimiterRegistry>` field init to wrap in ArcSwap.

---

## Tasks

### Task 1: Refactor `ResilientCaller` to use `ArcSwap<LimiterRegistry>`

**Files:**
- Modify: `crates/router/src/resilient_caller.rs`

- [ ] **Step 1: Add arc-swap to router dev-deps if missing**

```bash
grep arc-swap crates/router/Cargo.toml
```

If missing, add to `[dependencies]`:

```toml
arc-swap.workspace = true
```

- [ ] **Step 2: Change the field type**

In `crates/router/src/resilient_caller.rs:59`, change:

```rust
limiters: Arc<crate::rate_limit::LimiterRegistry>,
```

to:

```rust
limiters: Arc<arc_swap::ArcSwap<crate::rate_limit::LimiterRegistry>>,
```

- [ ] **Step 3: Change the constructor signature**

At `resilient_caller.rs:66`, change:

```rust
limiters: Arc<crate::rate_limit::LimiterRegistry>,
```

to:

```rust
limiters: Arc<arc_swap::ArcSwap<crate::rate_limit::LimiterRegistry>>,
```

- [ ] **Step 4: Update every read site**

Find every line in `resilient_caller.rs` that reads `self.limiters.<method>` (e.g., the `check_pre_chain` and any post-call gates). Wrap with `load_full()`:

```rust
// before:
self.limiters.check(...)
// after:
self.limiters.load_full().check(...)
```

Use `cargo build -p agent-shim-router 2>&1 | tail -10` after this step to surface every callsite the compiler flags.

- [ ] **Step 5: Update test helpers in the same file**

The test module at the bottom of `resilient_caller.rs` constructs limiters via `Arc::new(crate::rate_limit::LimiterRegistry::from_config(...))` and `Arc::new(crate::rate_limit::LimiterRegistry::disabled())`. Each becomes:

```rust
// before:
let limiters = Arc::new(crate::rate_limit::LimiterRegistry::from_config(&cfg));
// after:
let limiters = Arc::new(arc_swap::ArcSwap::from_pointee(
    crate::rate_limit::LimiterRegistry::from_config(&cfg),
));
```

`from_pointee` constructs `ArcSwap<T>` from a `T` directly — no need to double-wrap with Arc. The fn helper `disabled_limiter()` at line 497 also updates:

```rust
fn disabled_limiter() -> Arc<arc_swap::ArcSwap<crate::rate_limit::LimiterRegistry>> {
    Arc::new(arc_swap::ArcSwap::from_pointee(
        crate::rate_limit::LimiterRegistry::disabled(),
    ))
}
```

- [ ] **Step 6: Run router tests**

```bash
cargo test -p agent-shim-router 2>&1 | tail -15
```

Expected: all router tests pass (no behavioural change — `load_full()` returns the same `Arc<LimiterRegistry>` as before).

- [ ] **Step 7: Commit**

```bash
git add crates/router/Cargo.toml crates/router/src/resilient_caller.rs
git commit -m "refactor(router): ResilientCaller limiters behind ArcSwap (Plan 02 P02 T1)"
```

---

### Task 2: Refactor `AppCore::limiter_registry` to ArcSwap

**Files:**
- Modify: `crates/gateway/src/state.rs`

- [ ] **Step 1: Change the field type in AppCore**

In `crates/gateway/src/state.rs:100`, change:

```rust
pub limiter_registry: Arc<LimiterRegistry>,
```

to:

```rust
pub limiter_registry: Arc<arc_swap::ArcSwap<LimiterRegistry>>,
```

- [ ] **Step 2: Wrap the construction**

In `AppState::build`, around line 280, change:

```rust
let limiter_registry = Arc::new(if config.rate_limit.enabled {
    LimiterRegistry::from_config(&config.rate_limit)
} else {
    LimiterRegistry::disabled()
});
```

to:

```rust
let limiter_registry = Arc::new(arc_swap::ArcSwap::from_pointee(
    if config.rate_limit.enabled {
        LimiterRegistry::from_config(&config.rate_limit)
    } else {
        LimiterRegistry::disabled()
    },
));
```

- [ ] **Step 3: Update `ResilientCaller::new` callsite**

Same file, line 289 area, the `Arc::clone(&limiter_registry)` to `ResilientCaller::new` already passes the right type — `ResilientCaller` now expects `Arc<ArcSwap<LimiterRegistry>>` after Task 1.

- [ ] **Step 4: Update the compile-time pin test**

In the same file's `#[cfg(test)] mod tests` block, around line 388:

```rust
// before:
let _: &Arc<agent_shim_router::LimiterRegistry> = &state.core.limiter_registry;
// after:
let _: &Arc<arc_swap::ArcSwap<agent_shim_router::LimiterRegistry>> = &state.core.limiter_registry;
```

- [ ] **Step 5: Build the workspace**

```bash
cargo build --workspace 2>&1 | tail -15
```

Expected: clean, except for two test files that construct AppCore directly — those error out in Task 3.

- [ ] **Step 6: Don't commit yet**

The two direct-AppCore-construction tests in Task 3 need the same change to keep the workspace compiling.

---

### Task 3: Update tests that construct AppCore directly

**Files:**
- Modify: `crates/gateway/tests/vision_capability_mismatch.rs`
- Modify: `crates/gateway/tests/responses_capability_gate_deepseek.rs`

These two test files (from v0.5 P01 era) construct `AppCore` directly with a struct literal because they need to inject mock providers without going through `AppState::new`'s real provider discovery.

- [ ] **Step 1: Identify the field init line**

```bash
grep -n "limiter_registry:" crates/gateway/tests/vision_capability_mismatch.rs
grep -n "limiter_registry:" crates/gateway/tests/responses_capability_gate_deepseek.rs
```

Expected: each has a `limiter_registry: Arc::new(agent_shim_router::LimiterRegistry::disabled()),` line.

- [ ] **Step 2: Wrap in ArcSwap (vision_capability_mismatch.rs)**

Change:

```rust
limiter_registry: Arc::new(agent_shim_router::LimiterRegistry::disabled()),
```

to:

```rust
limiter_registry: Arc::new(arc_swap::ArcSwap::from_pointee(
    agent_shim_router::LimiterRegistry::disabled(),
)),
```

If `arc_swap` is not imported at the top of the file, add `use arc_swap;` (or qualify inline as shown).

- [ ] **Step 3: Same change in responses_capability_gate_deepseek.rs**

Same edit.

- [ ] **Step 4: Add arc-swap to gateway dev-deps if not already there**

```bash
grep arc-swap crates/gateway/Cargo.toml
```

Expected: present (`arc-swap.workspace = true` in `[dependencies]` from v0.5). Tests automatically see workspace deps.

- [ ] **Step 5: Build the workspace**

```bash
cargo build --workspace --tests 2>&1 | tail -10
```

Expected: clean.

- [ ] **Step 6: Run the workspace tests**

```bash
cargo test --workspace --quiet 2>&1 | grep -E "^test result:" | grep -v "0 failed" | head -5
echo "---"
echo "Any failures above?"
```

Expected: empty (no failures). The two impacted tests pass with the wrapped registry.

- [ ] **Step 7: Commit Tasks 2 + 3**

```bash
git add crates/gateway/src/state.rs crates/gateway/tests/vision_capability_mismatch.rs crates/gateway/tests/responses_capability_gate_deepseek.rs
git commit -m "refactor(gateway): AppCore::limiter_registry behind ArcSwap (Plan 02 P02 T2-T3)"
```

---

### Task 4: Rebuild + swap the limiter registry on reload

**Files:**
- Modify: `crates/gateway/src/commands/serve.rs`

- [ ] **Step 1: Read the current handle_reload**

The v0.5 `handle_reload` (after P04 followup in commit `449b093`) ends with `state.snapshot.store(new_snap)` in the `Ok(diff)` arm. Phase 6 P02 inserts a second swap immediately after.

- [ ] **Step 2: Add the limiter rebuild + swap**

In `crates/gateway/src/commands/serve.rs`, inside the `Ok(diff) => { ... }` arm of `handle_reload`, after the existing `state.snapshot.store(new_snap);` line and BEFORE the `metrics::counter!(...)` line, insert:

```rust
            // Plan 06 P02 T4: rebuild the rate-limit registry from the
            // candidate config and atomic-swap it. Spec §5.4: governor
            // doesn't expose retuning, so we replace the whole registry
            // — buckets reset with their new burst capacity. The swap
            // happens AFTER the AppSnapshot swap so a request that races
            // the two stores still sees a valid (old or new) limiter
            // and a valid (old or new) snapshot. Documented in spec §6.2.
            let new_limiter = if current.config.rate_limit.enabled
                || state.snapshot.load_full().config.rate_limit.enabled
            {
                // `current` was loaded before the snapshot swap above; load
                // fresh to read the just-committed candidate.
                agent_shim_router::LimiterRegistry::from_config(
                    &state.snapshot.load_full().config.rate_limit,
                )
            } else {
                agent_shim_router::LimiterRegistry::disabled()
            };
            state.core.limiter_registry.store(std::sync::Arc::new(new_limiter));
```

Wait — `current` was loaded BEFORE the snapshot swap. Simpler: re-derive from the just-swapped snapshot directly. Replace the snippet above with the cleaner version:

```rust
            // Plan 06 P02 T4: rebuild the rate-limit registry from the
            // just-committed snapshot. Spec §5.4: governor doesn't
            // expose retuning, so we replace the whole registry —
            // buckets reset with new burst capacity. The swap happens
            // AFTER the AppSnapshot swap (commit point 2 in spec §6.2):
            // a request racing between the two stores sees a valid
            // (old or new) snapshot AND a valid (old or new) limiter,
            // both legal — just from different config versions for a
            // tiny window. Inherent ArcSwap property; documented.
            let committed = state.snapshot.load_full();
            let new_limiter = if committed.config.rate_limit.enabled {
                agent_shim_router::LimiterRegistry::from_config(
                    &committed.config.rate_limit,
                )
            } else {
                agent_shim_router::LimiterRegistry::disabled()
            };
            state.core.limiter_registry.store(std::sync::Arc::new(new_limiter));
```

- [ ] **Step 3: Build**

```bash
cargo build -p agent-shim 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 4: Run gateway tests**

```bash
cargo test -p agent-shim --quiet 2>&1 | grep -E "^test result:" | grep -v "0 failed"
echo "---"
echo "Any failures?"
```

Expected: no failures.

- [ ] **Step 5: Commit**

```bash
git add crates/gateway/src/commands/serve.rs
git commit -m "feat(gateway): rebuild + swap LimiterRegistry on reload (Plan 02 P02 T4)"
```

---

### Task 5: Integration test — rate_limit reload takes effect

**Files:**
- Create: `crates/gateway/tests/reload_rate_limit_buckets_reset.rs`

- [ ] **Step 1: Write the test**

Create `crates/gateway/tests/reload_rate_limit_buckets_reset.rs`:

```rust
//! Plan 02 P02 T5: a reload that changes `rate_limit.*` produces a new
//! `LimiterRegistry` on the very next request.
//!
//! Strategy:
//! 1. Boot the gateway with rate_limit disabled.
//! 2. POST a request — should succeed.
//! 3. POST /admin/reload with rate_limit { enabled: true, per_route { ... burst: 1 } }.
//! 4. POST two more requests in quick succession — the second should be 429.
//!
//! This is the v0.5 §5.4 "deferred" deviation, now fully implemented.

use std::net::SocketAddr;

async fn pick_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
        .local_addr().unwrap().port()
}

fn yaml_for(public: u16, admin: u16, rate_limited: bool) -> String {
    let rl = if rate_limited {
        r#"rate_limit:
  enabled: true
  per_route:
    "openai_chat/x":
      rate_per_sec: 1
      burst: 1
"#
    } else {
        r#"rate_limit:
  enabled: false
"#
    };
    format!(
        r#"
server: {{bind: 127.0.0.1, port: {public}}}
admin: {{bind: 127.0.0.1, port: {admin}}}
{rl}
upstreams:
  m:
    type: open_ai_compatible
    base_url: http://localhost:9999/v1
    api_key: dummy
routes:
  - frontend: openai_chat
    model: x
    upstream: m
    upstream_model: x
"#
    )
}

#[tokio::test]
async fn rate_limit_reload_takes_effect_on_next_request() {
    let public = pick_port().await;
    let admin = pick_port().await;
    let initial_yaml = yaml_for(public, admin, /*rate_limited=*/ false);
    let updated_yaml = yaml_for(public, admin, /*rate_limited=*/ true);

    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(&initial_yaml).unwrap();
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
    let body = r#"{"model":"x","messages":[{"role":"user","content":"hi"}]}"#;

    // (1) Pre-reload: rate-limit disabled — request goes through to the
    //     (unreachable) upstream, returns 5xx but NOT 429.
    let pre = client.post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_ne!(pre.status(), 429, "pre-reload should NOT be 429; rate-limit is off");

    // (2) Reload to enabled with burst=1.
    let reload_resp = client.post(format!("http://{}/admin/reload", admin_addr))
        .header("content-type", "application/yaml")
        .body(updated_yaml.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(reload_resp.status(), 200, "reload must succeed");

    // (3) First post-reload request: should consume the single token.
    let first = client.post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    // Not 429 (token was available); could be 5xx from unreachable upstream.
    assert_ne!(first.status(), 429, "first post-reload request consumes the token; got {}", first.status());

    // (4) Second post-reload request, immediate: must be 429 because the
    //     new bucket has burst=1 and rate=1/sec. Without P02 this would
    //     hit the OLD (disabled) registry and pass — proving the swap.
    let second = client.post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 429,
        "second post-reload request must be 429 (new bucket exhausted); got {}", second.status());
}
```

- [ ] **Step 2: Run the test**

```bash
cargo test -p agent-shim --test reload_rate_limit_buckets_reset 2>&1 | tail -10
```

Expected: 1 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/gateway/tests/reload_rate_limit_buckets_reset.rs
git commit -m "test(gateway): reload of rate_limit takes effect immediately (Plan 02 P02 T5)"
```

---

### Task 6: Spec compliance + code quality review

- [ ] **Step 1: Reviewer dispatch (spec compliance)**

> Review commits Plan 02 T1..T5 against spec `docs/superpowers/specs/2026-05-12-phase-6-cost-aware-gateway-design.md`. Verify:
>
> 1. **§1 pillar 2** — `LimiterRegistry` is now hot-swappable. The reload-applying task atomically replaces it.
> 2. **§5 known v0.5 deferral closed** — `reload_rate_limit_buckets_reset.rs` actually proves the bucket reset. The pre-reload request is non-429; the second post-reload request is 429. Confirm both assertions are present and the test passes.
> 3. **§6.2 atomicity** — handle_reload swaps AppSnapshot first, then LimiterRegistry second. Both swaps are inside the `Ok(diff)` arm; an early-return `Err` doesn't touch either. Confirm by reading `commands/serve.rs::handle_reload`.
> 4. **§2.1 frozen-core** — `git diff a0139fc -- crates/core/ crates/frontends/` is empty.
> 5. **§D4 chosen** — registry is replaced atomically (not per-bucket replace_policy). Confirm by reading the snippet in `serve.rs`.

- [ ] **Step 2: Reviewer dispatch (code quality)**

> Review commits Plan 02 T1..T5 for code quality.
>
> 1. `ResilientCaller::limiters` now requires `load_full()` on every read. Each call allocates an `Arc` clone — is this overhead measurable? Compare to v0.5's per-request RateLimitDecision lookups which already happen.
> 2. The two-step swap in `handle_reload` (snapshot store → limiter store) has a tiny window between commits. Spec §6.2 documents this as benign. Are both stores wrapped in any structured-error path? What happens if `LimiterRegistry::from_config` panics (it shouldn't)?
> 3. Test files using `ArcSwap::from_pointee` — is the cargo lint clean? Compare to the AppSnapshot ArcSwap construction in `state.rs` for consistency.
> 4. `reload_rate_limit_buckets_reset.rs` uses `tokio::time::sleep(100ms)` — any chance of flakiness on slow CI? Compare to neighbouring reload tests.

- [ ] **Step 3: Apply CRITICAL/HIGH findings**

```bash
git commit -m "fix(router): address P02 review findings"
```

---

## Done when

- [ ] Workspace test count ≥ 644.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] `git diff a0139fc -- crates/core/ crates/frontends/` empty.
- [ ] `reload_rate_limit_buckets_reset.rs` passes — proves rate-limit reload takes effect on the very next request.
- [ ] `handle_reload`'s `Ok(diff)` arm performs both `state.snapshot.store(...)` AND `state.core.limiter_registry.store(...)`.
- [ ] The two AppCore-direct-construction tests (`vision_capability_mismatch.rs`, `responses_capability_gate_deepseek.rs`) updated to wrap `LimiterRegistry` in `ArcSwap`.
- [ ] T6 reviews clear of CRITICAL/HIGH findings.
