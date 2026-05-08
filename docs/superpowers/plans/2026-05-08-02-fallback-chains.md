# Plan 02 — Fallback Chains + ResilientCaller Skeleton (Phase 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-08-phase-4-resiliency-design.md`](../specs/2026-05-08-phase-4-resiliency-design.md) (decisions D2, D4, §5.2 layering, §6 error envelopes).

**Goal:** Resolver returns `Vec<BackendTarget>` (the full chain). New `ResilientCaller` orchestrator walks the chain on retry exhaustion, composing retry × fallback. End of this plan: a real two-upstream fallback test passes end-to-end through the gateway.

**Architecture:** `Router::resolve` and `ModelResolver::resolve` change return type from `BackendTarget` to `Vec<BackendTarget>`. New `crates/router/src/resilient_caller.rs` owns the chain-walk loop, calling Plan 01's `retry_with_policy` for each chain element. Pipeline replaces its `retry_with_policy(...)` call with a single `resilient_caller.complete(...)` call. New `crates/router/src/errors.rs` defines `ResilienceError`. `HandlerError` gains `NoUpstreamSucceeded` variant mapping to HTTP 503 with the dialect-correct envelope.

**Tech stack:** No new dependencies.

**Frontend changes:** NONE.
**Provider code changes:** NONE.
**Core changes:** NONE.

---

## File Structure

`crates/router/src/`:
- Modify: `static_routes.rs` — `Router::resolve` returns `Vec<BackendTarget>`.
- Modify: `resolver.rs` — `ModelResolver::resolve` returns `Vec<BackendTarget>`; fuzzy upgrade applies element-wise.
- Modify: `lib.rs` — new `Router` trait signature; export `ResilientCaller`, `ResilienceError`.
- Create: `errors.rs` — `ResilienceError` enum.
- Create: `resilient_caller.rs` — orchestrator (retry × fallback only — breakers + rate-limit arrive in P03/P04).

`crates/gateway/src/`:
- Modify: `pipeline.rs` — replace `retry_with_policy` call with `resilient_caller.complete()`.
- Modify: `state.rs` — `AppState` gains `Arc<ResilientCaller>`.
- Modify: `handlers/mod.rs` — `HandlerError::NoUpstreamSucceeded`; HTTP 503 mapping; dialect envelopes.

`crates/protocol-tests/tests/`:
- Create: `responses_fallback_oai_to_anthropic.rs` — cross-protocol fallback smoke.
- Create: `streaming_fallback_pre_stream_only.rs` — pins D4.

---

## Tasks

### Task 1: Router trait returns Vec<BackendTarget>

**Files:**
- Modify: `crates/router/src/lib.rs`
- Modify: `crates/router/src/static_routes.rs`

- [ ] **Step 1: Update Router trait signature**

In `crates/router/src/lib.rs`, change:

```rust
pub trait Router: Send + Sync {
    fn resolve(&self, frontend: FrontendKind, model: &str)
        -> Result<Vec<BackendTarget>, RouteError>;
}
```

(Was returning `Result<BackendTarget, RouteError>`.)

- [ ] **Step 2: Update StaticRouter::resolve**

In `crates/router/src/static_routes.rs`, modify:

```rust
impl Router for StaticRouter {
    fn resolve(&self, frontend: FrontendKind, model: &str) -> Result<Vec<BackendTarget>, RouteError> {
        let key = RouteKey { frontend, model: model.to_string() };
        if let Some(targets) = self.routes.get(&key) {
            return Ok(targets.clone());
        }
        if let Some(wc) = self.wildcards.get(&frontend) {
            let upstream_model = if wc.upstream_model == "*" {
                model.to_string()
            } else {
                wc.upstream_model.clone()
            };
            return Ok(vec![BackendTarget {
                provider: wc.provider.clone(),
                model: upstream_model,
                policy: wc.policy.clone(),
            }]);
        }
        Err(RouteError::NoRoute { frontend, model: model.to_string() })
    }
}
```

The `routes` field changes from `HashMap<RouteKey, BackendTarget>` to `HashMap<RouteKey, Vec<BackendTarget>>`. In `from_config`, when the route uses singular form, build a 1-element vec; when it uses array form, build a vec with one `BackendTarget` per `UpstreamRef`.

- [ ] **Step 3: Update existing StaticRouter tests**

Tests that previously asserted on `target.provider` / `target.model` now assert on `targets[0].provider` / `targets[0].model`. Update the static_routes tests module accordingly.

Add a new test for the array form:

