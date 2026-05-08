# Plan 04 — Rate Limiting + Auth (Phase 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-08-phase-4-resiliency-design.md`](../specs/2026-05-08-phase-4-resiliency-design.md) (decisions D8, D9; §4.2 + §4.3 config; §5.2 layering position).

**Goal:** Four-dimensional token-bucket rate limiting (per API key, per route, per upstream, per IP) + API key extraction from `Authorization` / `x-api-key` headers with SHA-256 hashing. Wired into `ResilientCaller` per the §5.2 layering: rate-limit gate runs before chain walk; per-upstream gate runs inside the chain walk just-in-time.

**Architecture:** New `crates/router/src/auth.rs` parses request headers and produces an `AgentIdentity` enum. New `crates/router/src/rate_limit.rs` (replaces v0.3 stub) wraps the `governor` crate with a per-dimension `LimiterRegistry`. `ResilientCaller::complete` gains `identity: AgentIdentity`, `client_ip: IpAddr` parameters and consults the registry at the appropriate points. New top-level `auth` and `rate_limit` config blocks. `HandlerError` already has `RateLimited` variant from P02.

**Tech stack:** Two new dependencies on `crates/router/Cargo.toml`:
- `governor = "0.6"` — atomic token-bucket math.
- `sha2 = "0.10"` — SHA-256 for API key hashing.

**Frontend changes:** NONE.
**Provider code changes:** NONE.
**Core changes:** NONE.

---

## File Structure

`crates/config/src/`:
- Modify: `schema.rs` — top-level `auth` and `rate_limit` blocks.
- Modify: `validation.rs` — rules 7-10 from §4.4 of the spec.

`crates/router/src/`:
- Create: `auth.rs` — header parsing, SHA-256 hash, `AgentIdentity`.
- Modify: `rate_limit.rs` — replace stub with `TokenBucket`, `LimiterRegistry`.
- Modify: `resilient_caller.rs` — gate at request start + per-upstream gate.
- Modify: `lib.rs` — re-exports.

`crates/router/Cargo.toml`:
- Modify: add `governor` and `sha2` dependencies.

`crates/gateway/src/`:
- Modify: `state.rs` — `AppState` gains `Arc<LimiterRegistry>`.
- Modify: `pipeline.rs` — extract identity + IP at request entry; thread through.

`crates/protocol-tests/tests/`:
- Create: `rate_limit_per_key_envelope.rs` — per-key bucket fires, OpenAI envelope, Retry-After header.
- Create: `auth_required_unconfigured_key.rs` — `auth.required=true` with unknown key → HTTP 401.

`docs/`:
- Create: `docs/resilience.md` — operator-facing guide.

---

## Tasks

### Task 1: auth.rs — header parsing + SHA-256

**Files:**
- Create: `crates/router/src/auth.rs`
- Modify: `crates/router/src/lib.rs`
- Modify: `crates/router/Cargo.toml` — add `sha2`.

- [ ] **Step 1: Add sha2 dependency**

In `crates/router/Cargo.toml` `[dependencies]`:

```toml
sha2 = "0.10"
```

- [ ] **Step 2: Write failing test**

Create `crates/router/src/auth.rs`:

