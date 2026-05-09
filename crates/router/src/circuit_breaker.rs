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
use std::sync::atomic::{AtomicBool, Ordering};
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
    Probe,
}

/// Internal state for one breaker. Concurrent access is guarded by the
/// registry's `RwLock`.
#[allow(dead_code)] // Used by `BreakerRegistry` in Plan 03 T2.
struct BreakerState {
    samples: VecDeque<(Instant, bool)>, // true = success, false = failure
    open_since: Option<Instant>,
    /// Set to true while the half-open probe is in flight; prevents a second
    /// probe from being authorized concurrently.
    probe_in_flight: AtomicBool,
}

#[allow(dead_code)] // Used by `BreakerRegistry` in Plan 03 T2.
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
        while self
            .samples
            .front()
            .map(|(t, _)| *t < cutoff)
            .unwrap_or(false)
        {
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
        let mut state = BreakerState::new();
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
        let mut state = BreakerState::new();
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
        let mut state = BreakerState::new();
        for _ in 0..100 {
            state.record(false, clock.now(), &policy);
        }
        assert_eq!(state.decision(clock.now(), &policy), BreakerDecision::Allow);
    }
}
