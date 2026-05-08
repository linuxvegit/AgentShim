# Phase 4 (v0.4) — Resilient gateway: fallback, retries, circuit breakers, rate limiting

**Status:** Draft (ready for grilling)
**Date:** 2026-05-08
**Source:** [`2026-04-28-agent-shim-design.md`](./2026-04-28-agent-shim-design.md) §9 Phase 4

---

## 1. Scope

Phase 4 closes what Phase 3's CHANGELOG promised as deferred items: the
gateway becomes **resilient** as well as protocol-translating. Four
subsystems compose into a single orchestration layer that sits between the
existing `ModelResolver` and the existing `BackendProvider` trait, with no
changes to the canonical model, no changes to frontends, and no provider
code changes.

**In scope (v0.4.0):**

- **Per-route upstream chains.** Existing `upstream`/`upstream_model` (singular)
  shape coexists with a new `upstreams: [...]` array shape. Operators express
  fallback intent locally on each route.
- **Per-route retry policy.** Exponential backoff + jitter + total-time budget.
  Defaults: 2 attempts, 100ms initial, 2.0× multiplier, ±25% jitter, 5000ms
  total budget. Retries within a single upstream before falling back.
- **Per-(upstream, model) circuit breakers.** Sliding-window failure-rate
  trip policy. Defaults: 50% failure rate over 20+ requests in 60s, 30s open
  cooldown, single half-open probe.
- **Pre-stream fallback.** Once the gateway commits to streaming bytes to
  the client, fallback is impossible. Mid-stream failures surface as
  stream-level errors, matching Phase 3's existing behavior.
- **Token-bucket rate limiting** on four dimensions: per API key, per route,
  per upstream, per source IP. Independent buckets — a request must satisfy
  all applicable buckets to pass.
- **API key extraction** from `Authorization: Bearer <key>` and
  `x-api-key: <key>`. Keys are SHA-256 hashed before lookup; configs store
  hashes only. Anonymous requests use a dedicated bucket.
- **Tracing logs** at info/warn level for every retry attempt, fallback
  transition, breaker state change, and a per-request summary at request
  end. Metrics ship in Phase 5.
- **Frozen-core invariant resumes.** Plan files declare
  `core changes: NONE`. ADR-0003 remains the bounded one-time exception.

**Permanently out** (per parent spec §11): embeddings, moderation, audio I/O,
admin UI, end-user identity, billing, multi-account Copilot, OAuth Anthropic.

**Deferred to v0.4.x or v0.5+:**

- Distributed/shared state (Redis backend) — v0.5 candidate when
  multi-instance deployment is the operator default.
- Cost/latency-aware routing — v0.5+.
- Request budget caps (per-key per-day token/cost limits) — v0.5.
- Prometheus metrics, hot-reload, OpenTelemetry — Phase 5.

---

## 2. Locked Decisions

The twelve decisions that shape Phase 4.

### D1. All four subsystems ship in v0.4.0

Fallback, retries, breakers, and rate limiting all land in one minor.
Decomposed into 5 plans (§6) for incremental review and shippability,
but the release tag waits for the full set.

### D2. Per-route ordered `upstreams` array; both shapes coexist forever

Each route lists its primary and backups in order:

```yaml
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstreams:
      - {name: openai, model: gpt-4o-2024-11-20}
      - {name: copilot, model: gpt-4o}
      - {name: deepseek, model: deepseek-v3}
```

The existing v0.3 singular `upstream`/`upstream_model` shape continues to
work — internally it deserializes to a 1-element vec. Validation rejects
configs that supply both shapes on the same route.

### D3. Industry-standard fallback triggers, with per-route override

Default `retry_on` set: `network`, `upstream_5xx`, `upstream_429`. Other
errors (4xx, decode, encode, capability mismatch) are terminal — no
fallback. Operators override per route via `retry.retry_on: [...]`.

### D4. Pre-stream fallback only

Fallback fires only before the gateway has committed any response bytes
to the client. Once `provider.complete()` returns `Ok(stream)` and bytes
flow, mid-stream failures surface as stream-level errors. This matches
existing v0.3 streaming-error behavior; Phase 4 does not introduce
buffering. Bounded buffered fallback was rejected (alt A4 in §7).

### D5. Exponential backoff + jitter + total-time budget for retries