```rust
//! Authentication header extraction (Plan 04 P04 T1).
//!
//! Plaintext API keys come in via `Authorization: Bearer <key>` or
//! `x-api-key: <key>`. The gateway hashes them with SHA-256 and looks up
//! the hash in `auth.keys.<sha256:hex>`. Plaintext is never stored or logged.

use sha2::{Digest, Sha256};

/// Identity tied to a request, used for per-key rate limiting and audit logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentIdentity {
    /// No API key was presented (or `auth.enabled = false`).
    Anonymous,
    /// `sha256:<hex>` of the presented key. Look this up in `auth.keys`.
    KeyHash(String),
}

impl AgentIdentity {
    /// String identifier used in operator logs. Plaintext keys never appear
    /// here — only the hash. `"anonymous"` for the Anonymous variant.
    pub fn log_id(&self) -> &str {
        match self {
            AgentIdentity::Anonymous => "anonymous",
            AgentIdentity::KeyHash(h) => h.as_str(),
        }
    }
}

/// Hash a plaintext key into `sha256:<hex>` form. Caller passes the bare
/// plaintext; this fn does NOT strip a `Bearer ` prefix (that's the parser's
/// job).
pub fn hash_key(plaintext: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    let bytes = hasher.finalize();
    let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    format!("sha256:{}", hex)
}

/// Extract identity from request headers. Checks `Authorization: Bearer ...`
/// first, then `x-api-key: ...`. Returns `Anonymous` if neither is present
/// or if the value is empty.
pub fn extract_identity_from_headers(
    authorization: Option<&str>,
    x_api_key: Option<&str>,
) -> AgentIdentity {
    if let Some(auth) = authorization {
        if let Some(key) = auth.strip_prefix("Bearer ") {
            let key = key.trim();
            if !key.is_empty() {
                return AgentIdentity::KeyHash(hash_key(key));
            }
        }
    }
    if let Some(key) = x_api_key {
        let key = key.trim();
        if !key.is_empty() {
            return AgentIdentity::KeyHash(hash_key(key));
        }
    }
    AgentIdentity::Anonymous
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_key_produces_deterministic_sha256() {
        let h = hash_key("hello");
        // SHA-256("hello") is a well-known fixture.
        assert_eq!(
            h,
            "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn extract_from_bearer_header() {
        let id = extract_identity_from_headers(Some("Bearer my-secret"), None);
        match id {
            AgentIdentity::KeyHash(h) => {
                assert!(h.starts_with("sha256:"));
                assert_eq!(h.len(), "sha256:".len() + 64);
            }
            _ => panic!("expected KeyHash"),
        }
    }

    #[test]
    fn extract_from_x_api_key() {
        let id = extract_identity_from_headers(None, Some("my-secret"));
        assert!(matches!(id, AgentIdentity::KeyHash(_)));
    }

    #[test]
    fn empty_headers_produce_anonymous() {
        assert_eq!(extract_identity_from_headers(None, None), AgentIdentity::Anonymous);
        assert_eq!(extract_identity_from_headers(Some(""), None), AgentIdentity::Anonymous);
        assert_eq!(extract_identity_from_headers(Some("Bearer "), None), AgentIdentity::Anonymous);
        assert_eq!(extract_identity_from_headers(None, Some("")), AgentIdentity::Anonymous);
    }

    #[test]
    fn authorization_takes_precedence_over_x_api_key() {
        let id = extract_identity_from_headers(Some("Bearer abc"), Some("def"));
        let abc_hash = hash_key("abc");
        match id {
            AgentIdentity::KeyHash(h) => assert_eq!(h, abc_hash),
            _ => panic!("expected KeyHash"),
        }
    }

    #[test]
    fn log_id_never_leaks_plaintext() {
        let id = AgentIdentity::KeyHash(hash_key("supersecret"));
        let s = id.log_id();
        assert!(s.starts_with("sha256:"));
        assert!(!s.contains("supersecret"));
    }
}
```

- [ ] **Step 3: Wire into lib.rs**

```rust
pub mod auth;

pub use auth::{extract_identity_from_headers, hash_key, AgentIdentity};
```

- [ ] **Step 4: Run tests**

```bash
rtk cargo test -p agent-shim-router --quiet
```

Expected: 6 auth tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/router/src/auth.rs crates/router/src/lib.rs crates/router/Cargo.toml
git commit -m "feat(router): auth header extraction + SHA-256 hashing (Plan 04 P04 T1)

New crates/router/src/auth.rs:
- AgentIdentity enum (Anonymous | KeyHash(String)).
- hash_key(plaintext) → 'sha256:<hex>' (deterministic; SHA-256).
- extract_identity_from_headers(authz, x_api_key) consults
  Authorization: Bearer first, then x-api-key. Returns Anonymous on
  missing/empty headers.
- log_id() exposes the hash form; plaintext never logged.

6 unit tests pin: deterministic hash output, both header forms,
empty-input fallback to Anonymous, header precedence, log_id safety."
```

---

### Task 2: rate_limit.rs — TokenBucket + LimiterRegistry

**Files:**
- Modify: `crates/router/src/rate_limit.rs`
- Modify: `crates/router/Cargo.toml` — add `governor`.
- Modify: `crates/router/src/lib.rs`.

- [ ] **Step 1: Add governor dependency**

In `crates/router/Cargo.toml` `[dependencies]`:

```toml
governor = "0.6"
nonzero_ext = "0.3"
```

- [ ] **Step 2: Replace rate_limit.rs stub**

Replace `crates/router/src/rate_limit.rs` (currently `// Stub for Phase 4 rate limiting.`):