```rust
#[test]
fn array_route_produces_multi_element_chain() {
    let cfg_yaml = r#"
        upstreams:
          openai: {type: openai_compatible, base_url: "https://x", api_key: "x"}
          copilot: {type: github_copilot, credential_path: "/x"}
        routes:
          - frontend: openai_chat
            model: gpt-4o
            upstreams:
              - {name: openai, model: gpt-4o-2024-11-20}
              - {name: copilot, model: gpt-4o}
    "#;
    let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
    let router = StaticRouter::from_config(&cfg);
    let chain = router.resolve(FrontendKind::OpenAiChat, "gpt-4o").unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].provider, "openai");
    assert_eq!(chain[0].model, "gpt-4o-2024-11-20");
    assert_eq!(chain[1].provider, "copilot");
    assert_eq!(chain[1].model, "gpt-4o");
}
```

- [ ] **Step 4: Run tests**

```bash
rtk cargo test -p agent-shim-router --quiet
```

Expected: PASS (existing tests adapted, plus the new array-route test).

- [ ] **Step 5: Commit**

```bash
git add crates/router/src/lib.rs crates/router/src/static_routes.rs
git commit -m "feat(router): Router::resolve returns Vec<BackendTarget> (Plan 04 P02 T1)

Type-level change: every resolver returns the full chain. Singular
RouteEntry config produces a 1-element vec; array form produces N
elements in the configured order.

Tests updated; new array_route_produces_multi_element_chain test
asserts the chain shape end-to-end."
```

---

### Task 2: ModelResolver returns Vec<BackendTarget>

**Files:**
- Modify: `crates/router/src/resolver.rs`

- [ ] **Step 1: Update ModelResolver::resolve**

```rust
pub fn resolve(
    &self,
    frontend: FrontendKind,
    model_alias: &str,
) -> Result<Vec<BackendTarget>, RouteError> {
    let mut chain = self.static_router.resolve(frontend, model_alias)?;
    for target in chain.iter_mut() {
        if let Some(canonical) = self.model_index.resolve(&target.provider, &target.model) {
            if canonical != target.model {
                tracing::info!(
                    requested = %target.model,
                    resolved = %canonical,
                    provider = %target.provider,
                    "fuzzy model match"
                );
                target.model = canonical.to_string();
            }
        }
    }
    Ok(chain)
}
```

- [ ] **Step 2: Update existing tests**

Adapt the existing 4 tests so they call `.unwrap()[0]` for the single-element cases:

```rust
let chain = resolver.resolve(FrontendKind::OpenAiChat, "gpt-4o").unwrap();
assert_eq!(chain.len(), 1);
let target = &chain[0];
assert_eq!(target.provider, "openai");
```

Add a new test for fuzzy upgrade across the chain:

```rust
#[test]
fn fuzzy_upgrade_applies_to_each_chain_element() {
    let cfg_yaml = r#"
        upstreams:
          openai: {type: openai_compatible, base_url: "x", api_key: "x"}
          copilot: {type: github_copilot, credential_path: "x"}
        routes:
          - frontend: openai_chat
            model: gpt-4o
            upstreams:
              - {name: openai, model: gpt-4o}
              - {name: copilot, model: gpt-4o}
    "#;
    let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
    let router: Arc<dyn Router> = Arc::new(StaticRouter::from_config(&cfg));

    let mut map = HashMap::new();
    map.insert("openai".to_string(), {
        let mut s = BTreeSet::new();
        s.insert("gpt-4o-2024-11-20".to_string());
        s
    });
    map.insert("copilot".to_string(), {
        let mut s = BTreeSet::new();
        s.insert("gpt-4o-2024-08-06".to_string());
        s
    });
    let index = Arc::new(ModelIndex::new(map));
    let resolver = ModelResolver::new(router, index);

    let chain = resolver.resolve(FrontendKind::OpenAiChat, "gpt-4o").unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].model, "gpt-4o-2024-11-20"); // openai upgrade
    assert_eq!(chain[1].model, "gpt-4o-2024-08-06"); // copilot upgrade
}
```

- [ ] **Step 3: Run tests**

```bash
rtk cargo test -p agent-shim-router --quiet
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/router/src/resolver.rs
git commit -m "feat(router): ModelResolver::resolve returns Vec<BackendTarget> (Plan 04 P02 T2)

Fuzzy model-name upgrade applies element-wise to each chain element.
Existing 4 tests adapted; new fuzzy_upgrade_applies_to_each_chain_element
covers the multi-element path."
```

---

### Task 3: ResilienceError enum

**Files:**
- Create: `crates/router/src/errors.rs`
- Modify: `crates/router/src/lib.rs`

- [ ] **Step 1: Create errors.rs**