Retries within one upstream use exponential backoff:

```
backoff_n = initial_backoff_ms × multiplier^(n-1) × (1 ± jitter_pct/100)
```

A `total_budget_ms` caps wall-clock time spent on retries against a single
upstream — when the budget is exhausted, fallback fires regardless of
remaining attempts. Defaults are tuned for the typical case (small N,
short total wall-clock).

### D6. Per-(upstream, model) breaker scope

Breaker state is keyed by `(provider_name, model)`. A misconfigured model
on a healthy provider trips its own breaker without affecting other
models on the same provider.

### D7. Sliding-window failure-rate breaker policy

Breaker trips when the failure rate over the most recent
`window_secs` (default 60) exceeds `failure_threshold_pct` (default 50%)
across at least `min_requests` (default 20). Open state holds for
`open_cooldown_secs` (default 30) before transitioning to half-open. A
single probe in half-open state determines whether to close (success) or
re-open with fresh cooldown (failure).

### D8. Token bucket on four dimensions

Independent buckets per:
- **Per API key.** Most common operator need ("limit caller foo to 100 RPM").
- **Per route.** Limit RPM hitting a specific `(frontend, model)` regardless
  of caller.
- **Per upstream.** Limit RPM crossing into a specific upstream provider.
- **Per source IP.** Catches anonymous abuse without API key auth.

A request must satisfy **all applicable buckets**. The first to reject
names the dimension in the error.

### D9. Hashed API keys in standard auth headers

Callers send `Authorization: Bearer <plaintext>` or `x-api-key: <plaintext>`.
The gateway hashes the key with SHA-256 and looks up the hash in
`auth.keys.<sha256:hex>`. Configs store hashes only — never plaintext.
Operators generate hashes with `echo -n "<plaintext>" | sha256sum`.

When `auth.enabled=false`, header inspection is skipped (zero overhead).
When `auth.enabled=true` and `auth.required=false`, requests without a
known key are tagged as `Anonymous` and use the dedicated anonymous bucket.
When `auth.required=true`, unknown keys produce HTTP 401 before any
upstream contact.

### D10. In-memory state only

Breaker state and rate-limit buckets live in the gateway process's memory.
Reset on restart. Multi-instance deployments lose strict enforcement
(each instance has its own buckets) — this is acceptable for v0.4 single-
instance deployments and documented as a known limitation. Distributed
state is a Phase 5 candidate.

### D11. Tracing logs only — no metrics

Structured tracing logs at info/warn level for every retry, fallback
transition, and breaker state change. Per-request summary log at request
end with the full chain walk. Prometheus metrics, OpenTelemetry, and
hot-reload all ship in Phase 5.

### D12. Default `retry.max_attempts: 2` (small behavior change on upgrade)

Routes that don't specify a `retry` block get `max_attempts: 2` by default.
This is a behavior change from v0.3 (which had no retry at all). The change
is small (one extra round-trip on transient errors), positive (more requests
succeed), and called out in CHANGELOG. Operators wanting strict v0.3
behavior set `retry: {max_attempts: 1}` per route.

---

## 3. Module Layout

The whole phase lives in `crates/router/`, `crates/config/`, and
`crates/gateway/`. **No changes** to `crates/core/`, `crates/frontends/`,
or `crates/providers/src/`. Provider docs change in Plan 01.