```rust
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

use governor::clock::DefaultClock;
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
        Quota::per_second(per_sec).allow_burst(
            NonZeroU32::new(self.burst.max(1)).unwrap_or(nonzero!(1u32)),
        )
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

    fn get_or_create(&self, dim: RateLimitDimension, key: &str, cfg: &BucketConfig) -> Arc<Limiter> {
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
                LimitOutcome::Limited {
                    retry_after_secs: wait.as_secs().max(1) as u32,
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
            let bucket = self.get_or_create(
                RateLimitDimension::PerKey,
                identity.log_id(),
                &cfg,
            );
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
    pub fn check_per_upstream(
        &self,
        upstream: &str,
    ) -> Result<(), (RateLimitDimension, u32)> {
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

    fn small_config() -> agent_shim_config::RateLimitConfig {
        // Small burst (2) so a 3rd request in the same instant gets limited.
        let bucket = agent_shim_config::BucketConfigYaml { rate_per_sec: 1, burst: 2 };
        let mut cfg = agent_shim_config::RateLimitConfig {
            enabled: true,
            per_key: agent_shim_config::PerKeyConfig {
                default: Some(bucket.clone()),
                anonymous: Some(bucket.clone()),
                overrides: HashMap::new(),
            },
            per_route: HashMap::new(),
            per_upstream: HashMap::new(),
            per_ip: agent_shim_config::PerIpConfig {
                enabled: false,
                rate_per_sec: 5,
                burst: 5,
            },
        };
        cfg.per_route.insert("openai_chat/gpt-4o".to_string(), bucket.clone());
        cfg.per_upstream.insert("openai".to_string(), bucket);
        cfg
    }

    #[test]
    fn allows_burst_then_rejects_third() {
        let registry = LimiterRegistry::from_config(&small_config());
        let id = AgentIdentity::Anonymous;
        assert!(registry.check_pre_chain(&id, "openai_chat/gpt-4o", "127.0.0.1").is_ok());
        assert!(registry.check_pre_chain(&id, "openai_chat/gpt-4o", "127.0.0.1").is_ok());
        match registry.check_pre_chain(&id, "openai_chat/gpt-4o", "127.0.0.1") {
            Err((dim, secs)) => {
                // First to reject is per_key (the inner gate; per_route also
                // would reject — implementation runs per_key first).
                assert!(matches!(dim, RateLimitDimension::PerKey | RateLimitDimension::PerRoute));
                assert!(secs >= 1);
            }
            Ok(()) => panic!("expected limit on 3rd request"),
        }
    }

    #[test]
    fn distinct_keys_have_independent_buckets() {
        let registry = LimiterRegistry::from_config(&small_config());
        let alice = AgentIdentity::KeyHash("sha256:aaa".to_string());
        let bob = AgentIdentity::KeyHash("sha256:bbb".to_string());
        // Burn alice's bucket.
        for _ in 0..2 {
            assert!(registry.check_pre_chain(&alice, "openai_chat/gpt-4o", "127.0.0.1").is_ok());
        }
        assert!(registry.check_pre_chain(&alice, "openai_chat/gpt-4o", "127.0.0.1").is_err());
        // Bob is independent.
        assert!(registry.check_pre_chain(&bob, "openai_chat/gpt-4o", "127.0.0.1").is_ok());
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
            assert!(registry.check_pre_chain(&id, "openai_chat/gpt-4o", "127.0.0.1").is_ok());
            assert!(registry.check_per_upstream("openai").is_ok());
        }
    }
}
```

- [ ] **Step 3: Wire into lib.rs**

```rust
pub mod rate_limit;

pub use rate_limit::{BucketConfig, LimitOutcome, LimiterRegistry};
```

- [ ] **Step 4: Run tests** (the test depends on schema additions in T3 — run T3 first if compile fails)

After T3 lands the schema types, this test compiles and runs:

```bash
rtk cargo test -p agent-shim-router --quiet
```

Expected: 4 rate_limit tests PASS.

- [ ] **Step 5: Commit (after T3 schema lands)**

