//! Token-bucket rate limiting (Plan 04 P04 T2).
//!
//! Wraps the `governor` crate with a per-dimension registry. Each bucket
//! is keyed by `(dimension, key)` — for per-key buckets the key is the
//! AgentIdentity's hash; for per-route it's `<frontend>/<model>`; for
//! per-upstream it's the upstream name; for per-IP it's the IP string.
//!
//! Composition: a request must satisfy ALL applicable buckets. The first
//! bucket to reject names the dimension in the resulting `RateLimited`
//! error.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::RwLock;

use governor::clock::{Clock, DefaultClock};
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use nonzero_ext::nonzero;

pub use crate::errors::RateLimitDimension;

/// Configuration for a single bucket.
#[derive(Debug, Clone)]
pub struct BucketConfig {
    pub rate_per_sec: u32,
    pub burst: u32,
}

impl BucketConfig {
    fn quota(&self) -> Quota {
        let per_sec = NonZeroU32::new(self.rate_per_sec.max(1)).unwrap_or(nonzero!(1u32));
        Quota::per_second(per_sec)
            .allow_burst(NonZeroU32::new(self.burst.max(1)).unwrap_or(nonzero!(1u32)))
    }
}

type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

/// Outcome of a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitOutcome {
    Allowed,
    Limited { retry_after_secs: u32 },
}

/// Top-level limiter registry. Holds one `governor::RateLimiter` per
/// `(dimension, key)`. Buckets are created on first use.
pub struct LimiterRegistry {
    buckets: RwLock<HashMap<(RateLimitDimension, String), Arc<Limiter>>>,
    /// Default per-key bucket — used when no override applies AND identity is
    /// `KeyHash`. None when per_key bucketing is disabled in config.
    default_per_key: Option<BucketConfig>,
    /// Anonymous bucket — used when identity is `Anonymous`.
    anonymous: Option<BucketConfig>,
    /// Per-key overrides keyed by `sha256:<hex>`.
    per_key_overrides: HashMap<String, BucketConfig>,
    /// Per-route overrides keyed by `<frontend>/<model>`.
    per_route: HashMap<String, BucketConfig>,
    /// Per-upstream overrides keyed by upstream name.
    per_upstream: HashMap<String, BucketConfig>,
    /// Per-IP bucket (if enabled).
    per_ip: Option<BucketConfig>,
    /// Master switch.
    pub(crate) enabled: bool,
}

impl LimiterRegistry {
    pub fn from_config(cfg: &agent_shim_config::RateLimitConfig) -> Self {
        Self {
            buckets: RwLock::new(HashMap::new()),
            default_per_key: cfg.per_key.default.clone().map(BucketConfig::from),
            anonymous: cfg.per_key.anonymous.clone().map(BucketConfig::from),
            per_key_overrides: cfg
                .per_key
                .overrides
                .iter()
                .map(|(k, v)| (k.clone(), BucketConfig::from(v.clone())))
                .collect(),
            per_route: cfg
                .per_route
                .iter()
                .map(|(k, v)| (k.clone(), BucketConfig::from(v.clone())))
                .collect(),
            per_upstream: cfg
                .per_upstream
                .iter()
                .map(|(k, v)| (k.clone(), BucketConfig::from(v.clone())))
                .collect(),
            per_ip: if cfg.per_ip.enabled {
                Some(BucketConfig {
                    rate_per_sec: cfg.per_ip.rate_per_sec,
                    burst: cfg.per_ip.burst,
                })
            } else {
                None
            },
            enabled: cfg.enabled,
        }
    }

    /// Disabled-rate-limit construction (used when `rate_limit.enabled = false`
    /// or when no rate_limit block is configured at all).
    pub fn disabled() -> Self {
        Self {
            buckets: RwLock::new(HashMap::new()),
            default_per_key: None,
            anonymous: None,
            per_key_overrides: HashMap::new(),
            per_route: HashMap::new(),
            per_upstream: HashMap::new(),
            per_ip: None,
            enabled: false,
        }
    }

    fn get_or_create(
        &self,
        dim: RateLimitDimension,
        key: &str,
        cfg: &BucketConfig,
    ) -> Arc<Limiter> {
        let composite = (dim, key.to_string());
        if let Some(l) = self.buckets.read().unwrap().get(&composite) {
            return Arc::clone(l);
        }
        let mut w = self.buckets.write().unwrap();
        Arc::clone(
            w.entry(composite)
                .or_insert_with(|| Arc::new(RateLimiter::direct(cfg.quota()))),
        )
    }

