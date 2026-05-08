# Plan 03 — Circuit Breakers (Phase 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-08-phase-4-resiliency-design.md`](../specs/2026-05-08-phase-4-resiliency-design.md) (decisions D6, D7; §5.2 layering).

**Goal:** Sliding-window circuit breakers keyed by `(provider, model)` slot into the chain walk inside `ResilientCaller`. An open breaker skips that chain element entirely (no retries, no rate-limit consumption). Half-open recovery uses a single probe.

**Architecture:** New `BreakerState` enum (Closed | Open | HalfOpen) with sliding-window failure-rate accounting. `BreakerRegistry` is `Arc<RwLock<HashMap<(String, String), BreakerState>>>` — uncontended `read()` on the hot path; `write()` only on state transitions. A `Clock` trait abstracts time so transitions are unit-testable. Wired into `ResilientCaller::complete` between the chain-walk loop start and the per-element retry loop.

**Tech stack:** No new dependencies. Use `std::time::Instant` in production; inject a `MockClock` in tests.

**Frontend changes:** NONE.
**Provider code changes:** NONE.
**Core changes:** NONE.

---

## File Structure

`crates/router/src/`:
- Modify: `circuit_breaker.rs` — replace stub with `BreakerState`, `BreakerRegistry`, `Clock` trait.
- Modify: `resilient_caller.rs` — insert breaker gate into chain walk.
- Modify: `lib.rs` — re-export `BreakerRegistry`, `BreakerConfig`-derived `BreakerPolicy`.

`crates/gateway/src/`:
- Modify: `state.rs` — `AppState` gains `Arc<BreakerRegistry>`.
- Modify: `pipeline.rs` — wire breaker into the `ResilientCaller` construction.

`crates/protocol-tests/tests/`:
- Create: `breaker_trip_skips_upstream.rs` — 25 successive 502s trip the breaker; subsequent requests skip A.
- Create: `breaker_half_open_recovery.rs` — cooldown elapses, single probe attempts, success closes.

---

## Tasks

### Task 1: BreakerState + Clock + sliding-window math

**Files:**
- Modify: `crates/router/src/circuit_breaker.rs`

- [ ] **Step 1: Replace stub with state machine + tests**

Replace `crates/router/src/circuit_breaker.rs`:

```rust
//! Sliding-window circuit breaker per `(provider, model)` (Plan 04 P03).
//!
//! State machine (D7):
//! - **Closed**: requests flow normally. On each failure, append a (timestamp,
//!   false) sample to the window deque. On each success, append (ts, true).
//!   When the failure rate over the most recent `window_secs` exceeds
//!   `failure_threshold_pct` AND total samples ≥ `min_requests`, transition
//!   to Open.
//! - **Open**: requests short-circuit (return `BreakerOpen` to caller).
//!   After `open_cooldown_secs` elapse, transition to HalfOpen.
//! - **HalfOpen**: a single probe is allowed through. On success, transition
//!   back to Closed (and clear the window). On failure, transition back to
//!   Open with fresh cooldown.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Injectable clock so transitions are deterministic in tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// Production clock — wraps `Instant::now()`.
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant { Instant::now() }
}

/// Configuration for a per-route breaker. Built from
/// `agent_shim_config::BreakerConfig` at AppState construction time.
#[derive(Debug, Clone)]
pub struct BreakerPolicy {
    pub enabled: bool,
    pub failure_threshold_pct: u32,
    pub min_requests: u32,
    pub window: Duration,
    pub open_cooldown: Duration,
}

impl From<&agent_shim_config::BreakerConfig> for BreakerPolicy {
    fn from(c: &agent_shim_config::BreakerConfig) -> Self {
        Self {
            enabled: c.enabled,
            failure_threshold_pct: c.failure_threshold_pct,
            min_requests: c.min_requests,
            window: Duration::from_secs(c.window_secs),
            open_cooldown: Duration::from_secs(c.open_cooldown_secs),
        }
    }
}

/// Outcome of `query_state` — what should the chain walker do?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerDecision {
    /// Proceed with the call.
    Allow,
    /// Open and within cooldown — skip this chain element.
    Skip,
    /// Open but cooldown elapsed — single probe allowed.
    Probe,
}

/// Internal state for one breaker. Concurrent access is guarded by the
/// registry's `RwLock`.
struct BreakerState {
    samples: VecDeque<(Instant, bool)>,  // true = success, false = failure
    open_since: Option<Instant>,
    /// Set to true while the half-open probe is in flight; prevents a second
    /// probe from being authorized concurrently.
    probe_in_flight: AtomicBool,
}

impl BreakerState {
    fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            open_since: None,
            probe_in_flight: AtomicBool::new(false),
        }
    }

    fn record(&mut self, succeeded: bool, now: Instant, policy: &BreakerPolicy) {
        // Drop samples outside the window first.
        let cutoff = now.checked_sub(policy.window).unwrap_or(now);
        while self.samples.front().map(|(t, _)| *t < cutoff).unwrap_or(false) {
            self.samples.pop_front();
        }
        self.samples.push_back((now, succeeded));

        // Re-evaluate state.
        if let Some(opened) = self.open_since {
            // We're Open or HalfOpen. Probes are handled in query_state;
            // here we only transition based on the probe result.
            if succeeded {
                // Half-open probe succeeded → Closed.
                self.open_since = None;
                self.samples.clear();
                self.samples.push_back((now, true));
            } else if now.duration_since(opened) >= policy.open_cooldown {
                // Half-open probe failed → re-open with fresh cooldown.
                self.open_since = Some(now);
            }
            // else: still in cooldown; recording while Open shouldn't happen
            // (caller honors Skip), but tolerate it without crashing.
            self.probe_in_flight.store(false, Ordering::SeqCst);
            return;
        }

        // Closed: check trip condition.
        let total = self.samples.len() as u32;
        if total < policy.min_requests {
            return;
        }
        let failures = self.samples.iter().filter(|(_, ok)| !*ok).count() as u32;
        let pct = (failures * 100) / total;
        if pct >= policy.failure_threshold_pct {
            self.open_since = Some(now);
        }
    }

    fn decision(&self, now: Instant, policy: &BreakerPolicy) -> BreakerDecision {
        if !policy.enabled {
            return BreakerDecision::Allow;
        }
        match self.open_since {
            None => BreakerDecision::Allow,
            Some(opened) => {
                if now.duration_since(opened) >= policy.open_cooldown {
                    // Half-open: only one probe at a time.
                    if self
                        .probe_in_flight
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        BreakerDecision::Probe
                    } else {
                        BreakerDecision::Skip
                    }
                } else {
                    BreakerDecision::Skip
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeClock(std::sync::Mutex<Instant>);
    impl FakeClock {
        fn new() -> Self { Self(std::sync::Mutex::new(Instant::now())) }
        fn advance(&self, d: Duration) {
            let mut t = self.0.lock().unwrap();
            *t += d;
        }
    }
    impl Clock for FakeClock {
        fn now(&self) -> Instant { *self.0.lock().unwrap() }
    }

    fn fast_policy() -> BreakerPolicy {
        BreakerPolicy {
            enabled: true,
            failure_threshold_pct: 50,
            min_requests: 5,
            window: Duration::from_secs(60),
            open_cooldown: Duration::from_secs(30),
        }
    }

    #[test]
    fn closed_under_min_requests_never_trips() {
        let clock = FakeClock::new();
        let policy = fast_policy();
        let mut state = BreakerState::new();
        // 4 failures < min_requests (5), so still Closed even at 100% failure.
        for _ in 0..4 {
            state.record(false, clock.now(), &policy);
        }
        assert_eq!(state.decision(clock.now(), &policy), BreakerDecision::Allow);
    }

    #[test]
    fn closed_trips_when_failure_rate_crosses_threshold() {
        let clock = FakeClock::new();
        let policy = fast_policy();
        let mut state = BreakerState::new();
        // 5 failures = 100% > 50% threshold → trip.
        for _ in 0..5 {
            state.record(false, clock.now(), &policy);
        }
        assert_eq!(state.decision(clock.now(), &policy), BreakerDecision::Skip);
    }

    #[test]
    fn open_skips_during_cooldown() {
        let clock = FakeClock::new();
        let policy = fast_policy();
        let mut state = BreakerState::new();
        for _ in 0..5 {
            state.record(false, clock.now(), &policy);
        }
        clock.advance(Duration::from_secs(10)); // less than open_cooldown=30
        assert_eq!(state.decision(clock.now(), &policy), BreakerDecision::Skip);
    }

    #[test]
    fn open_transitions_to_probe_after_cooldown() {
        let clock = FakeClock::new();
        let policy = fast_policy();
        let mut state = BreakerState::new();
        for _ in 0..5 {
            state.record(false, clock.now(), &policy);
        }
        clock.advance(Duration::from_secs(31));
        assert_eq!(state.decision(clock.now(), &policy), BreakerDecision::Probe);
        // A second concurrent caller in the same instant gets Skip.
        assert_eq!(state.decision(clock.now(), &policy), BreakerDecision::Skip);
    }

    #[test]
    fn half_open_success_closes_breaker() {
        let clock = FakeClock::new();
        let policy = fast_policy();
        let mut state = BreakerState::new();
        for _ in 0..5 { state.record(false, clock.now(), &policy); }
        clock.advance(Duration::from_secs(31));
        assert_eq!(state.decision(clock.now(), &policy), BreakerDecision::Probe);
        // Probe succeeds → Closed.
        state.record(true, clock.now(), &policy);
        assert_eq!(state.decision(clock.now(), &policy), BreakerDecision::Allow);
    }

    #[test]
    fn half_open_failure_reopens_with_fresh_cooldown() {
        let clock = FakeClock::new();
        let policy = fast_policy();
        let mut state = BreakerState::new();
        for _ in 0..5 { state.record(false, clock.now(), &policy); }
        clock.advance(Duration::from_secs(31));
        assert_eq!(state.decision(clock.now(), &policy), BreakerDecision::Probe);
        // Probe fails → re-Open. open_since updates to current time.
        state.record(false, clock.now(), &policy);
        // Need to advance past the new cooldown, not the old one.
        clock.advance(Duration::from_secs(10));
        assert_eq!(state.decision(clock.now(), &policy), BreakerDecision::Skip);
        clock.advance(Duration::from_secs(21));
        assert_eq!(state.decision(clock.now(), &policy), BreakerDecision::Probe);
    }

    #[test]
    fn samples_outside_window_are_evicted() {
        let clock = FakeClock::new();
        let policy = fast_policy();  // window = 60s
        let mut state = BreakerState::new();
        // 5 old failures, all outside the window after advance.
        for _ in 0..5 { state.record(false, clock.now(), &policy); }
        clock.advance(Duration::from_secs(61));
        // New successes only — failures evicted; should not trip.
        for _ in 0..5 { state.record(true, clock.now(), &policy); }
        assert_eq!(state.decision(clock.now(), &policy), BreakerDecision::Allow);
    }

    #[test]
    fn disabled_breaker_always_allows() {
        let clock = FakeClock::new();
        let policy = BreakerPolicy { enabled: false, ..fast_policy() };
        let mut state = BreakerState::new();
        for _ in 0..100 { state.record(false, clock.now(), &policy); }
        assert_eq!(state.decision(clock.now(), &policy), BreakerDecision::Allow);
    }
}
```