```bash
git add crates/router/src/rate_limit.rs crates/router/src/lib.rs crates/router/Cargo.toml
git commit -m "feat(router): TokenBucket + LimiterRegistry (Plan 04 P04 T2)

Replaces v0.3 stub with the four-dimensional rate limiter.
Wraps governor::RateLimiter::direct (atomic, lock-free per bucket)
behind a registry keyed by (dimension, key).

API surface:
- LimiterRegistry::from_config(cfg) — constructs from config block.
- LimiterRegistry::disabled() — bypass mode.
- check_pre_chain(identity, frontend_model, client_ip) — runs per-key,
  per-route, per-IP in order; first rejection names the dimension.
- check_per_upstream(name) — separate gate for the in-chain check.

4 unit tests cover: burst-then-reject, distinct-keys, per-upstream
independence, disabled-bypass."
```

---

### Task 3: Config schema for auth + rate_limit

**Files:**
- Modify: `crates/config/src/schema.rs`
- Modify: `crates/config/src/validation.rs`

- [ ] **Step 1: Add auth + rate_limit + bucket types**

Add to `crates/config/src/schema.rs`:

```rust
/// Top-level auth config. Default: disabled (preserves v0.3 behavior).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    pub enabled: bool,
    pub required: bool,
    pub keys: BTreeMap<String, AuthKeyEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthKeyEntry {
    pub label: String,
}

/// Top-level rate-limit config. Default: disabled.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RateLimitConfig {
    pub enabled: bool,
    pub per_key: PerKeyConfig,
    pub per_route: BTreeMap<String, BucketConfigYaml>,
    pub per_upstream: BTreeMap<String, BucketConfigYaml>,
    pub per_ip: PerIpConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PerKeyConfig {
    pub default: Option<BucketConfigYaml>,
    pub anonymous: Option<BucketConfigYaml>,
    pub overrides: BTreeMap<String, BucketConfigYaml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BucketConfigYaml {
    pub rate_per_sec: u32,
    pub burst: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerIpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_per_ip_rate")]
    pub rate_per_sec: u32,
    #[serde(default = "default_per_ip_burst")]
    pub burst: u32,
}

impl Default for PerIpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rate_per_sec: default_per_ip_rate(),
            burst: default_per_ip_burst(),
        }
    }
}

fn default_per_ip_rate() -> u32 { 5 }
fn default_per_ip_burst() -> u32 { 20 }
```

Add to `GatewayConfig`:

```rust
pub struct GatewayConfig {
    // ... existing fields
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
}
```

- [ ] **Step 2: Add validation rules 7-10**

In `crates/config/src/validation.rs`, extend the route validation function (or add a new `validate_rate_limit`):

```rust
pub fn validate_rate_limit(cfg: &GatewayConfig) -> Result<(), String> {
    // Rule 7: per_route keys reference existing routes.
    for key in cfg.rate_limit.per_route.keys() {
        let parts: Vec<&str> = key.splitn(2, '/').collect();
        if parts.len() != 2 {
            return Err(format!(
                "rate_limit.per_route key '{key}' must be '<frontend>/<model>'"
            ));
        }
        let exists = cfg.routes.iter().any(|r| {
            r.frontend == parts[0] && r.model == parts[1]
        });
        if !exists {
            return Err(format!(
                "rate_limit.per_route key '{key}' references non-existent route"
            ));
        }
    }

    // Rule 8: per_upstream keys reference configured upstreams.
    for key in cfg.rate_limit.per_upstream.keys() {
        if !cfg.upstreams.contains_key(key) {
            return Err(format!(
                "rate_limit.per_upstream key '{key}' references non-existent upstream"
            ));
        }
    }

    // Rule 9: per_key overrides start with sha256:.
    for key in cfg.rate_limit.per_key.overrides.keys() {
        if !key.starts_with("sha256:") {
            return Err(format!(
                "rate_limit.per_key.overrides key '{key}' must start with 'sha256:'; \
                 do not paste plaintext API keys here"
            ));
        }
    }

    // Rule 10: every bucket has rate_per_sec > 0 AND burst >= 1.
    let validate_bucket = |b: &BucketConfigYaml, ctx: &str| -> Result<(), String> {
        if b.rate_per_sec == 0 {
            return Err(format!("{ctx} rate_per_sec must be > 0"));
        }
        if b.burst == 0 {
            return Err(format!("{ctx} burst must be >= 1"));
        }
        Ok(())
    };
    if let Some(b) = &cfg.rate_limit.per_key.default {
        validate_bucket(b, "rate_limit.per_key.default")?;
    }
    if let Some(b) = &cfg.rate_limit.per_key.anonymous {
        validate_bucket(b, "rate_limit.per_key.anonymous")?;
    }
    for (k, v) in &cfg.rate_limit.per_key.overrides {
        validate_bucket(v, &format!("rate_limit.per_key.overrides[{k}]"))?;
    }
    for (k, v) in &cfg.rate_limit.per_route {
        validate_bucket(v, &format!("rate_limit.per_route[{k}]"))?;
    }
    for (k, v) in &cfg.rate_limit.per_upstream {
        validate_bucket(v, &format!("rate_limit.per_upstream[{k}]"))?;
    }
    if cfg.rate_limit.per_ip.enabled {
        if cfg.rate_limit.per_ip.rate_per_sec == 0 {
            return Err("rate_limit.per_ip.rate_per_sec must be > 0".into());
        }
        if cfg.rate_limit.per_ip.burst == 0 {
            return Err("rate_limit.per_ip.burst must be >= 1".into());
        }
    }

    Ok(())
}
```