```
crates/config/src/
  schema.rs            # MODIFY: RouteEntry.upstreams array; retry/breaker
                       #         per-route blocks; top-level auth and
                       #         rate_limit blocks.
  validation.rs        # MODIFY: 10 new validation rules (§3.4 of design).

crates/router/src/
  lib.rs               # MODIFY: re-export ResilientCaller, AuthExtractor.
  static_routes.rs     # MODIFY: StaticRouter::resolve returns Vec<BackendTarget>.
  resolver.rs          # MODIFY: ModelResolver::resolve returns Vec<BackendTarget>.
  retry.rs             # NEW: RetryPolicy, compute_backoff, retry loop.
  fallback.rs          # IMPLEMENT (was stub): chain walker, error classifier.
  circuit_breaker.rs   # IMPLEMENT (was stub): sliding-window state machine,
                       #      BreakerRegistry.
  rate_limit.rs        # IMPLEMENT (was stub): TokenBucket per dimension,
                       #      LimiterRegistry.
  auth.rs              # NEW: header parsing, SHA-256 hash, AgentIdentity.
  resilient_caller.rs  # NEW: orchestrator composing the four subsystems.
  errors.rs            # NEW: ResilienceError variants.

crates/gateway/src/
  pipeline.rs          # MODIFY: call resilient_caller.complete() instead of
                       #      provider.complete() directly.
  state.rs             # MODIFY: AppState gains ResilientCaller, registries.
  handlers/mod.rs      # MODIFY: HandlerError gains RateLimited /
                       #      NoUpstreamSucceeded / AllBreakersOpen variants.

crates/router/Cargo.toml
                       # MODIFY: add `governor` (token-bucket math),
                       #         `sha2` (key hashing).

docs/
  adr/0004-resilient-caller.md        # NEW: records §3 architecture choice.
  providers/{anthropic,openai-compatible,gemini,deepseek}.md
                                      # MODIFY: each gains a "Resilience
                                      # behavior" subsection.
  architecture.md                     # MODIFY: capability matrix gains
                                      # Resilience row; perf overhead notes.
  resilience.md                       # NEW: operator-facing guide.

CHANGELOG.md                          # NEW [0.4.0] entry.
README.md                             # MODIFY: capability matrix v0.4;
                                      # "What's NOT in v0.4"; releases v0.4.0.
Cargo.toml                            # MODIFY: 0.4.0-dev → 0.4.0 (Plan 05 T5).
```

**Frozen-core verification at every plan close:**
`git diff master..HEAD -- crates/core/` must be empty for every Phase 4
plan merge. Final-review step at each plan checks.

---

## 4. Configuration Schema

The most concrete part of the design — what operators write in
`gateway.yaml` after v0.4.0.

### 4.1 Route entry — new `upstreams` array form (D2)

Existing v0.3 shape (continues to work):

```yaml
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstream: openai
    upstream_model: gpt-4o-2024-11-20
    reasoning_effort: medium
    anthropic_beta: context-1m-2025-08-07
```

New v0.4 array shape (preferred for new configs):

```yaml
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstreams:
      - {name: openai, model: gpt-4o-2024-11-20}
      - {name: copilot, model: gpt-4o}
      - {name: deepseek, model: deepseek-v3}
    reasoning_effort: medium
    anthropic_beta: context-1m-2025-08-07
    retry:
      max_attempts: 3
      initial_backoff_ms: 100
      multiplier: 2.0
      jitter_pct: 25
      total_budget_ms: 5000
      retry_on:
        - network
        - upstream_5xx
        - upstream_429
    breaker:
      enabled: true
      failure_threshold_pct: 50
      min_requests: 20
      window_secs: 60
      open_cooldown_secs: 30
```

**Internal representation.** Both shapes produce a
`RouteEntry { upstreams: Vec<UpstreamRef>, ... }`. Singular form is sugar
for a 1-element vec. `validation.rs` rejects mixed configs.

### 4.2 Top-level `rate_limit` block (D8)

```yaml
rate_limit:
  enabled: true            # default false; opt-in for v0.4.0
  per_key:
    default:               # applied to known keys without an override
      rate_per_sec: 10
      burst: 30
    overrides:             # keyed by hashed API key (sha256:<hex>)
      sha256:abc123...:
        rate_per_sec: 100
        burst: 300
    anonymous:             # applies when no key was presented
      rate_per_sec: 1
      burst: 5
  per_route:               # keyed by "<frontend>/<model>"
    "openai_chat/gpt-4o":
      rate_per_sec: 50
      burst: 100
  per_upstream:            # keyed by upstream name
    openai:
      rate_per_sec: 200
      burst: 500
  per_ip:
    enabled: false         # default false
    rate_per_sec: 5
    burst: 20
```

A request must satisfy **all applicable buckets**. The first to reject
produces 429 with that dimension named.

### 4.3 Top-level `auth` block (D9)

```yaml
auth:
  enabled: false           # default false; per-key rate-limiting needs true
  required: false          # default false; when true, unknown keys → HTTP 401
  keys:                    # keyed by SHA-256 hash of plaintext
    sha256:abc123...:
      label: "alice-ci"    # opaque identifier for logs
    sha256:def456...:
      label: "bob-prod"
```