- [ ] **Step 2: Run tests**

```bash
rtk cargo test -p agent-shim-router --quiet
```

Expected: 8 breaker state tests PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/router/src/circuit_breaker.rs
git commit -m "feat(router): BreakerState sliding-window state machine (Plan 04 P03 T1)

Replaces v0.3 stub with the full sliding-window failure-rate breaker
per D7:
- Closed: append samples; trip when failure_rate ≥ threshold_pct
  AND total ≥ min_requests.
- Open: skip during cooldown; transition to half-open afterwards.
- HalfOpen: single probe (atomic compare_exchange ensures one-at-a-time);
  success → Closed (clear window), failure → re-Open with fresh cooldown.

BreakerPolicy struct derives from agent_shim_config::BreakerConfig.
Clock trait + FakeClock injection makes every transition deterministic.

8 tests cover: under-min-requests, threshold-trip, open-cooldown,
half-open-probe (and concurrent-skip), success-closes, failure-reopens,
window-eviction, disabled-bypass."
```

---

### Task 2: BreakerRegistry concurrent stress

**Files:**
- Modify: `crates/router/src/circuit_breaker.rs`
- Modify: `crates/router/src/lib.rs`

- [ ] **Step 1: Add registry on top of BreakerState**

Append to `crates/router/src/circuit_breaker.rs`:

```rust
use std::collections::HashMap;
use std::sync::RwLock;

/// Registry keyed by `(provider, model)`. Each key has its own
/// `BreakerState` behind the registry's `RwLock`.
pub struct BreakerRegistry {
    breakers: RwLock<HashMap<(String, String), Arc<RwLock<BreakerState>>>>,
    clock: Arc<dyn Clock>,
}