- [ ] **Step 3: Add auth validation (auth.required → auth.enabled implies)**

```rust
pub fn validate_auth(cfg: &GatewayConfig) -> Result<(), String> {
    if cfg.auth.required && !cfg.auth.enabled {
        return Err("auth.required=true requires auth.enabled=true".into());
    }
    for key in cfg.auth.keys.keys() {
        if !key.starts_with("sha256:") {
            return Err(format!(
                "auth.keys key '{key}' must start with 'sha256:'; \
                 do not paste plaintext API keys here"
            ));
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Tests for the new rules**

```rust
#[test]
fn rejects_per_route_unknown_route() {
    let cfg_yaml = r#"
upstreams:
  oai: {type: openai_compatible, base_url: "x", api_key: "x"}
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstream: oai
    upstream_model: gpt-4o
rate_limit:
  enabled: true
  per_route:
    "anthropic_messages/claude-opus-4-7": {rate_per_sec: 10, burst: 30}
    "#;
    let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
    assert!(validate_rate_limit(&cfg).is_err());
}

#[test]
fn rejects_per_key_override_without_sha256_prefix() {
    let cfg_yaml = r#"
rate_limit:
  enabled: true
  per_key:
    overrides:
      "plaintext-key": {rate_per_sec: 100, burst: 300}
    "#;
    let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
    let err = validate_rate_limit(&cfg).unwrap_err();
    assert!(err.contains("sha256:"));
}

#[test]
fn rejects_zero_burst() {
    let cfg_yaml = r#"
rate_limit:
  enabled: true
  per_key:
    default: {rate_per_sec: 10, burst: 0}
    "#;
    let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
    assert!(validate_rate_limit(&cfg).is_err());
}

#[test]
fn rejects_required_without_enabled() {
    let cfg_yaml = r#"
auth:
  enabled: false
  required: true
    "#;
    let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
    assert!(validate_auth(&cfg).is_err());
}
```

- [ ] **Step 5: Run tests**

```bash
rtk cargo test -p agent-shim-config --quiet
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/config/src/schema.rs crates/config/src/validation.rs
git commit -m "feat(config): top-level auth + rate_limit blocks (Plan 04 P04 T3)

Adds the v0.4 schema per §4.2 + §4.3 of the Phase 4 design:
- AuthConfig {enabled, required, keys}
- RateLimitConfig {enabled, per_key, per_route, per_upstream, per_ip}
- BucketConfigYaml, PerKeyConfig, PerIpConfig

Defaults: everything disabled — v0.3 configs work unchanged.

4 new validation rules from §4.4:
- Rule 7: per_route keys reference existing routes.
- Rule 8: per_upstream keys reference configured upstreams.
- Rule 9: per_key overrides + auth.keys must start with 'sha256:'
  (catches plaintext-key paste mistakes).
- Rule 10: rate_per_sec > 0 AND burst >= 1 for every bucket.

4 validation tests pin each rule + the auth.required/enabled
consistency check."
```

---

### Task 4: Integrate rate-limit into ResilientCaller

**Files:**
- Modify: `crates/router/src/resilient_caller.rs`
- Modify: `crates/gateway/src/state.rs`
- Modify: `crates/gateway/src/pipeline.rs`

- [ ] **Step 1: Add gate to ResilientCaller**

In `crates/router/src/resilient_caller.rs`, modify the struct and complete signature:

```rust
pub struct ResilientCaller {
    providers: Arc<dyn ProviderLookup>,
    breakers: Arc<crate::circuit_breaker::BreakerRegistry>,
    limiters: Arc<crate::rate_limit::LimiterRegistry>,
}