```rust
//! Errors raised by the resilience layer (Plan 04 P02+).
//!
//! These are distinct from `ProviderError` (which describes a single
//! upstream call's outcome) and `RouteError` (which describes resolver
//! failures). `ResilienceError` describes outcomes that involved walking
//! the chain.

use agent_shim_providers::ProviderError;
use thiserror::Error;

/// Per-attempt summary used for operator logging on failure.
#[derive(Debug, Clone)]
pub struct TriedUpstream {
    pub provider: String,
    pub model: String,
    pub attempts: u32,
    pub last_error_tag: String,    // e.g. "upstream_5xx", "network", "decode"
    pub last_error_msg: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Error)]
pub enum ResilienceError {
    /// Every chain element was attempted; every retry budget exhausted; the
    /// most recent error was fallback-eligible (so we walked off the end of
    /// the chain). HTTP 503.
    #[error("no upstream succeeded after trying {} options", tried.len())]
    NoUpstreamSucceeded {
        tried: Vec<TriedUpstream>,
        last_error: ProviderError,
    },

    /// Some chain element returned a terminal error; we stopped without
    /// trying the rest. The HTTP status passes through (typically 4xx from
    /// the provider's `Upstream{status}`).
    #[error("terminal error from upstream: {error}")]
    TerminalError {
        error: ProviderError,
        tried: Vec<TriedUpstream>,
    },

    /// Plan 03 will populate this. Defined here so HandlerError mapping is
    /// stable across plans.
    #[error("all {} upstreams have open circuit breakers", tried.len())]
    AllBreakersOpen { tried: Vec<String> },

    /// Plan 04 will populate this. Defined here for the same reason.
    #[error("rate limited on {dimension:?}; retry after {retry_after_secs}s")]
    RateLimited {
        dimension: RateLimitDimension,
        retry_after_secs: u32,
    },
}

/// Which bucket dimension rejected the request.
#[derive(Debug, Clone, Copy)]
pub enum RateLimitDimension {
    PerKey,
    PerRoute,
    PerUpstream,
    PerIp,
}
```

- [ ] **Step 2: Wire into lib.rs**

```rust
pub mod errors;

pub use errors::{RateLimitDimension, ResilienceError, TriedUpstream};
```

- [ ] **Step 3: Compile & verify**

```bash
rtk cargo build -p agent-shim-router --quiet
```

Expected: clean build.

- [ ] **Step 4: Commit**

```bash
git add crates/router/src/errors.rs crates/router/src/lib.rs
git commit -m "feat(router): ResilienceError enum (Plan 04 P02 T3)

ResilienceError captures the four outcomes a chain walk can produce:
NoUpstreamSucceeded (every chain element exhausted), TerminalError
(short-circuit on a 4xx/Decode/etc.), AllBreakersOpen (Plan 03),
RateLimited (Plan 04). The latter two are defined now so HandlerError
mapping stays stable across plans.

TriedUpstream captures per-attempt summary for operator logs."
```

---

### Task 4: ResilientCaller (retry × fallback)

**Files:**
- Create: `crates/router/src/resilient_caller.rs`
- Modify: `crates/router/src/lib.rs`

- [ ] **Step 1: Write failing test for chain walk**

Create `crates/router/src/resilient_caller.rs`:

```rust
//! Resilient caller orchestrator (Plan 04 P02 T4).
//!
//! Walks a `Vec<BackendTarget>` chain, calling `retry_with_policy` for each
//! element. On retry exhaustion with a fallback-eligible last error,
//! advances to the next chain element. On a terminal error, returns
//! immediately. On success, returns the stream.
//!
//! Plan 03 inserts the breaker gate inside the chain walk.
//! Plan 04 inserts the rate-limit gate before the chain walk.

use std::sync::Arc;
use std::time::Instant;

use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream};
use agent_shim_providers::{BackendProvider, ProviderError};

use crate::errors::{ResilienceError, TriedUpstream};
use crate::fallback::{fallback_eligibility_with_overrides, FallbackEligibility};
use crate::retry::{retry_with_policy, RetryPolicy};

/// Trait abstraction over the provider registry so tests can substitute a
/// mock. The gateway's existing `ProviderRegistry` implements this trivially.
pub trait ProviderLookup: Send + Sync {
    fn get(&self, provider: &str) -> Option<Arc<dyn BackendProvider>>;
}

/// The resilience-layer entry point.
pub struct ResilientCaller {
    providers: Arc<dyn ProviderLookup>,
}

impl ResilientCaller {
    pub fn new(providers: Arc<dyn ProviderLookup>) -> Self {
        Self { providers }
    }

    /// Walk the chain. Each element gets its own retry budget. Fallback
    /// to chain[i+1] only on retry exhaustion with a fallback-eligible
    /// error; terminal errors short-circuit.
    pub async fn complete(
        &self,
        chain: Vec<BackendTarget>,
        req: CanonicalRequest,
        policies: Vec<RetryPolicy>,
    ) -> Result<CanonicalStream, ResilienceError> {
        debug_assert_eq!(chain.len(), policies.len(), "one policy per chain element");
        let mut tried: Vec<TriedUpstream> = Vec::new();
        let mut last_error: Option<ProviderError> = None;

        for (i, target) in chain.iter().enumerate() {
            let policy = &policies[i];
            let provider = match self.providers.get(&target.provider) {
                Some(p) => p,
                None => {
                    let e = ProviderError::UnknownProvider(target.provider.clone());
                    return Err(ResilienceError::TerminalError { error: e, tried });
                }
            };

            let started = Instant::now();
            // retry_with_policy returns Err on retry exhaustion OR terminal.
            let result = retry_with_policy(provider, target.clone(), req.clone(), policy).await;
            let elapsed_ms = started.elapsed().as_millis() as u64;

            match result {
                Ok(stream) => {
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
                    let eligibility = fallback_eligibility_with_overrides(&e, &policy.retry_on);
                    let tag = error_tag(&e);
                    tried.push(TriedUpstream {
                        provider: target.provider.clone(),
                        model: target.model.clone(),
                        attempts: policy.max_attempts, // upper bound; real count
                                                        // is logged by retry loop
                        last_error_tag: tag.to_string(),
                        last_error_msg: e.to_string(),
                        elapsed_ms,
                    });
                    if eligibility == FallbackEligibility::Terminal {
                        tracing::warn!(
                            provider = %target.provider,
                            model = %target.model,
                            chain_position = i,
                            error = %e,
                            "terminal error; not falling back"
                        );
                        return Err(ResilienceError::TerminalError { error: e, tried });
                    }
                    last_error = Some(e);
                    if i + 1 < chain.len() {
                        tracing::warn!(
                            from = %target.provider,
                            to = %chain[i + 1].provider,
                            chain_position = i,
                            "falling back to next upstream"
                        );
                    }
                    // continue to chain[i+1]
                }
            }
        }

        Err(ResilienceError::NoUpstreamSucceeded {
            tried,
            last_error: last_error.expect("loop only exits with last_error set on Err path"),
        })
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
    use agent_shim_core::{
        ContentBlock, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel,
        GenerationOptions, Message, RequestId, ResolvedPolicy, request::RequestMetadata,
    };
    use agent_shim_providers::ProviderCapabilities;
    use async_trait::async_trait;
    use futures::stream;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory ProviderLookup with a name → MockProvider mapping.
    struct InMemoryProviders {
        map: HashMap<String, Arc<dyn BackendProvider>>,
    }

    impl ProviderLookup for InMemoryProviders {
        fn get(&self, name: &str) -> Option<Arc<dyn BackendProvider>> {
            self.map.get(name).cloned()
        }
    }

    /// Scripted MockProvider — returns one result per call.
    struct MockProvider {
        name: &'static str,
        scripted: Mutex<Vec<Result<(), ProviderError>>>,
        capabilities: ProviderCapabilities,
    }

    impl MockProvider {
        fn new(name: &'static str, scripted: Vec<Result<(), ProviderError>>) -> Arc<Self> {
            Arc::new(Self {
                name,
                scripted: Mutex::new(scripted),
                capabilities: ProviderCapabilities::default(),
            })
        }
    }

    #[async_trait]
    impl BackendProvider for MockProvider {
        fn name(&self) -> &'static str { self.name }
        fn capabilities(&self) -> &ProviderCapabilities { &self.capabilities }
        async fn complete(
            &self,
            _req: CanonicalRequest,
            _target: BackendTarget,
        ) -> Result<CanonicalStream, ProviderError> {
            let mut s = self.scripted.lock().unwrap();
            if s.is_empty() {
                return Err(ProviderError::Network("script exhausted".into()));
            }
            match s.remove(0) {
                Ok(()) => Ok(Box::pin(stream::iter(vec![]))),
                Err(e) => Err(e),
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

    fn fast_policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            initial_backoff_ms: 1,
            multiplier: 2.0,
            jitter_pct: 0.0,
            total_budget_ms: 1000,
            retry_on: vec!["network".into(), "upstream_5xx".into(), "upstream_429".into()],
        }
    }

    fn target(provider: &str) -> BackendTarget {
        BackendTarget {
            provider: provider.into(),
            model: "gpt-4o".into(),
            policy: Default::default(),
        }
    }

    #[tokio::test]
    async fn returns_first_chain_element_on_success() {
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([
                ("a".to_string(), MockProvider::new("a", vec![Ok(())]) as Arc<_>),
            ]),
        });
        let caller = ResilientCaller::new(providers);
        let chain = vec![target("a")];
        let policies = vec![fast_policy(2)];
        let result = caller.complete(chain, dummy_request(), policies).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn falls_back_on_5xx_then_succeeds() {
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([
                ("a".to_string(),
                 MockProvider::new("a", vec![
                    Err(ProviderError::Upstream { status: 502, body: "x".into() }),
                    Err(ProviderError::Upstream { status: 502, body: "x".into() }),
                 ]) as Arc<_>),
                ("b".to_string(),
                 MockProvider::new("b", vec![Ok(())]) as Arc<_>),
            ]),
        });
        let caller = ResilientCaller::new(providers);
        let chain = vec![target("a"), target("b")];
        let policies = vec![fast_policy(2), fast_policy(2)];
        let result = caller.complete(chain, dummy_request(), policies).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn does_not_fallback_on_terminal_4xx() {
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([
                ("a".to_string(),
                 MockProvider::new("a", vec![
                    Err(ProviderError::Upstream { status: 401, body: "auth".into() }),
                 ]) as Arc<_>),
                ("b".to_string(),
                 MockProvider::new("b", vec![Ok(())]) as Arc<_>),
            ]),
        });
        let caller = ResilientCaller::new(providers);
        let chain = vec![target("a"), target("b")];
        let policies = vec![fast_policy(2), fast_policy(2)];
        let result = caller.complete(chain, dummy_request(), policies).await;
        assert!(matches!(result, Err(ResilienceError::TerminalError { .. })));
    }

    #[tokio::test]
    async fn no_upstream_succeeded_when_chain_exhausts() {
        let providers: Arc<dyn ProviderLookup> = Arc::new(InMemoryProviders {
            map: HashMap::from([
                ("a".to_string(),
                 MockProvider::new("a", vec![
                    Err(ProviderError::Network("a-down".into())),
                    Err(ProviderError::Network("a-down".into())),
                 ]) as Arc<_>),
                ("b".to_string(),
                 MockProvider::new("b", vec![
                    Err(ProviderError::Network("b-down".into())),
                    Err(ProviderError::Network("b-down".into())),
                 ]) as Arc<_>),
            ]),
        });
        let caller = ResilientCaller::new(providers);
        let chain = vec![target("a"), target("b")];
        let policies = vec![fast_policy(2), fast_policy(2)];
        let result = caller.complete(chain, dummy_request(), policies).await;
        match result {
            Err(ResilienceError::NoUpstreamSucceeded { tried, .. }) => {
                assert_eq!(tried.len(), 2);
                assert_eq!(tried[0].provider, "a");
                assert_eq!(tried[1].provider, "b");
            }
            other => panic!("expected NoUpstreamSucceeded, got {other:?}"),
        }
    }
}
```