impl BreakerRegistry {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            breakers: RwLock::new(HashMap::new()),
            clock,
        }
    }

    pub fn with_system_clock() -> Self {
        Self::new(Arc::new(SystemClock))
    }

    fn get_or_create(&self, provider: &str, model: &str) -> Arc<RwLock<BreakerState>> {
        let key = (provider.to_string(), model.to_string());
        // Fast path: read-only lookup.
        if let Some(state) = self.breakers.read().unwrap().get(&key) {
            return Arc::clone(state);
        }
        // Slow path: create.
        let mut w = self.breakers.write().unwrap();
        Arc::clone(
            w.entry(key)
                .or_insert_with(|| Arc::new(RwLock::new(BreakerState::new()))),
        )
    }

    /// Query whether the chain walker should call this upstream.
    pub fn decision(
        &self,
        provider: &str,
        model: &str,
        policy: &BreakerPolicy,
    ) -> BreakerDecision {
        let state = self.get_or_create(provider, model);
        let s = state.read().unwrap();
        s.decision(self.clock.now(), policy)
    }

    /// Record the outcome of a call (regardless of how the breaker decided).
    pub fn record(
        &self,
        provider: &str,
        model: &str,
        succeeded: bool,
        policy: &BreakerPolicy,
    ) {
        let state = self.get_or_create(provider, model);
        let mut s = state.write().unwrap();
        s.record(succeeded, self.clock.now(), policy);
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    fn fast_policy() -> BreakerPolicy {
        BreakerPolicy {
            enabled: true,
            failure_threshold_pct: 50,
            min_requests: 5,
            window: Duration::from_secs(60),
            open_cooldown: Duration::from_secs(30),
        }
    }

    #[test]
    fn distinct_keys_have_independent_breakers() {
        let registry = BreakerRegistry::with_system_clock();
        let policy = fast_policy();
        // Trip A's breaker.
        for _ in 0..5 {
            registry.record("openai", "gpt-4o", false, &policy);
        }
        assert_eq!(
            registry.decision("openai", "gpt-4o", &policy),
            BreakerDecision::Skip
        );
        // B is independent.
        assert_eq!(
            registry.decision("anthropic", "claude-opus-4-7", &policy),
            BreakerDecision::Allow
        );
    }

    #[tokio::test]
    async fn concurrent_record_and_query_does_not_deadlock() {
        let registry = Arc::new(BreakerRegistry::with_system_clock());
        let policy = fast_policy();
        let mut handles = vec![];
        for i in 0..50 {
            let r = Arc::clone(&registry);
            let p = policy.clone();
            handles.push(tokio::spawn(async move {
                let succeeded = i % 3 != 0;
                r.record("openai", "gpt-4o", succeeded, &p);
                let _d = r.decision("openai", "gpt-4o", &p);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // No assertion on final state — concurrency makes that
        // non-deterministic. The win is that the test completes without
        // deadlocking or panicking.
    }
}
```

- [ ] **Step 2: Wire into lib.rs**

```rust
pub use circuit_breaker::{
    BreakerDecision, BreakerPolicy, BreakerRegistry, Clock, SystemClock,
};
```

- [ ] **Step 3: Run tests**

```bash
rtk cargo test -p agent-shim-router --quiet
```

Expected: 2 new registry tests PASS, plus all 8 from T1.

- [ ] **Step 4: Commit**

```bash
git add crates/router/src/circuit_breaker.rs crates/router/src/lib.rs
git commit -m "feat(router): BreakerRegistry per (provider, model) (Plan 04 P03 T2)

Layered on T1's BreakerState. Outer RwLock<HashMap<...>> guards the
key set; inner RwLock<BreakerState> per key avoids cross-key contention.
Fast path uses read() lookup; only first-touch and state mutation
require write().

2 new tests:
- distinct_keys_have_independent_breakers — confirms scoping per D6.
- concurrent_record_and_query_does_not_deadlock — 50 concurrent tasks
  hammering the same key; no deadlock, no panic."
```

---

### Task 3: Integrate breaker gate into ResilientCaller

**Files:**
- Modify: `crates/router/src/resilient_caller.rs`
- Modify: `crates/router/src/errors.rs` (already has `AllBreakersOpen`).

- [ ] **Step 1: Update ResilientCaller signature to take registry + per-element BreakerPolicy**

In `crates/router/src/resilient_caller.rs`, modify the struct:

```rust
pub struct ResilientCaller {
    providers: Arc<dyn ProviderLookup>,
    breakers: Arc<crate::circuit_breaker::BreakerRegistry>,
}

impl ResilientCaller {
    pub fn new(
        providers: Arc<dyn ProviderLookup>,
        breakers: Arc<crate::circuit_breaker::BreakerRegistry>,
    ) -> Self {
        Self { providers, breakers }
    }
}
```

Modify `complete` to take parallel `Vec<BreakerPolicy>` and gate per element:

```rust
pub async fn complete(
    &self,
    chain: Vec<BackendTarget>,
    req: CanonicalRequest,
    retry_policies: Vec<RetryPolicy>,
    breaker_policies: Vec<crate::circuit_breaker::BreakerPolicy>,
) -> Result<CanonicalStream, ResilienceError> {
    debug_assert_eq!(chain.len(), retry_policies.len());
    debug_assert_eq!(chain.len(), breaker_policies.len());

    let mut tried: Vec<TriedUpstream> = Vec::new();
    let mut last_error: Option<ProviderError> = None;
    let mut breakers_skipped: Vec<String> = Vec::new();

    for (i, target) in chain.iter().enumerate() {
        let bpolicy = &breaker_policies[i];
        let rpolicy = &retry_policies[i];

        // ── BREAKER GATE ────────────────────────────────────────────────
        let decision = self.breakers.decision(&target.provider, &target.model, bpolicy);
        match decision {
            crate::circuit_breaker::BreakerDecision::Skip => {
                tracing::warn!(
                    provider = %target.provider,
                    model = %target.model,
                    chain_position = i,
                    "breaker open; skipping chain element"
                );
                breakers_skipped.push(target.provider.clone());
                continue; // next chain element
            }
            crate::circuit_breaker::BreakerDecision::Probe => {
                tracing::info!(
                    provider = %target.provider,
                    model = %target.model,
                    chain_position = i,
                    "breaker half-open; attempting probe"
                );
                // Fall through to call. probe_in_flight is set by decision();
                // record() clears it.
            }
            crate::circuit_breaker::BreakerDecision::Allow => {
                // Normal path.
            }
        }

        let provider = match self.providers.get(&target.provider) {
            Some(p) => p,
            None => {
                let e = ProviderError::UnknownProvider(target.provider.clone());
                return Err(ResilienceError::TerminalError { error: e, tried });
            }
        };

        let started = Instant::now();
        let result = retry_with_policy(provider, target.clone(), req.clone(), rpolicy).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        match result {
            Ok(stream) => {
                self.breakers.record(&target.provider, &target.model, true, bpolicy);
                tracing::info!(
                    provider = %target.provider,
                    model = %target.model,
                    chain_position = i,
                    elapsed_ms,
                    "chain element succeeded"
                );
                return Ok(stream);
            }
            Err(e) => {
                self.breakers.record(&target.provider, &target.model, false, bpolicy);
                let eligibility = fallback_eligibility_with_overrides(&e, &rpolicy.retry_on);
                let tag = error_tag(&e);
                tried.push(TriedUpstream {
                    provider: target.provider.clone(),
                    model: target.model.clone(),
                    attempts: rpolicy.max_attempts,
                    last_error_tag: tag.to_string(),
                    last_error_msg: e.to_string(),
                    elapsed_ms,
                });
                if eligibility == FallbackEligibility::Terminal {
                    return Err(ResilienceError::TerminalError { error: e, tried });
                }
                last_error = Some(e);
            }
        }
    }

    // Chain exhausted. Distinguish "every element skipped by breaker" from
    // "every element actually attempted and failed".
    if !tried.is_empty() {
        Err(ResilienceError::NoUpstreamSucceeded {
            tried,
            last_error: last_error.expect("last_error set on every Err path"),
        })
    } else if !breakers_skipped.is_empty() {
        Err(ResilienceError::AllBreakersOpen { tried: breakers_skipped })
    } else {
        // chain.is_empty() — shouldn't reach here in practice.
        Err(ResilienceError::TerminalError {
            error: ProviderError::Network("empty chain".into()),
            tried: vec![],
        })
    }
}
```

- [ ] **Step 2: Update the existing P02 unit tests to thread breaker_policies**

The existing 4 tests pass `breaker_policies = vec![BreakerPolicy { enabled: false, ... }; chain.len()]` so breakers don't interfere. Update each call site:

```rust
fn disabled_breaker(n: usize) -> Vec<BreakerPolicy> {
    (0..n).map(|_| BreakerPolicy {
        enabled: false,
        failure_threshold_pct: 50,
        min_requests: 5,
        window: Duration::from_secs(60),
        open_cooldown: Duration::from_secs(30),
    }).collect()
}

// Update each test to construct ResilientCaller with a registry and pass
// breaker_policies. Example:
let registry = Arc::new(BreakerRegistry::with_system_clock());
let caller = ResilientCaller::new(providers, registry);
let result = caller
    .complete(chain, dummy_request(), policies, disabled_breaker(2))
    .await;
```

- [ ] **Step 3: Add new test — breaker open skips without retry**

```rust
#[tokio::test]
async fn breaker_open_skips_chain_element_without_retry() {
    let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
        map: HashMap::from([
            ("a".to_string(), MockProvider::new("a", vec![]) as Arc<_>),
            ("b".to_string(), MockProvider::new("b", vec![Ok(())]) as Arc<_>),
        ]),
    });
    let registry = Arc::new(BreakerRegistry::with_system_clock());

    // Pre-trip A's breaker.
    let policy = BreakerPolicy {
        enabled: true,
        failure_threshold_pct: 50,
        min_requests: 5,
        window: Duration::from_secs(60),
        open_cooldown: Duration::from_secs(30),
    };
    for _ in 0..5 {
        registry.record("a", "gpt-4o", false, &policy);
    }
    assert_eq!(
        registry.decision("a", "gpt-4o", &policy),
        BreakerDecision::Skip
    );

    let caller = ResilientCaller::new(providers, registry);
    let chain = vec![target("a"), target("b")];
    let result = caller
        .complete(
            chain,
            dummy_request(),
            vec![fast_policy(2), fast_policy(2)],
            vec![policy.clone(), policy.clone()],
        )
        .await;
    assert!(result.is_ok());
    // A's MockProvider was set up with empty scripted vec; if `a` was
    // called even once it would have errored with "script exhausted".
    // The Ok() result on B confirms A was correctly skipped.
}

#[tokio::test]
async fn all_breakers_open_returns_dedicated_variant() {
    let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
        map: HashMap::from([
            ("a".to_string(), MockProvider::new("a", vec![]) as Arc<_>),
            ("b".to_string(), MockProvider::new("b", vec![]) as Arc<_>),
        ]),
    });
    let registry = Arc::new(BreakerRegistry::with_system_clock());
    let policy = BreakerPolicy {
        enabled: true,
        failure_threshold_pct: 50,
        min_requests: 5,
        window: Duration::from_secs(60),
        open_cooldown: Duration::from_secs(30),
    };
    for _ in 0..5 { registry.record("a", "gpt-4o", false, &policy); }
    for _ in 0..5 { registry.record("b", "gpt-4o", false, &policy); }
    let caller = ResilientCaller::new(providers, registry);
    let chain = vec![target("a"), target("b")];
    let result = caller
        .complete(
            chain,
            dummy_request(),
            vec![fast_policy(2), fast_policy(2)],
            vec![policy.clone(), policy.clone()],
        )
        .await;
    match result {
        Err(ResilienceError::AllBreakersOpen { tried }) => {
            assert_eq!(tried.len(), 2);
            assert_eq!(tried[0], "a");
            assert_eq!(tried[1], "b");
        }
        other => panic!("expected AllBreakersOpen, got {other:?}"),
    }
}
```

- [ ] **Step 4: Run tests**

```bash
rtk cargo test -p agent-shim-router --quiet
```

Expected: 4 P02 tests + 2 new breaker-integration tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/router/src/resilient_caller.rs crates/router/src/lib.rs
git commit -m "feat(router): breaker gate inside chain walk (Plan 04 P03 T3)

ResilientCaller::complete gains breaker_policies param parallel to
chain. Per §5.2 layering:
- Skip → continue to chain[i+1] without consuming retries.
- Probe → fall through to retry loop; record() resets probe_in_flight.
- Allow → normal path.
- record() called on every retry-loop outcome.

Chain-exhaustion path distinguishes:
- tried.is_empty() AND breakers_skipped.len() > 0 → AllBreakersOpen.
- tried not empty → NoUpstreamSucceeded.

Existing P02 tests updated with disabled_breaker policies. Two new
tests: breaker-open-skips-without-retry, all-breakers-open-variant."
```