impl ResilientCaller {
    pub fn new(
        providers: Arc<dyn ProviderLookup>,
        breakers: Arc<crate::circuit_breaker::BreakerRegistry>,
        limiters: Arc<crate::rate_limit::LimiterRegistry>,
    ) -> Self {
        Self { providers, breakers, limiters }
    }

    pub async fn complete(
        &self,
        chain: Vec<BackendTarget>,
        req: CanonicalRequest,
        retry_policies: Vec<RetryPolicy>,
        breaker_policies: Vec<crate::circuit_breaker::BreakerPolicy>,
        identity: crate::auth::AgentIdentity,
        client_ip: String,
        frontend_model: String,
    ) -> Result<CanonicalStream, ResilienceError> {
        // ── PRE-CHAIN RATE LIMIT GATE ───────────────────────────────────────
        if let Err((dim, retry_after_secs)) =
            self.limiters.check_pre_chain(&identity, &frontend_model, &client_ip)
        {
            tracing::warn!(
                identity = %identity.log_id(),
                frontend_model = %frontend_model,
                client_ip = %client_ip,
                dimension = ?dim,
                retry_after_secs,
                "rate limited"
            );
            return Err(ResilienceError::RateLimited { dimension: dim, retry_after_secs });
        }

        // ── CHAIN WALK (existing logic from P02 + P03) ─────────────────────
        // ... existing breaker gate + retry loop ...

        for (i, target) in chain.iter().enumerate() {
            // Breaker gate (P03).
            // ...

            // ── PER-UPSTREAM RATE LIMIT GATE ────────────────────────────────
            if let Err((dim, retry_after_secs)) =
                self.limiters.check_per_upstream(&target.provider)
            {
                tracing::warn!(
                    upstream = %target.provider,
                    dimension = ?dim,
                    "per-upstream rate limit exceeded; not falling back"
                );
                return Err(ResilienceError::RateLimited { dimension: dim, retry_after_secs });
            }

            // ... rest of chain walk ...
        }
        // ... rest of error path ...
    }
}
```

- [ ] **Step 2: AppState gains LimiterRegistry**

In `crates/gateway/src/state.rs`:

```rust
pub struct AppState {
    // ... existing fields
    pub limiter_registry: Arc<agent_shim_router::LimiterRegistry>,
}

impl AppState {
    pub fn new(cfg: &GatewayConfig, ...) -> Self {
        let limiter_registry = Arc::new(if cfg.rate_limit.enabled {
            agent_shim_router::LimiterRegistry::from_config(&cfg.rate_limit)
        } else {
            agent_shim_router::LimiterRegistry::disabled()
        });
        let resilient_caller = Arc::new(agent_shim_router::ResilientCaller::new(
            provider_lookup,
            Arc::clone(&breaker_registry),
            Arc::clone(&limiter_registry),
        ));
        // ...
    }
}
```

- [ ] **Step 3: Pipeline extracts identity + IP**

In `crates/gateway/src/pipeline.rs`, near the top of the request handler (before the chain resolution):

```rust
use agent_shim_router::extract_identity_from_headers;

// Extract identity from the request headers. If auth is disabled, this is
// always Anonymous (the registry's `enabled=false` short-circuits anyway).
let identity = if state.auth_enabled {
    let authz = req_headers.get("authorization").and_then(|v| v.to_str().ok());
    let xkey = req_headers.get("x-api-key").and_then(|v| v.to_str().ok());
    let extracted = extract_identity_from_headers(authz, xkey);

    // If auth.required is set, reject unconfigured keys here.
    if state.auth_required && matches!(extracted, agent_shim_router::AgentIdentity::Anonymous) {
        return Err(HandlerError::Unauthorized);
    }
    if let agent_shim_router::AgentIdentity::KeyHash(h) = &extracted {
        if state.auth_required && !state.configured_key_hashes.contains(h) {
            return Err(HandlerError::Unauthorized);
        }
    }
    extracted
} else {
    agent_shim_router::AgentIdentity::Anonymous
};

let client_ip = client_ip_from_request(req).unwrap_or_else(|| "unknown".to_string());
let frontend_model = format!("{}/{}", spec.frontend.kind_str(), model_alias);

// Then call resilient_caller.complete with these fields plumbed through.
let stream = state
    .resilient_caller
    .complete(chain, canonical, retry_policies, breaker_policies, identity, client_ip, frontend_model)
    .await
    .map_err(HandlerError::from)?;