    fn check_bucket(&self, limiter: &Limiter) -> LimitOutcome {
        match limiter.check() {
            Ok(_) => LimitOutcome::Allowed,
            Err(neg) => {
                let wait = neg.wait_time_from(DefaultClock::default().now());
                // Round up to the next whole second so the client never
                // retries earlier than the bucket allows. `as_secs()`
                // floors; e.g. 1.7s would become 1, telling the client
                // to retry 0.7s too early. The `.max(1)` floor handles
                // the sub-second case where the bucket is technically
                // limited but as_secs() already returns 0.
                let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
                LimitOutcome::Limited {
                    retry_after_secs: secs.max(1) as u32,
                }
            }
        }
    }

    /// Apply per-key + per-route + per-IP gates (run BEFORE the chain walk).
    /// Returns `Allowed` if every applicable bucket admits; otherwise
    /// names the first dimension to reject.
    pub fn check_pre_chain(
        &self,
        identity: &crate::auth::AgentIdentity,
        frontend_model: &str,
        client_ip: &str,
    ) -> Result<(), (RateLimitDimension, u32)> {
        if !self.enabled {
            return Ok(());
        }

        // Per-key
        let key_cfg = match identity {
            crate::auth::AgentIdentity::KeyHash(h) => self
                .per_key_overrides
                .get(h.as_str())
                .or(self.default_per_key.as_ref())
                .cloned(),
            crate::auth::AgentIdentity::Anonymous => self.anonymous.clone(),
        };
        if let Some(cfg) = key_cfg {
            let bucket = self.get_or_create(RateLimitDimension::PerKey, identity.log_id(), &cfg);
            if let LimitOutcome::Limited { retry_after_secs } = self.check_bucket(&bucket) {
                return Err((RateLimitDimension::PerKey, retry_after_secs));
            }
        }

        // Per-route
        if let Some(cfg) = self.per_route.get(frontend_model).cloned() {
            let bucket = self.get_or_create(RateLimitDimension::PerRoute, frontend_model, &cfg);
            if let LimitOutcome::Limited { retry_after_secs } = self.check_bucket(&bucket) {
                return Err((RateLimitDimension::PerRoute, retry_after_secs));
            }
        }

        // Per-IP (only when enabled)
        if let Some(cfg) = self.per_ip.clone() {
            let bucket = self.get_or_create(RateLimitDimension::PerIp, client_ip, &cfg);
            if let LimitOutcome::Limited { retry_after_secs } = self.check_bucket(&bucket) {
                return Err((RateLimitDimension::PerIp, retry_after_secs));
            }
        }

        Ok(())
    }

    /// Per-upstream gate (run INSIDE the chain walk on each element).
    pub fn check_per_upstream(&self, upstream: &str) -> Result<(), (RateLimitDimension, u32)> {
        if !self.enabled {
            return Ok(());
        }
        if let Some(cfg) = self.per_upstream.get(upstream).cloned() {
            let bucket = self.get_or_create(RateLimitDimension::PerUpstream, upstream, &cfg);
            if let LimitOutcome::Limited { retry_after_secs } = self.check_bucket(&bucket) {
                return Err((RateLimitDimension::PerUpstream, retry_after_secs));
            }
        }
        Ok(())
    }
}