- [ ] **Step 2: Wire into lib.rs**

```rust
pub mod resilient_caller;

pub use resilient_caller::{ProviderLookup, ResilientCaller};
```

- [ ] **Step 3: Run tests**

```bash
rtk cargo test -p agent-shim-router --quiet
```

Expected: 4 new resilient_caller tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/router/src/resilient_caller.rs crates/router/src/lib.rs
git commit -m "feat(router): ResilientCaller orchestrator (Plan 04 P02 T4)

Walks a Vec<BackendTarget> chain via retry_with_policy per element.
On retry exhaustion: classifies the last error and falls back to
chain[i+1] (Eligible) or returns TerminalError (Terminal). On success:
returns immediately with the stream.

ProviderLookup trait abstracts the provider registry for test
substitutability. Plan 03 inserts the breaker gate; Plan 04 inserts
the rate-limit gate.

4 unit tests cover: success-on-first, fallback-on-5xx, no-fallback-on-4xx,
chain-exhaustion-NoUpstreamSucceeded."
```

---

### Task 5: HandlerError mapping + dialect envelopes

**Files:**
- Modify: `crates/gateway/src/handlers/mod.rs`
- Modify: `crates/gateway/src/state.rs`

- [ ] **Step 1: Extend HandlerError**

In `crates/gateway/src/handlers/mod.rs`, add variants and HTTP mapping. Find the existing `HandlerError` enum and add:

```rust
pub enum HandlerError {
    // ... existing variants
    NoUpstreamSucceeded {
        last_error: ProviderError,
        tried_count: usize,
    },
    TerminalUpstream {
        error: ProviderError,
    },
    AllBreakersOpen {
        tried_count: usize,
    },
    RateLimited {
        dimension: agent_shim_router::RateLimitDimension,
        retry_after_secs: u32,
    },
}
```

The `From<agent_shim_router::ResilienceError>` impl is the bridge:

```rust
impl From<agent_shim_router::ResilienceError> for HandlerError {
    fn from(e: agent_shim_router::ResilienceError) -> Self {
        use agent_shim_router::ResilienceError;
        match e {
            ResilienceError::NoUpstreamSucceeded { tried, last_error } => {
                HandlerError::NoUpstreamSucceeded {
                    last_error,
                    tried_count: tried.len(),
                }
            }
            ResilienceError::TerminalError { error, .. } => {
                HandlerError::TerminalUpstream { error }
            }
            ResilienceError::AllBreakersOpen { tried } => {
                HandlerError::AllBreakersOpen { tried_count: tried.len() }
            }
            ResilienceError::RateLimited { dimension, retry_after_secs } => {
                HandlerError::RateLimited { dimension, retry_after_secs }
            }
        }
    }
}
```

- [ ] **Step 2: HTTP status mapping**

Find the existing `HandlerError::status_code()` (or similar) method and add:

```rust
match self {
    HandlerError::NoUpstreamSucceeded { .. } | HandlerError::AllBreakersOpen { .. } => {
        StatusCode::SERVICE_UNAVAILABLE
    }
    HandlerError::TerminalUpstream { error: ProviderError::Upstream { status, .. } } => {
        StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
    }
    HandlerError::TerminalUpstream { error: _ } => StatusCode::BAD_GATEWAY,
    HandlerError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
    // ... existing arms
}
```

- [ ] **Step 3: Dialect envelopes**

Find the existing JSON-body builder (per-dialect) and extend. For OpenAI Chat / OpenAI Responses (shared envelope):

```rust
fn openai_envelope(handler_error: &HandlerError) -> serde_json::Value {
    use serde_json::json;
    match handler_error {
        HandlerError::NoUpstreamSucceeded { last_error, tried_count } => {
            json!({"error": {
                "message": format!("All {tried_count} upstreams failed: {last_error}"),
                "type": "service_unavailable_error",
                "code": "no_upstream_available",
            }})
        }
        HandlerError::AllBreakersOpen { tried_count } => {
            json!({"error": {
                "message": format!("All {tried_count} upstreams temporarily unavailable (circuit breakers open)"),
                "type": "service_unavailable_error",
                "code": "all_breakers_open",
            }})
        }
        HandlerError::RateLimited { dimension, retry_after_secs } => {
            let (code, dimension_msg) = match dimension {
                RateLimitDimension::PerKey => ("rate_limited_per_key", "Per-API-key"),
                RateLimitDimension::PerRoute => ("rate_limited_per_route", "Route"),
                RateLimitDimension::PerUpstream => ("rate_limited_per_upstream", "Upstream"),
                RateLimitDimension::PerIp => ("rate_limited_per_ip", "Per-IP"),
            };
            json!({"error": {
                "message": format!("{dimension_msg} rate limit exceeded; retry after {retry_after_secs}s"),
                "type": "rate_limit_error",
                "code": code,
            }})
        }
        // ... existing arms
    }
}
```

For Anthropic Messages dialect (no `code` field):

```rust
fn anthropic_envelope(handler_error: &HandlerError) -> serde_json::Value {
    use serde_json::json;
    match handler_error {
        HandlerError::NoUpstreamSucceeded { .. } | HandlerError::AllBreakersOpen { .. } => {
            json!({"type": "error", "error": {
                "type": "overloaded_error",
                "message": handler_error.to_string(),
            }})
        }
        HandlerError::RateLimited { .. } => {
            json!({"type": "error", "error": {
                "type": "rate_limit_error",
                "message": handler_error.to_string(),
            }})
        }
        // ... existing arms
    }
}
```

- [ ] **Step 4: Retry-After header on RateLimited responses**

In the response builder, when the variant is `RateLimited`, add the header:

```rust
if let HandlerError::RateLimited { retry_after_secs, .. } = &self.handler_error {
    response.headers_mut().insert(
        "Retry-After",
        retry_after_secs.to_string().parse().unwrap(),
    );
}
```

- [ ] **Step 5: Unit-test the mapping**

Add tests in `crates/gateway/src/handlers/mod.rs` or a new `crates/gateway/tests/error_envelopes.rs`:

```rust
#[test]
fn no_upstream_succeeded_maps_to_503_with_openai_envelope() {
    let err = HandlerError::NoUpstreamSucceeded {
        last_error: ProviderError::Upstream { status: 502, body: "x".into() },
        tried_count: 3,
    };
    assert_eq!(err.status_code(), StatusCode::SERVICE_UNAVAILABLE);
    let body = openai_envelope(&err);
    assert_eq!(body["error"]["type"], "service_unavailable_error");
    assert_eq!(body["error"]["code"], "no_upstream_available");
}