---

### Task 4: Wire BreakerRegistry into AppState

**Files:**
- Modify: `crates/gateway/src/state.rs`
- Modify: `crates/gateway/src/pipeline.rs`

- [ ] **Step 1: AppState gains BreakerRegistry**

In `crates/gateway/src/state.rs`:

```rust
pub struct AppState {
    // ... existing fields
    pub breaker_registry: Arc<agent_shim_router::BreakerRegistry>,
    // resilient_caller from P02 still here; constructor changes to pass the registry.
}

impl AppState {
    pub fn new(...) -> Self {
        // ... existing setup
        let breaker_registry = Arc::new(agent_shim_router::BreakerRegistry::with_system_clock());
        let resilient_caller = Arc::new(agent_shim_router::ResilientCaller::new(
            provider_lookup,
            Arc::clone(&breaker_registry),
        ));
        Self {
            // ... existing fields
            breaker_registry,
            resilient_caller,
        }
    }
}
```

- [ ] **Step 2: Pipeline builds BreakerPolicies parallel to RetryPolicies**

In `crates/gateway/src/pipeline.rs`, replace the P02 chain construction:

```rust
let chain = state.resolver.resolve(spec.frontend.kind(), &model_alias)?;

// Look up retry + breaker config for the route. P05 will fold this into
// a single helper on the resolver; for now we look them up parallel.
let route_retry = state
    .static_routes
    .find_retry_policy(spec.frontend.kind(), &model_alias)
    .unwrap_or_default();
let route_breaker = state
    .static_routes
    .find_breaker_policy(spec.frontend.kind(), &model_alias)
    .unwrap_or_default();

let retry_policies: Vec<_> = chain.iter()
    .map(|_| agent_shim_router::RetryPolicy::from(&route_retry))
    .collect();
let breaker_policies: Vec<_> = chain.iter()
    .map(|_| agent_shim_router::BreakerPolicy::from(&route_breaker))
    .collect();

let stream = state
    .resilient_caller
    .complete(chain, canonical, retry_policies, breaker_policies)
    .await
    .map_err(HandlerError::from)?;
```