When `auth.enabled=false`, the gateway behaves exactly as v0.3.
When `auth.enabled=true, required=false`, unknown keys are tagged
`Anonymous`. When `required=true`, unknown keys → HTTP 401.

### 4.4 Validation rules

Centralized in `validation.rs`. Each has a unit test in Plan 01 T1.

1. A route MUST have either singular OR array form, never both.
2. Every `upstreams[].name` MUST reference a configured upstream block.
3. `retry.max_attempts >= 1`. `retry.total_budget_ms >= initial_backoff_ms`.
4. `retry.multiplier > 1.0`.
5. `breaker.failure_threshold_pct ∈ [1, 100]`.
6. `breaker.min_requests >= 1`.
7. Every `rate_limit.per_route.<key>` references an existing route.
8. Every `rate_limit.per_upstream.<name>` references a configured upstream.
9. `rate_limit` keys in `per_key.overrides` MUST start with `sha256:`.
10. `rate_per_sec > 0` and `burst >= 1` for every bucket.

### 4.5 Defaults rationale

| Knob | Default | Rationale |
|---|---|---|
| `retry.max_attempts` | 2 | Average request retries once; total wall-clock under 1s typical. |
| `retry.initial_backoff_ms` | 100 | Short enough to be unnoticed; long enough for upstream recovery. |
| `retry.multiplier` | 2.0 | Standard exponential. |
| `retry.jitter_pct` | 25 | Avoids thundering-herd at fixed offsets. |
| `retry.total_budget_ms` | 5000 | Caps long backoff series under user HTTP timeouts. |
| `breaker.failure_threshold_pct` | 50 | Conservative — half the requests must fail. |
| `breaker.min_requests` | 20 | Low-traffic routes never trip from a single bad request. |
| `breaker.window_secs` | 60 | Short enough to catch fast failures, long enough to absorb blips. |
| `breaker.open_cooldown_secs` | 30 | Aligns with most upstream auto-healing windows. |

---

## 5. ResilientCaller

The orchestrator the pipeline calls in place of `provider.complete(...)`.

### 5.1 Public surface

```rust
// crates/router/src/resilient_caller.rs

pub struct ResilientCaller {
    breakers: Arc<BreakerRegistry>,
    limiters: Arc<LimiterRegistry>,
    providers: Arc<ProviderRegistry>,
    clock: Arc<dyn Clock>,
}

impl ResilientCaller {
    pub async fn complete(
        &self,
        chain: Vec<BackendTarget>,
        req: CanonicalRequest,
        identity: AgentIdentity,
        client_ip: IpAddr,
    ) -> Result<CanonicalStream, ResilienceError>;
}
```

### 5.2 Layering order (the contract)

```
1. RATE-LIMIT GATE (per-request, never retried)
   ├─ check per-key bucket (or anonymous bucket)
   ├─ check per-route bucket
   ├─ check per-IP bucket (if enabled)
   └─ on rejection: return RateLimited { dimension, retry_after_secs }

2. CHAIN WALK (per upstream chain[i], i = 0..N)
   for each target in chain:
     2a. BREAKER GATE
         - if Open AND cooldown not elapsed: skip; continue to chain[i+1]
         - if Open AND cooldown elapsed: enter HalfOpen (one probe allowed)
     2b. PER-UPSTREAM RATE-LIMIT GATE
         - on rejection: return RateLimited (no fallback — per-upstream
           limits encode operator intent: "don't exceed this quota")
     2c. RETRY LOOP within this upstream
         attempt = 1, total_elapsed_ms = 0
         while attempt <= max_attempts AND total_elapsed_ms < total_budget_ms:
           result = provider.complete(req.clone(), target.clone()).await
           Ok(stream)  → record breaker.success(); RETURN Ok(stream)
                          ← FIRST BYTE WINS; no further fallback possible
           Err(e):
             record breaker.failure(e)
             if e in retry_on AND attempt < max_attempts:
               backoff = compute_backoff(attempt, jitter)
               if total_elapsed_ms + backoff > total_budget_ms: break
               sleep(backoff); attempt += 1; total_elapsed_ms += backoff
             else: break
     2d. ON RETRY EXHAUSTION
         if e is fallback-eligible (network/5xx/429): continue to chain[i+1]
         else (4xx, decode, encode, capability):
           return TerminalError { error: e, tried: chain[..=i] }

3. CHAIN EXHAUSTION
   if every chain element exhausted retries:
     return NoUpstreamSucceeded { tried: chain }
   if every chain element had its breaker open:
     return AllBreakersOpen { tried: chain.map(|t| t.provider.clone()) }
```