#[test]
fn rate_limited_maps_to_429_with_dimension_in_code() {
    let err = HandlerError::RateLimited {
        dimension: RateLimitDimension::PerKey,
        retry_after_secs: 30,
    };
    assert_eq!(err.status_code(), StatusCode::TOO_MANY_REQUESTS);
    let body = openai_envelope(&err);
    assert_eq!(body["error"]["code"], "rate_limited_per_key");
}
```

- [ ] **Step 6: Run tests**

```bash
rtk cargo test -p agent-shim --quiet
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/gateway/src/handlers/mod.rs crates/gateway/src/state.rs
git commit -m "feat(gateway): HandlerError gains resilience variants (Plan 04 P02 T5)

Maps ResilienceError variants to HandlerError + HTTP responses per
§5.2 + §6.3 of the Phase 4 design:

- NoUpstreamSucceeded → HTTP 503, OpenAI envelope
  {error.type: 'service_unavailable_error', code: 'no_upstream_available'};
  Anthropic {error.type: 'overloaded_error'}.
- AllBreakersOpen → HTTP 503, code: 'all_breakers_open'.
- RateLimited → HTTP 429, code: 'rate_limited_<dimension>',
  Retry-After header from retry_after_secs.
- TerminalUpstream → passes through upstream's HTTP status (4xx).