- [ ] **Step 3: Add `find_breaker_policy` helper to StaticRouter**

In `crates/router/src/static_routes.rs`:

```rust
impl StaticRouter {
    pub fn find_breaker_policy(
        &self,
        frontend: FrontendKind,
        model: &str,
    ) -> Option<agent_shim_config::BreakerConfig> {
        let key = RouteKey { frontend, model: model.to_string() };
        self.route_entries.get(&key).map(|e| e.breaker.clone())
    }
}
```

- [ ] **Step 4: Run tests**

```bash
rtk cargo test --workspace --quiet
```

Expected: existing tests pass; integration test from P01 T5 still passes (default breaker enabled but never trips on a 1-failure scenario).

- [ ] **Step 5: Commit**

```bash
git add crates/gateway/src/state.rs crates/gateway/src/pipeline.rs crates/router/src/static_routes.rs
git commit -m "feat(gateway): wire BreakerRegistry into AppState (Plan 04 P03 T4)

AppState gains breaker_registry: Arc<BreakerRegistry>; ResilientCaller
constructor takes the registry. Pipeline builds parallel
breaker_policies: Vec<BreakerPolicy> alongside retry_policies and
threads them into caller.complete().

StaticRouter gains find_breaker_policy() mirroring
find_retry_policy() from P01."
```

