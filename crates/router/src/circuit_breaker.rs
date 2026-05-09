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

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Injectable clock so transitions are deterministic in tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// Production clock — wraps `Instant::now()`.
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
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
    ///
    /// **Caller contract:** when this is returned, the caller MUST follow
    /// up with [`BreakerState::record`] (or [`BreakerRegistry::record`])
    /// regardless of whether the probe succeeded or failed. Failing to
    /// do so leaves `probe_in_flight` set, which wedges the breaker
    /// permanently in Skip until the process restarts. `ResilientCaller`
    /// (Plan 03 T3) enforces this by recording every retry-loop outcome.
    Probe,
}

/// Internal state for one breaker. Concurrent access is guarded by the
/// registry's `RwLock`.
struct BreakerState {
    /// `(provider, model)` pair this breaker tracks. Used solely for the
    /// `breaker.state_change` tracing event (Plan 05 P05 T1) so operators
    /// can pinpoint which breaker tripped.
    upstream: String,
    model: String,
    samples: VecDeque<(Instant, bool)>, // true = success, false = failure
    open_since: Option<Instant>,
    /// Set to true while the half-open probe is in flight; prevents a second
    /// probe from being authorized concurrently.
    probe_in_flight: AtomicBool,
}

impl BreakerState {
    fn new(upstream: String, model: String) -> Self {
        Self {
            upstream,
            model,
            samples: VecDeque::new(),
            open_since: None,
            probe_in_flight: AtomicBool::new(false),
        }
    }

    /// Emit a `breaker.state_change` tracing event. Centralized so the four
    /// transition points stay consistent (Plan 05 P05 T1).
    fn emit_state_change(&self, from: &'static str, to: &'static str, reason: &'static str) {
        tracing::info!(
            target: "agent_shim::resilience",
            event_name = "breaker.state_change",
            upstream = %self.upstream,
            model = %self.model,
            from_state = from,
            to_state = to,
            reason = reason,
            "circuit breaker state changed"
        );
    }

    fn record(&mut self, succeeded: bool, now: Instant, policy: &BreakerPolicy) {
        // Drop samples outside the window first. If `now - window` underflows
        // (clock can't go that far back — extremely rare but possible early in
        // process lifetime), skip eviction entirely rather than collapsing the
        // cutoff to `now` and evicting almost every sample.
        if let Some(cutoff) = now.checked_sub(policy.window) {
            while self
                .samples
                .front()
                .map(|(t, _)| *t < cutoff)
                .unwrap_or(false)
            {
                self.samples.pop_front();
            }
        }
        self.samples.push_back((now, succeeded));

        // Re-evaluate state.
        if let Some(opened) = self.open_since {
            // We're Open or HalfOpen. Probes are handled in query_state;
            // here we only transition based on the probe result.
            if succeeded {
                // Half-open probe succeeded → Closed.
                self.emit_state_change("half_open", "closed", "probe_succeeded");
                self.open_since = None;
                self.samples.clear();
                self.samples.push_back((now, true));
            } else if now.duration_since(opened) >= policy.open_cooldown {
                // Half-open probe failed → re-open with fresh cooldown.
                self.emit_state_change("half_open", "open", "probe_failed");
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
            self.emit_state_change("closed", "open", "failure_threshold_exceeded");
            self.open_since = Some(now);
        }
    }

    /// Returns the breaker's current decision for a fresh request.
    ///
    /// When this returns [`BreakerDecision::Probe`], the caller MUST
    /// pair it with a subsequent [`BreakerState::record`] call (success
    /// or failure) — see the variant docs for the full contract.
    ///
    /// Note: takes `&self` (read-only). The `Open → HalfOpen` state
    /// transition is implicit — `open_since` doesn't get cleared until
    /// `record()` runs after the probe — but a `breaker.state_change`
    /// event is still emitted at the moment the probe is authorized
    /// (Plan 05 P05 T1) so operators see the cooldown elapsed.
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
                        self.emit_state_change("open", "half_open", "cooldown_elapsed");
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
        fn new() -> Self {
            Self(std::sync::Mutex::new(Instant::now()))
        }
        fn advance(&self, d: Duration) {
            let mut t = self.0.lock().unwrap();
            *t += d;
        }
    }
    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            *self.0.lock().unwrap()
        }
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

    fn new_state() -> BreakerState {
        BreakerState::new("openai".to_string(), "gpt-4o".to_string())
    }

    #[test]
    fn closed_under_min_requests_never_trips() {
        let clock = FakeClock::new();
        let policy = fast_policy();
        let mut state = new_state();
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
        let mut state = new_state();
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
        let mut state = new_state();
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
        let mut state = new_state();
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
        let mut state = new_state();
        for _ in 0..5 {
            state.record(false, clock.now(), &policy);
        }
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
        let mut state = new_state();
        for _ in 0..5 {
            state.record(false, clock.now(), &policy);
        }
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
        let policy = fast_policy(); // window = 60s
        let mut state = new_state();
        // 5 old failures, all outside the window after advance.
        for _ in 0..5 {
            state.record(false, clock.now(), &policy);
        }
        clock.advance(Duration::from_secs(61));
        // New successes only — failures evicted; should not trip.
        for _ in 0..5 {
            state.record(true, clock.now(), &policy);
        }
        assert_eq!(state.decision(clock.now(), &policy), BreakerDecision::Allow);
    }

    #[test]
    fn disabled_breaker_always_allows() {
        let clock = FakeClock::new();
        let policy = BreakerPolicy {
            enabled: false,
            ..fast_policy()
        };
        let mut state = new_state();
        for _ in 0..100 {
            state.record(false, clock.now(), &policy);
        }
        assert_eq!(state.decision(clock.now(), &policy), BreakerDecision::Allow);
    }
}