AllBreakersOpen + RateLimited variants are present but never produced
in P02; P03/P04 wire them. Unit tests cover the OpenAI envelope shape."
```

---

### Task 6: Pipeline integration + cross-protocol smoke tests

**Files:**
- Modify: `crates/gateway/src/pipeline.rs`
- Modify: `crates/gateway/src/state.rs`
- Create: `crates/protocol-tests/tests/responses_fallback_oai_to_anthropic.rs`
- Create: `crates/protocol-tests/tests/streaming_fallback_pre_stream_only.rs`

- [ ] **Step 1: Wire ResilientCaller into AppState**

In `crates/gateway/src/state.rs`, add a `ResilientCaller` to `AppState`:

```rust
pub struct AppState {
    // ... existing fields
    pub resilient_caller: Arc<agent_shim_router::ResilientCaller>,
}
```

In the `AppState::new` constructor, build the caller:

```rust
let provider_lookup: Arc<dyn agent_shim_router::ProviderLookup> = providers.clone();
let resilient_caller = Arc::new(agent_shim_router::ResilientCaller::new(provider_lookup));
```

The existing `ProviderRegistry` should `impl ProviderLookup` (a 5-line impl: `fn get(&self, name) -> Option<Arc<dyn BackendProvider>> { self.map.get(name).cloned() }`).

- [ ] **Step 2: Replace pipeline retry_with_policy call with caller.complete**

In `crates/gateway/src/pipeline.rs`, replace the P01 `retry_with_policy(...)` call with:

```rust
// Build per-chain-element retry policies (same RetryConfig applies to every
// element under D2; the route's retry block governs the whole chain). P03
// extends this to thread breaker config.
let chain = state
    .resolver
    .resolve(spec.frontend.kind(), &model_alias)
    .map_err(|_| HandlerError::NoRoute)?;
let policies: Vec<agent_shim_router::RetryPolicy> = chain
    .iter()
    .map(|_| agent_shim_router::RetryPolicy::from(
        state.static_routes.find_retry_policy(spec.frontend.kind(), &model_alias)
            .as_ref()
            .unwrap_or(&agent_shim_config::RetryConfig::default())
    ))
    .collect();

let stream = state
    .resilient_caller
    .complete(chain, canonical, policies)
    .await
    .map_err(HandlerError::from)?;
```

- [ ] **Step 3: Cross-protocol fallback test (Plan 02 T6)**

Create `crates/protocol-tests/tests/responses_fallback_oai_to_anthropic.rs`:

```rust
//! Plan 04 P02 T6: cross-protocol fallback smoke.
//!
//! Mockito serves 502 from upstream A (OAI-compat), 200 + Anthropic SSE
//! from upstream B. Pipeline sees the 502, retries+fails, falls back to B,
//! and the client receives B's response in OpenAI Responses event shape.

use agent_shim_core::FrontendKind;
use agent_shim_frontends::{openai_responses::OpenAiResponses, FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{
    collect_sse, make_anthropic_provider, make_canonical_request, TEST_CLOCK,
};
// ... full imports

const ANTHROPIC_TEXT_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_b\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-4-7\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":7,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi from B\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":3}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