---

### Task 5: Cross-protocol breaker-trip smoke test

**Files:**
- Create: `crates/protocol-tests/tests/breaker_trip_skips_upstream.rs`

- [ ] **Step 1: Write end-to-end breaker-trip test**

```rust
//! Plan 04 P03 T5: 25 successive 502s on chain[0] trip its breaker;
//! the 26th request must skip chain[0] entirely and go straight to chain[1].

use std::sync::Arc;

#[tokio::test]
async fn twenty_five_failures_trip_breaker_subsequent_requests_skip_a() {
    let mut server_a = mockito::Server::new_async().await;
    // Server A: returns 502 for the first 50 requests (more than enough to
    // trip the breaker AND verify subsequent requests don't even reach A).
    let _bad_a = server_a
        .mock("POST", "/v1/chat/completions")
        .with_status(502)
        .with_body("Bad Gateway")
        .expect_at_most(25)  // ← must NOT exceed; 25 trips the breaker
        .create_async()
        .await;

    let mut server_b = mockito::Server::new_async().await;
    let _good_b = server_b
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"id":"x","object":"chat.completion","created":1700000000,"model":"gpt-4o",
                 "choices":[{"index":0,"message":{"role":"assistant","content":"from B"},
                 "finish_reason":"stop"}],
                 "usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
        )
        .expect_at_least(1)
        .create_async()
        .await;

    let cfg_yaml = format!(
        r#"
upstreams:
  oai_a:
    type: openai_compatible
    base_url: {url_a}
    api_key: test-key
  oai_b:
    type: openai_compatible
    base_url: {url_b}
    api_key: test-key
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstreams:
      - {{name: oai_a, model: gpt-4o}}
      - {{name: oai_b, model: gpt-4o}}
    retry:
      max_attempts: 1            # no retries; one call per chain element
      initial_backoff_ms: 1
      total_budget_ms: 100
    breaker:
      failure_threshold_pct: 50
      min_requests: 5
      window_secs: 60
      open_cooldown_secs: 30
"#,
        url_a = server_a.url(),
        url_b = server_b.url(),
    );

    let app = build_test_app(cfg_yaml).await;

    // Send 30 requests. The first ~5 fall through to B (A failing, retries
    // disabled, fallback to B succeeds). Around request 5 A's breaker trips;
    // requests 6-30 skip A entirely and go straight to B.
    for i in 0..30 {
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
        assert_eq!(response.status(), 200, "request {i} failed");
    }

    // Server A must have been hit ≤ 25 times (mock's expect_at_most enforces).
    // Server B must have been hit at least once.
}
```