impl From<agent_shim_config::BucketConfigYaml> for BucketConfig {
    fn from(c: agent_shim_config::BucketConfigYaml) -> Self {
        Self {
            rate_per_sec: c.rate_per_sec,
            burst: c.burst,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AgentIdentity;
    use std::collections::BTreeMap;

    fn small_config() -> agent_shim_config::RateLimitConfig {
        // Small burst (2) so a 3rd request in the same instant gets limited.
        let bucket = agent_shim_config::BucketConfigYaml {
            rate_per_sec: 1,
            burst: 2,
        };
        let mut cfg = agent_shim_config::RateLimitConfig {
            enabled: true,
            per_key: agent_shim_config::PerKeyConfig {
                default: Some(bucket.clone()),
                anonymous: Some(bucket.clone()),
                overrides: BTreeMap::new(),
            },
            per_route: BTreeMap::new(),
            per_upstream: BTreeMap::new(),
            per_ip: agent_shim_config::PerIpConfig {
                enabled: false,
                rate_per_sec: 5,
                burst: 5,
            },
        };
        cfg.per_route
            .insert("openai_chat/gpt-4o".to_string(), bucket.clone());
        cfg.per_upstream.insert("openai".to_string(), bucket);
        cfg
    }

    /// Variant of `small_config` with only per_key buckets configured so the
    /// per_route bucket cannot pollute tests that exercise per-key behavior
    /// across distinct identities.
    fn per_key_only_config() -> agent_shim_config::RateLimitConfig {
        let bucket = agent_shim_config::BucketConfigYaml {
            rate_per_sec: 1,
            burst: 2,
        };
        agent_shim_config::RateLimitConfig {
            enabled: true,
            per_key: agent_shim_config::PerKeyConfig {
                default: Some(bucket.clone()),
                anonymous: Some(bucket),
                overrides: BTreeMap::new(),
            },
            per_route: BTreeMap::new(),
            per_upstream: BTreeMap::new(),
            per_ip: agent_shim_config::PerIpConfig {
                enabled: false,
                rate_per_sec: 5,
                burst: 5,
            },
        }
    }

    #[test]
    fn allows_burst_then_rejects_third() {
        // Use per_key_only_config so the assertion can pin the dimension
        // strictly to PerKey. The original config also configured a
        // per_route bucket of identical shape, which would reject at the
        // same moment — the original test had to allow either dimension,
        // making it tolerant to a regression that swapped the order.
        let registry = LimiterRegistry::from_config(&per_key_only_config());
        let id = AgentIdentity::Anonymous;
        assert!(registry
            .check_pre_chain(&id, "openai_chat/gpt-4o", "127.0.0.1")
            .is_ok());
        assert!(registry
            .check_pre_chain(&id, "openai_chat/gpt-4o", "127.0.0.1")
            .is_ok());
        match registry.check_pre_chain(&id, "openai_chat/gpt-4o", "127.0.0.1") {
            Err((dim, secs)) => {
                assert_eq!(dim, RateLimitDimension::PerKey);
                assert!(secs >= 1);
            }
            Ok(()) => panic!("expected limit on 3rd request"),
        }
    }

    #[test]
    fn first_rejection_wins_per_key_before_per_route() {
        // Both per-key and per-route buckets reject on the same request;
        // the documented contract says per-key is checked first and
        // names the rejection. Locks in the dimension-priority contract
        // against future reorderings.
        let registry = LimiterRegistry::from_config(&small_config());
        let id = AgentIdentity::Anonymous;
        // Burn both buckets via two successful requests (each consumes
        // one token from per_key AND one from per_route since burst=2).
        for _ in 0..2 {
            assert!(registry
                .check_pre_chain(&id, "openai_chat/gpt-4o", "127.0.0.1")
                .is_ok());
        }
        match registry.check_pre_chain(&id, "openai_chat/gpt-4o", "127.0.0.1") {
            Err((dim, _)) => assert_eq!(
                dim,
                RateLimitDimension::PerKey,
                "per-key must win over per-route on the first rejection"
            ),
            Ok(()) => panic!("expected limit on 3rd request"),
        }
    }

    #[test]
    fn distinct_keys_have_independent_buckets() {
        // Use per_key_only_config so the shared per_route bucket doesn't
        // intercept bob's request after alice drains it.
        let registry = LimiterRegistry::from_config(&per_key_only_config());
        let alice = AgentIdentity::KeyHash("sha256:aaa".to_string());
        let bob = AgentIdentity::KeyHash("sha256:bbb".to_string());
        // Burn alice's bucket.
        for _ in 0..2 {
            assert!(registry
                .check_pre_chain(&alice, "openai_chat/gpt-4o", "127.0.0.1")
                .is_ok());
        }
        assert!(registry
            .check_pre_chain(&alice, "openai_chat/gpt-4o", "127.0.0.1")
            .is_err());
        // Bob is independent.
        assert!(registry
            .check_pre_chain(&bob, "openai_chat/gpt-4o", "127.0.0.1")
            .is_ok());
    }

    #[test]
    fn per_upstream_check_is_independent_from_pre_chain() {
        let registry = LimiterRegistry::from_config(&small_config());
        // Burn the per_upstream bucket for "openai".
        for _ in 0..2 {
            assert!(registry.check_per_upstream("openai").is_ok());
        }
        assert!(matches!(
            registry.check_per_upstream("openai"),
            Err((RateLimitDimension::PerUpstream, _))
        ));
        // Different upstream still allowed.
        assert!(registry.check_per_upstream("anthropic").is_ok());
    }

    #[test]
    fn disabled_registry_always_allows() {
        let registry = LimiterRegistry::disabled();
        let id = AgentIdentity::Anonymous;
        for _ in 0..1000 {
            assert!(registry
                .check_pre_chain(&id, "openai_chat/gpt-4o", "127.0.0.1")
                .is_ok());
            assert!(registry.check_per_upstream("openai").is_ok());
        }
    }
}