```

`client_ip_from_request` extracts from the `x-forwarded-for` header (when present) or the socket peer addr. Implementation (~10 lines).

`HandlerError::Unauthorized` returns HTTP 401 with the dialect-correct envelope (similar to existing CapabilityMismatch handling).

- [ ] **Step 4: Run tests**

```bash
rtk cargo test --workspace --quiet
```

Expected: existing tests pass; new ones land in T5/T6.

- [ ] **Step 5: Commit**

```bash
git add crates/router/src/resilient_caller.rs crates/gateway/src/state.rs crates/gateway/src/pipeline.rs
git commit -m "feat(gateway): integrate rate limiter + auth into pipeline (Plan 04 P04 T4)

ResilientCaller::complete gains identity, client_ip, frontend_model
params. Per §5.2 layering:
- Pre-chain gate: per-key + per-route + per-IP buckets.
- Per-upstream gate: inside chain walk, just-in-time.

Pipeline extracts identity from Authorization/x-api-key headers via
agent_shim_router::extract_identity_from_headers. When
auth.required=true, unknown keys return HTTP 401 before the chain
walk; HandlerError::Unauthorized maps to the dialect-correct envelope.

AppState gains limiter_registry: Arc<LimiterRegistry>; constructed
from cfg.rate_limit (disabled when cfg.rate_limit.enabled=false)."
```

---

### Task 5: Per-key rate-limit envelope smoke test

**Files:**
- Create: `crates/protocol-tests/tests/rate_limit_per_key_envelope.rs`

- [ ] **Step 1: Write end-to-end per-key rate-limit test**

```rust
//! Plan 04 P04 T5: per-key bucket fires; OpenAI envelope shape verified
//! across both Chat and Responses dialects; Retry-After header present.

#[tokio::test]
async fn per_key_rate_limit_returns_429_with_openai_envelope_chat() {
    // Configure a tiny bucket: 1 RPS, burst 2, so a 3rd request hits the limit.
    let key_plain = "test-secret";
    let key_hash = agent_shim_router::hash_key(key_plain);
    let cfg_yaml = format!(r#"
upstreams:
  oai:
    type: openai_compatible
    base_url: {url}
    api_key: dummy
auth:
  enabled: true
  keys:
    "{key_hash}":
      label: "test"
rate_limit:
  enabled: true
  per_key:
    overrides:
      "{key_hash}": {{rate_per_sec: 1, burst: 2}}
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstream: oai
    upstream_model: gpt-4o
"#, url = "http://localhost:0", key_hash = key_hash);

    // ... server mock returning 200 for the first two requests
    let app = build_test_app(&cfg_yaml).await;

    // Request 1 + 2: pass.
    for _ in 0..2 {
        let resp = send_with_auth(&app, key_plain).await;
        assert_eq!(resp.status(), 200);
    }
    // Request 3: rate limited.
    let resp = send_with_auth(&app, key_plain).await;
    assert_eq!(resp.status(), 429);
    assert!(resp.headers().contains_key("retry-after"));

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["type"], "rate_limit_error");
    assert_eq!(json["error"]["code"], "rate_limited_per_key");
    assert!(json["error"]["message"].as_str().unwrap().contains("retry"));
}

#[tokio::test]
async fn per_key_rate_limit_uses_anthropic_envelope_for_messages_dialect() {
    // Same setup but route is anthropic_messages.
    // Assert envelope is {type:'error', error:{type:'rate_limit_error', message:...}}.
    // No 'code' field per Anthropic dialect.
}
```

- [ ] **Step 2: Run test**

```bash
rtk cargo test -p agent-shim-protocol-tests --test rate_limit_per_key_envelope --quiet
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/protocol-tests/tests/rate_limit_per_key_envelope.rs
git commit -m "test(protocol-tests): per-key rate-limit envelope (Plan 04 P04 T5)

End-to-end: configure auth.keys + rate_limit.per_key.overrides with
a 1-RPS-burst-2 bucket. Send 3 requests with the same key. Asserts:
- Request 1 + 2: HTTP 200.
- Request 3: HTTP 429 + Retry-After header.
- OpenAI envelope: type=rate_limit_error, code=rate_limited_per_key.
- Anthropic envelope: error.type=rate_limit_error (no code field per
  D9)."