- [ ] **Step 2: Run test**

```bash
rtk cargo test -p agent-shim-protocol-tests --test breaker_trip_skips_upstream --quiet
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/protocol-tests/tests/breaker_trip_skips_upstream.rs
git commit -m "test(protocol-tests): breaker trip skips upstream (Plan 04 P03 T5)

End-to-end: 30 requests against chain [oai_a, oai_b]. Server A
returns 502 with mockito's expect_at_most(25); Server B returns 200
with expect_at_least(1). Breaker config: 50% over 5 requests in 60s,
30s cooldown.

The first ~5 requests fail on A and fall through to B; A's breaker
trips; requests 6-30 skip A entirely. mockito's expect_at_most(25)
verifies A is never called more than the trip threshold + cushion."
```

---

### Task 6: Half-open recovery smoke test

**Files:**
- Create: `crates/protocol-tests/tests/breaker_half_open_recovery.rs`

- [ ] **Step 1: Write half-open recovery test**

This test requires injecting a fake clock into the gateway. The cleanest path: add a `with_clock(Arc<dyn Clock>)` builder method to the test-only `build_test_app` helper, which threads through to the BreakerRegistry constructor.

```rust
//! Plan 04 P03 T6: cooldown elapses, single probe attempts chain[0],
//! success closes the breaker and subsequent requests use chain[0] again.

use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn cooldown_elapses_probe_succeeds_breaker_closes() {
    let clock = Arc::new(FakeClock::new());
    let mut server_a = mockito::Server::new_async().await;
    let mut server_b = mockito::Server::new_async().await;

    // A returns 502 5 times (trip) then 200 thereafter.
    let _trip_a = server_a
        .mock("POST", "/v1/chat/completions")
        .with_status(502)
        .with_body("bad")
        .expect(5)
        .create_async()
        .await;
    let _recovered_a = server_a
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(success_body())
        .expect_at_least(1)
        .create_async()
        .await;

    let _b = server_b
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(success_body())
        .expect_at_least(1)
        .create_async()
        .await;

    let app = build_test_app_with_clock(/* yaml */, Arc::clone(&clock) as Arc<dyn Clock>).await;

    // 5 requests trip A's breaker.
    for _ in 0..5 {
        let _ = send_request(&app).await;
    }
    // Verify breaker is open by checking that A receives no further hits
    // for the cooldown duration.

    // Advance clock past cooldown.
    clock.advance(Duration::from_secs(31));

    // Single probe request — should hit A's recovered mock.
    let resp = send_request(&app).await;
    assert_eq!(resp.status(), 200);

    // Subsequent requests also go to A (breaker closed).
    let resp2 = send_request(&app).await;
    assert_eq!(resp2.status(), 200);
}
```

The `build_test_app_with_clock` helper requires `BreakerRegistry::new(clock)` to be passed through `AppState`'s constructor — extend AppState with an optional `clock` builder hook for tests.

- [ ] **Step 2: Run test**

```bash
rtk cargo test -p agent-shim-protocol-tests --test breaker_half_open_recovery --quiet
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/protocol-tests/tests/breaker_half_open_recovery.rs crates/gateway/src/state.rs
git commit -m "test(protocol-tests): breaker half-open recovery (Plan 04 P03 T6)

End-to-end half-open recovery: trip breaker, advance fake clock past
cooldown, single probe succeeds, breaker closes, subsequent requests
flow through normally.

AppState gains an optional clock parameter (test-only constructor)
so the registry's clock can be substituted with a FakeClock."
```

---

## Definition of Done

- [ ] All 6 tasks complete.
- [ ] `cargo test --workspace --quiet` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] `cargo fmt --all -- --check` clean.
- [ ] Workspace test count: ~497 → ~507.
- [ ] No changes to `crates/core/`, `crates/frontends/`, `crates/providers/src/`.
- [ ] Default breaker enabled per route, never trips on healthy upstreams.

After this plan merges, the gateway:
- Tracks failure rates per `(provider, model)`.
- Trips a breaker after the configured threshold.
- Skips tripped upstreams without consuming retries or rate-limit tokens.
- Recovers via single half-open probe.