**Architectural properties:**

- Rate limits gate first — 429 costs zero upstream calls.
- Breakers gate per-upstream — tripped breaker doesn't consume retries
  or rate-limit tokens.
- Retries are within one upstream — they don't cross upstream boundaries.
- Fallback fires on retry exhaustion — not on a single failure.
- Terminal errors don't fallback — a 4xx from `chain[0]` means the request
  is wrong; trying `chain[1]` would just reproduce it.
- Streaming fallback follows D4 — once `complete()` returns `Ok(stream)`,
  the call commits. Mid-stream failures surface as stream errors.

### 5.3 Backoff math

```rust
fn compute_backoff(attempt: u32, policy: &RetryPolicy, rng: &mut impl Rng) -> Duration {
    let base_ms = policy.initial_backoff_ms as f64
        * policy.multiplier.powi((attempt - 1) as i32);
    let jitter_factor = 1.0
        + rng.gen_range(-policy.jitter_pct..=policy.jitter_pct) / 100.0;
    let final_ms = (base_ms * jitter_factor).round() as u64;
    Duration::from_millis(final_ms)
}
```

Pure function. Property test bounds output to
`[base × (1 - jitter%), base × (1 + jitter%)]`. Deterministic test for
the §4.5 defaults.

### 5.4 Error classification

```rust
pub(crate) fn fallback_eligibility(e: &ProviderError) -> FallbackEligibility {
    match e {
        ProviderError::Network(_)                            => Eligible,
        ProviderError::Upstream { status, .. } if *status >= 500 => Eligible,
        ProviderError::Upstream { status: 429, .. }          => Eligible,
        ProviderError::Upstream { .. }                       => Terminal, // 4xx
        ProviderError::Decode(_)                             => Terminal,
        ProviderError::Encode(_)                             => Terminal,
        ProviderError::CapabilityMismatch(_)                 => Terminal,
        ProviderError::UnknownProvider(_)                    => Terminal,
    }
}
```

Per-route `retry_on` overrides the default mapping.

### 5.5 Concurrency & cancellation

`ResilientCaller` is `Send + Sync`. Registries are `Arc`-shared:
- `BreakerRegistry` uses `RwLock<HashMap<(String, String), BreakerState>>`.
  Hot path uses `read()` only; mutations on state transitions.
- `LimiterRegistry` uses `governor`'s lock-free atomic implementation
  internally; outer registry uses `DashMap` for per-key insertion.

**Cancellation:** `complete()` is cancel-safe. Dropping the future at any
point releases all locks, leaves breaker state coherent (failure recorded
only after `provider.complete()` returns), and doesn't leak rate-limit
tokens.

---

## 6. Error Envelopes & HTTP Status Codes

### 6.1 New `HandlerError` variants

```rust
pub enum HandlerError {
    // ... existing variants
    RateLimited { dimension: RateLimitDimension, retry_after_secs: u32 },
    NoUpstreamSucceeded { tried: Vec<TriedUpstream>, last_error: ProviderError },
    AllBreakersOpen { tried: Vec<String> },
}
```

### 6.2 HTTP status mapping

| Variant | HTTP | When |
|---|---|---|
| `RateLimited` | **429** | Any of 4 buckets rejected. Includes `Retry-After`. |
| `NoUpstreamSucceeded` | **503** | All chain attempts failed; last error eligible. |
| `AllBreakersOpen` | **503** | Chain walked, every upstream skipped. |
| Terminal `Provider(Upstream{4xx})` | **passes through** | First chain element returned 4xx. |

### 6.3 Wire envelopes per dialect

**OpenAI Chat / OpenAI Responses:**

```json
{"error": {
  "message": "<human readable>",
  "type":    "rate_limit_error" | "service_unavailable_error",
  "code":    "rate_limited_per_key" | "rate_limited_per_route" |
             "rate_limited_per_upstream" | "rate_limited_per_ip" |
             "no_upstream_available" | "all_breakers_open"
}}
```