#[tokio::test]
async fn falls_back_from_oai_compat_to_anthropic_when_oai_returns_502() {
    let mut server_a = mockito::Server::new_async().await;
    let _bad_a = server_a
        .mock("POST", "/v1/chat/completions")
        .with_status(502)
        .with_body("Bad Gateway")
        .expect_at_least(1)
        .create_async()
        .await;

    let mut server_b = mockito::Server::new_async().await;
    let _good_b = server_b
        .mock("POST", "/v1/messages")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(ANTHROPIC_TEXT_SSE)
        .expect(1)
        .create_async()
        .await;

    // Build a gateway with a 2-element chain: oai_compat → anthropic.
    // ... (use the same build_test_app helper from P01 T5 retry_smoke,
    //      with config that has both upstreams configured and a single
    //      route with frontend openai_responses, model gpt-4o, upstreams
    //      array of [oai, anthropic]).

    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"model":"gpt-4o","input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}],"stream":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body_str = String::from_utf8(body.to_vec()).unwrap();
    assert!(body_str.contains("event: response.completed"));
    assert!(body_str.contains("hi from B"));  // came from upstream B
}
```

- [ ] **Step 4: Streaming pre-stream-only test (D4 regression)**

Create `crates/protocol-tests/tests/streaming_fallback_pre_stream_only.rs`:

```rust
//! Plan 04 P02 T6: pins D4. Once chain[0] returns Ok(stream) and bytes
//! flow, mid-stream failures surface as stream errors; chain[1] is NEVER
//! called. The mock asserts `expect(0)` on chain[1] to lock this in.

#[tokio::test]
async fn mid_stream_failure_does_not_trigger_fallback() {
    let mut server_a = mockito::Server::new_async().await;
    let _truncated = server_a
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        // SSE that opens cleanly but cuts off mid-stream (no DONE marker,
        // body simply ends).
        .with_body("data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"par\"}}]}\n\n")
        .expect(1)
        .create_async()
        .await;

    let mut server_b = mockito::Server::new_async().await;
    let _b_unused = server_b
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body("never-called")
        .expect(0)  // ← the contract: B is NEVER called once A starts streaming
        .create_async()
        .await;

    // Build app with chain [a, b], both OAI-compat upstreams.
    let app = build_test_app(/* ... */);
    let response = app.oneshot(/* streaming POST */).await.unwrap();

    // Status is 200 because A returned 200 + stream. The truncation
    // surfaces in the stream body but doesn't switch to B.
    assert_eq!(response.status(), 200);

    // mock_b's expect(0) verification fires automatically when the test ends.
}
```

- [ ] **Step 5: Run all tests**

```bash
rtk cargo test --workspace --quiet
```

Expected: all tests PASS, including the two new cross-protocol smokes.

- [ ] **Step 6: Commit**

```bash
git add crates/gateway/src/pipeline.rs crates/gateway/src/state.rs crates/protocol-tests/tests/responses_fallback_oai_to_anthropic.rs crates/protocol-tests/tests/streaming_fallback_pre_stream_only.rs
git commit -m "feat(gateway): wire ResilientCaller into pipeline + fallback smokes (Plan 04 P02 T6)

Pipeline replaces the P01 retry_with_policy call with a single
ResilientCaller::complete invocation. AppState gains resilient_caller:
Arc<ResilientCaller>; ProviderRegistry gains a 5-line ProviderLookup
impl.

Cross-protocol smoke tests:
- responses_fallback_oai_to_anthropic.rs (T6): chain [oai, anthropic].
  oai returns 502, anthropic returns 200 + Anthropic SSE. Client
  receives Anthropic's response in OpenAI Responses event shape.
- streaming_fallback_pre_stream_only.rs (T6): chain [a, b]. a returns
  200 + truncated stream; mock_b.expect(0) verifies b is NEVER called
  once a starts streaming. Pins D4 in regression coverage.

Workspace tests: ~485 → ~497."
```

---

## Definition of Done

- [ ] All 6 tasks complete.
- [ ] `cargo test --workspace --quiet` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] `cargo fmt --all -- --check` clean.
- [ ] Workspace test count: ~485 → ~497.
- [ ] No changes to `crates/core/`, `crates/frontends/`, `crates/providers/src/`.
- [ ] Existing v0.3 single-upstream configs continue to work unchanged.

After this plan merges, the gateway:
- Resolves the full chain of upstreams.
- Walks the chain on retry exhaustion.
- Returns proper HTTP 503 with dialect-correct envelopes when every chain element fails.
- Honors D4: never falls back once streaming has started.