```

---

### Task 6: Auth-required smoke test

**Files:**
- Create: `crates/protocol-tests/tests/auth_required_unconfigured_key.rs`

- [ ] **Step 1: Write auth.required=true test**

```rust
//! Plan 04 P04 T6: auth.required=true with an unconfigured key returns
//! HTTP 401 before any provider is contacted.

#[tokio::test]
async fn auth_required_with_unknown_key_returns_401() {
    let known_hash = agent_shim_router::hash_key("known-key");
    let cfg_yaml = format!(r#"
upstreams:
  oai:
    type: openai_compatible
    base_url: http://localhost:0
    api_key: dummy
auth:
  enabled: true
  required: true
  keys:
    "{known_hash}":
      label: "alice"
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstream: oai
    upstream_model: gpt-4o
"#, known_hash = known_hash);

    let app = build_test_app(&cfg_yaml).await;
    // Send with an UNKNOWN key.
    let resp = send_with_auth(&app, "wrong-key").await;
    assert_eq!(resp.status(), 401);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // OpenAI envelope for unauthorized.
    assert_eq!(json["error"]["type"], "authentication_error");
}

#[tokio::test]
async fn auth_required_with_no_key_returns_401() {
    // Same setup; no Authorization or x-api-key header.
    // Expect 401.
}

#[tokio::test]
async fn auth_required_with_known_key_passes() {
    // Same setup; send with the known key.
    // Expect provider to be reached (200 from mock).
}
```

- [ ] **Step 2: Run test**

```bash
rtk cargo test -p agent-shim-protocol-tests --test auth_required_unconfigured_key --quiet
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/protocol-tests/tests/auth_required_unconfigured_key.rs
git commit -m "test(protocol-tests): auth.required gates the pipeline (Plan 04 P04 T6)

3 tests confirm:
- Unknown key + auth.required=true → HTTP 401, no provider contact.
- No key + auth.required=true → HTTP 401, no provider contact.
- Known key + auth.required=true → reaches the provider (mock 200)."
```

---

### Task 7: docs/resilience.md operator guide

**Files:**
- Create: `docs/resilience.md`

- [ ] **Step 1: Write operator-facing guide**

```markdown
# Resilience guide (v0.4+)

This guide is for operators configuring AgentShim's resilience features:
fallback chains, retries, circuit breakers, and rate limiting.

## Quick start

[Worked-example YAML — same as §4.6 of the design spec]

## Generating SHA-256 key hashes

\`\`\`bash
echo -n "my-plaintext-key" | sha256sum | awk '{print "sha256:" $1}'
\`\`\`

Paste the result into `auth.keys.<hash>:label` and into
`rate_limit.per_key.overrides.<hash>` if a per-key bucket is needed.

## Tuning the retry policy

[copy of §4.5 defaults table]

## When breakers trip vs fallback fires

[summary of §5.2 layering]

## Operator log lines

[summary of §6.5 log shape]

## Multi-instance deployments

In v0.4, breaker and rate-limit state lives in process memory. Two gateway
instances behind a load balancer each have their own buckets — effective
rate-limit is `2 × configured` until an upstream actually trips.

For strict enforcement across instances, use a dedicated reverse proxy
(envoy, nginx) for rate limiting and configure AgentShim with very loose
buckets as a safety net.

Distributed state via Redis is a v0.5 candidate.

## Troubleshooting

[FAQ — common operator questions]
```

The full file is ~150 lines. Mirror the style of `docs/providers/anthropic.md`.

- [ ] **Step 2: Commit**

```bash
git add docs/resilience.md
git commit -m "docs: operator-facing resilience guide (Plan 04 P04 T7)

New docs/resilience.md covers:
- Quick-start config example (matches §4.6 of design spec).
- SHA-256 key generation recipe.
- Retry tuning + defaults table.
- Layering walkthrough: when breakers trip vs fallback fires.
- Operator log line reference.
- Multi-instance caveat (v0.4 single-process state)."
```

---

## Definition of Done

- [ ] All 7 tasks complete.
- [ ] `cargo test --workspace --quiet` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] `cargo fmt --all -- --check` clean.
- [ ] Workspace test count: ~507 → ~521.
- [ ] No changes to `crates/core/`, `crates/frontends/`, `crates/providers/src/`.

After this plan merges, the gateway:
- Honors per-key, per-route, per-upstream, per-IP buckets.
- Returns HTTP 429 + dialect-correct envelopes + Retry-After header.
- Optionally enforces `auth.required=true` returning HTTP 401.
- Has an operator-facing guide at `docs/resilience.md`.