**Anthropic Messages:**

```json
{"type": "error", "error": {
  "type":    "rate_limit_error" | "overloaded_error",
  "message": "<human readable>"
}}
```

(No `code` field — dimension/cause distinction lives in the message and
the operator log.)

### 6.4 `Retry-After` header

For all `RateLimited` variants, the response includes `Retry-After: <secs>`
where `<secs>` comes from `governor::query()`. Most LLM SDKs honor this
header automatically.

`NoUpstreamSucceeded` and `AllBreakersOpen` don't send `Retry-After` —
the gateway can't predict when an upstream will recover.

### 6.5 Operator log shape

Every error response produces a structured `tracing::warn!` log:

```
WARN gateway::resilient_caller: request failed
  request_id="req_abc123"
  identity="key:sha256:e3b0..."  # or "anonymous"
  client_ip="203.0.113.42"
  frontend="openai_chat"
  model="gpt-4o"
  outcome="no_upstream_succeeded"
  tried=[
    {upstream="openai",   attempts=3, last_error="upstream_5xx:502", elapsed_ms=2300},
    {upstream="copilot",  attempts=2, last_error="network:timeout",  elapsed_ms=4100},
    {upstream="deepseek", attempts=1, last_error="upstream_4xx:401", elapsed_ms=180},
  ]
  total_elapsed_ms=6580
```

API keys and IPs are redacted in `info` logs and only present at `debug`
level (existing `crates/observability/` convention).

### 6.6 Streaming responses

Per D4, rate-limit and breaker-open errors fire only pre-stream and always
produce a clean HTTP 429/503. Mid-stream failures (after `complete()`
returns `Ok(stream)` and bytes flow) surface as frontend-shaped error
events on the stream — existing v0.3 behavior, unchanged.

---

## 7. Plan Decomposition

Five plans, frontend-major sequencing. Each plan independently shippable.

```
2026-05-08-01-config-and-retries.md          (foundation: schema + retry policy)
2026-05-08-02-fallback-chains.md             (chain walker, ResilientCaller skeleton)
2026-05-08-03-circuit-breakers.md            (breaker state machine)
2026-05-08-04-rate-limiting-and-auth.md      (4-dimension limiter + auth headers)
2026-05-08-05-observability-docs-release.md  (tracing fields, docs, v0.4.0 tag)
```

Each plan is independently shippable:
- After Plan 01, retries work against single-upstream routes.
- After Plan 02, fallback works.
- After Plan 03, breakers prevent retry storms.
- After Plan 04, rate limiting and auth are live.
- After Plan 05, v0.4.0 ships.

**Per-plan tasks** (rough count):
- Plan 01: 6 tasks (~8 new tests).
- Plan 02: 6 tasks (~12 new tests).
- Plan 03: 6 tasks (~10 new tests).
- Plan 04: 7 tasks (~14 new tests).
- Plan 05: 5 tasks (~4 new tests; mostly docs).

**Total:** ~30 tasks, ~48 new tests. Workspace test count: 477 → ~525.

**Cross-plan invariants** (verified per merge):
- Frozen-core: `git diff master..HEAD -- crates/core/` is empty.
- No frontend changes: `git diff master..HEAD -- crates/frontends/` is empty.
- No provider code changes (Plan 01 docs only excepted):
  `git diff master..HEAD -- crates/providers/src/` is empty.

---

## 8. Test Strategy

Three layers, mirroring Phase 3.

### Layer A — Pure unit tests (in-module)

- `compute_backoff()` — property test bounds output; deterministic test
  for §4.5 defaults; budget-exhaustion test (R7).
- `BreakerState` transitions — every state change with injected `Clock`.
- `TokenBucket` — concurrent-access test under contention.
- `fallback_eligibility()` — every `ProviderError` variant.
- Schema deserialization — every (singular, array, both, neither) ×
  (valid, invalid) combination.

### Layer B — Resilience subsystem tests (`crates/router/tests/`)

- `ResilientCaller` decision-tree branches with `MockProvider`. At minimum:
  retry-then-fallback-then-succeed; retry-exhaust-then-fallback-then-succeed;
  fallback-on-5xx-not-on-4xx; breaker-open-skips-without-retry;
  rate-limit-fires-before-chain-walk; cancellation-during-sleep-leaves-
  state-coherent.