/// `(provider, model)` key for the breaker registry.
type BreakerKey = (String, String);

/// Map from breaker key to its shared state. Each entry is wrapped in
/// `Arc<RwLock<_>>` so the outer registry lock can be released while a
/// per-key call holds the inner write guard.
type BreakerMap = HashMap<BreakerKey, Arc<RwLock<BreakerState>>>;

/// Registry keyed by `(provider, model)`. Each key has its own
/// `BreakerState` behind the registry's `RwLock`.
///
/// **Memory growth:** the inner map only inserts and never evicts, so its
/// cardinality is bounded by the set of `(provider, model)` pairs that have
/// ever been observed. Under Phase 4 P03 this is bounded by the static route
/// table; future wildcard / dynamic-model routing should revisit this and
/// add an eviction strategy if the bound becomes loose.
///
/// **Lock-poisoning policy:** internal `read()`/`write()` calls
/// `.unwrap()` poisoned guards. A panic inside any guard taints that
/// breaker; callers should treat a poisoned breaker as a fatal process
/// invariant violation and propagate the panic.
pub struct BreakerRegistry {
    breakers: RwLock<BreakerMap>,
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
        Arc::clone(w.entry(key).or_insert_with(|| {
            Arc::new(RwLock::new(BreakerState::new(
                provider.to_string(),
                model.to_string(),
            )))
        }))
    }

    /// Query whether the chain walker should call this upstream.
    pub fn decision(&self, provider: &str, model: &str, policy: &BreakerPolicy) -> BreakerDecision {
        let state = self.get_or_create(provider, model);
        let s = state.read().unwrap();
        s.decision(self.clock.now(), policy)
    }

    /// Record the outcome of a call (regardless of how the breaker decided).
    pub fn record(&self, provider: &str, model: &str, succeeded: bool, policy: &BreakerPolicy) {
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

    #[test]
    fn get_or_create_returns_same_arc_for_same_key() {
        // Locks in the dedup invariant of `get_or_create`'s slow path:
        // two calls for the same `(provider, model)` must yield clones of
        // the *same* Arc, not two distinct breakers. The concurrent test
        // above only proves liveness; this asserts identity.
        let registry = BreakerRegistry::with_system_clock();
        let a = registry.get_or_create("openai", "gpt-4o");
        let b = registry.get_or_create("openai", "gpt-4o");
        assert!(Arc::ptr_eq(&a, &b));
        // Distinct key → distinct Arc.
        let c = registry.get_or_create("anthropic", "claude-opus-4-7");
        assert!(!Arc::ptr_eq(&a, &c));
    }
}