- `BreakerRegistry` concurrent stress (100 tasks recording success/failure
  on same key).
- `LimiterRegistry` four-dimension composition.

### Layer C — Cross-protocol integration (`crates/protocol-tests/tests/`)

End-to-end mockito serving real upstream wire shapes:
- `responses_fallback_oai_to_anthropic.rs` — Plan 02 T5.
- `streaming_fallback_pre_stream_only.rs` — Plan 02 T6 (pins D4).
- `breaker_trip_skips_upstream.rs` — Plan 03 T5.
- `breaker_half_open_recovery.rs` — Plan 03 T6.
- `rate_limit_per_key_envelope.rs` — Plan 04 T5.
- `auth_required_unconfigured_key.rs` — Plan 04 T6.
- `routes_singular_form_unchanged.rs` — Plan 01 T5 (R5 + reversibility).

### Performance & overhead targets

- Rate-limit gate disabled: zero atomic ops, zero allocations on hot path.
- Rate-limit gate enabled, no buckets exceeded: ≤4 atomic loads per request.
- Breaker gate (always on): one `RwLock::read()` per chain element;
  uncontended path ~50ns.
- Retry overhead: zero on success.

No benchmark in v0.4 — Phase 5's metrics will surface real-world overhead.

### Live-traffic verification (before tagging v0.4.0)

1. Run `agent-shim serve` with a real fallback-chain config.
2. Force-fail upstream A (misconfigure key) and verify a real client SDK
   gets the response from upstream B without manual intervention.
3. Hit the rate limit deliberately and verify `Retry-After` is honored
   by the SDK's auto-retry.

---

## 9. Risks and Mitigations

**R1. `governor` crate API mismatch.** Plan 04 T2 starts with a 30-line
wrapper; if it grows past ~150 lines, escalate.

**R2. Streaming-fallback ambiguity in client SDKs.** Documented in
`docs/resilience.md`; Plan 02 T6 pins D4 in regression coverage.

**R3. Breaker thrash on noisy upstreams.** Conservative defaults; per-route
tuning available.

**R4. In-memory state under multi-instance deployments.** Documented as
known limitation. Phase 5 candidate.

**R5. `RouteEntry` mixed-shape deserialization.** Plan 01 T1 unit-tests
every (singular, array, both, neither) × (valid, invalid) combination.

**R6. Per-upstream rate limit blocking traffic that fallback could handle.**
Per-upstream limits off by default; warn-level log on every rejection.

**R7. Backoff exceeding total budget.** Pseudo-code explicitly checks
`if total_elapsed_ms + backoff_ms > total_budget_ms: break`; Plan 01 T3
includes this scenario.

**R8. Cancellation during retry sleep.** Breaker `record_*` calls happen
only after `provider.complete()` returns or errors — never during sleep.
Plan 03 T3 includes a cancellation test.

**R9. `Retry-After` precision under jitter.** Acceptable — clients add
their own randomization.

**R10. Anonymous-bucket abuse.** Per-IP bucket gates first when enabled;
operators are documented to enable per-IP when accepting anonymous traffic.

---

## 10. Rejected Alternatives

**A1. Named fallback groups.** `fallback_groups: { name: [...] }` plus
route references. Rejected: most v0.4 deployments have ≤10 routes;
indirection adds friction. Reconsider in v0.5.

**A2. Provider-graph fallback.** Separate `fallbacks:` table keyed by
`(upstream, error_class) → next_target`. Rejected: harder to reason
about, harder to test, no operator feedback demands this flexibility.

**A3. Hard-break: array form only.** Rejected: violates semver
expectations; deserialization complexity is bounded.

**A4. Bounded buffered fallback for streaming.** Rejected: latency penalty
on every successful streaming request; complexity; observed Phase 3
mid-stream failure rate is low.

**A5. Full-response buffering.** Rejected: defeats streaming entirely.

**A6. Fallback on any error.** Rejected: 4xx falls through every chain
element, burning rate-limit tokens at each.

**A7. Consecutive-failure breaker policy.** Rejected: bursty patterns
reset the counter and never trip.

**A8. Trip-on-first-failure breaker.** Rejected: trips on single transient
blips.

**A9. Custom auth header.** Rejected: standard `Authorization` /
`x-api-key` matches what client SDKs already send.

**A10. Defer auth/key plumbing to v0.5.** Rejected: per-key is the
highest-value rate-limit dimension.

**A11. External KV (Redis) for shared state.** Rejected for v0.4 — adds
infrastructure dependency; most deployments are single-instance. v0.5
candidate.

**A12. Middleware-per-subsystem in pipeline.** Rejected: ordering fragile;
shared state awkward; intrinsic coupling between layers.

**A13. Inline-in-pipeline.** Rejected: pipeline.rs would push to ~1200
lines; cohesion suffers.

**A14. Cost/latency-aware routing in v0.4.** Rejected: requires
infrastructure that doesn't exist yet; orthogonal to resilience story.

**A15. Request budget caps in v0.4.** Rejected: requires upstream cost
models, daily-rollover bucket math, persistence. v0.5 candidate.

ADR-0004 (Plan 05 T2) records the §3 architecture choice (A12 + A13
rejected in favor of `ResilientCaller` orchestrator).

---

## 11. Reversibility

Every v0.4 feature except retries is opt-in via config:
- `auth.enabled` defaults to `false` — header inspection skipped.
- `rate_limit.enabled` defaults to `false` — entire subsystem bypassed.
- `breaker.enabled` defaults to `true` per route, but breakers never trip
  unless `min_requests` is reached AND `failure_threshold_pct` is exceeded.
  In practice, healthy upstreams never trip the breaker.
- `retry.max_attempts` defaults to `2` per D12 — small behavior change
  on upgrade. Operators wanting strict v0.3 behavior set
  `retry: {max_attempts: 1}` per route.

A v0.3 → v0.4 upgrade with no config changes produces identical wire
output for healthy upstreams. The only behavior difference is more
requests succeed on transient errors (one extra retry).

---

## 12. Out of Scope for v0.4

**Frontends:** No new frontends. Resilience is the focus.

**Providers:** No new providers. Existing 5 (Anthropic, OpenAI-compat,
Copilot, Gemini, DeepSeek) gain Resilience subsections in their docs.

**Cross-cutting features deferred to v0.5+:**
- Distributed/shared state (Redis backend) — Phase 5 candidate.
- Cost/latency-aware routing — Phase 5+.
- Request budget caps (per-key per-day token/cost limits) — Phase 5.
- Prometheus metrics, hot-reload config, OpenTelemetry — Phase 5.
- Audio / file content end-to-end — Phase 6+ if at all.
- Multi-account Copilot — Phase 6.
- OAuth Anthropic — Phase 6.
- Vertex Anthropic / Bedrock Anthropic — Phase 4.x candidate (separate plan).

**Definition of done (v0.4.0):**

- All 5 Phase 4 plan files merged via `--no-ff` with detailed commit messages.
- `cargo test --workspace` passes; final test count documented (~525).
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.
- `cargo build --release -p agent-shim` succeeds.
- `cargo deny check` clean.
- All 12 locked decisions (D1–D12) honored in implementation.
- Frozen-core invariant holds: `git diff v0.3.0..master -- crates/core/`
  is empty.
- No frontend changes:
  `git diff v0.3.0..master -- crates/frontends/` is empty.
- No provider code changes:
  `git diff v0.3.0..master -- crates/providers/src/` is empty.
- All v0.3 example configs continue to validate.
- CHANGELOG `[0.4.0]` entry comprehensive: Added (5 subsystems), Changed
  (default retry behavior per D12), Deprecated (none), Fixed (any latent
  bugs surfaced).
- README.md capability matrix v0.4; "What's NOT in v0.4"; releases bullet.
- `docs/architecture.md` capability matrix gains Resilience row; perf
  overhead targets paragraph.
- `docs/resilience.md` operator-facing guide published.
- `docs/adr/0004-resilient-caller.md` published.
- Each provider doc gains "Resilience behavior" subsection.
- Workspace version bumped `0.4.0-dev → 0.4.0`.
- `EDITOR_VERSION` / `EDITOR_PLUGIN_VERSION` already at `AgentShim/0.4.0`
  (set during kickoff bump).
- Live-traffic smoke test passes against real upstreams.
- Tag `v0.4.0` on master release-merge commit.
