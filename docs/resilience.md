# Resilience Guide (v0.4+)

This guide is for operators configuring AgentShim's resilience features:
fallback chains, retries, circuit breakers, rate limiting, and inbound
API key authentication. Underlying design rationale lives in
[`docs/superpowers/specs/2026-05-08-phase-4-resiliency-design.md`](superpowers/specs/2026-05-08-phase-4-resiliency-design.md).

Every feature here is opt-in. A v0.3 config that doesn't add a `retry`,
`breaker`, `rate_limit`, or `auth` block keeps v0.3 behavior verbatim.

## Quick Start

A worked config exercising every resilience block:

```yaml
server:
  bind: 127.0.0.1
  port: 8787
logging:
  filter: info,agent_shim=debug

upstreams:
  openai:
    type: openai_compat
    base_url: https://api.openai.com/v1
    api_key: "sk-openai-..."
  copilot:
    type: openai_compat
    base_url: https://api.githubcopilot.com
    api_key: "ghu_..."

routes:
  - frontend: openai_chat
    model: gpt-4o
    upstreams:
      - { name: openai,  model: gpt-4o-2024-11-20 }
      - { name: copilot, model: gpt-4o }
    retry:
      max_attempts: 3
      initial_backoff_ms: 100
      multiplier: 2.0
      jitter_pct: 25
      total_budget_ms: 5000
      retry_on: [network, upstream_5xx, upstream_429]
    breaker:
      enabled: true
      failure_threshold_pct: 50
      min_requests: 20
      window_secs: 60
      open_cooldown_secs: 30

auth:
  enabled: true
  required: true
  keys:
    "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855":
      label: alice-ci

rate_limit:
  enabled: true
  per_key:
    default:    { rate_per_sec: 10, burst: 30 }
    anonymous:  { rate_per_sec: 1,  burst: 5  }
  per_route:
    "openai_chat/gpt-4o": { rate_per_sec: 50, burst: 100 }
  per_upstream:
    openai: { rate_per_sec: 200, burst: 500 }
  per_ip:
    enabled: false
```

Validate with `agent-shim validate-config --config config/gateway.yaml`
before booting.

## Authentication

The `auth` block enables inbound API key authentication. Keys are stored
hashed; plaintext is never written to disk or logs. The gateway reads
keys from `Authorization: Bearer ...` (OpenAI dialect) and `x-api-key`
(Anthropic dialect), hashes them, and matches against `auth.keys`.

### Generating SHA-256 key hashes

Linux / macOS:

```bash
echo -n "my-plaintext-key" | sha256sum | awk '{print "sha256:" $1}'
```

Windows PowerShell:

```powershell
$key = "my-plaintext-key"
"sha256:" + (Get-FileHash -Algorithm SHA256 -InputStream `
  ([System.IO.MemoryStream]::new(
     [System.Text.Encoding]::UTF8.GetBytes($key)
   ))).Hash.ToLower()
```

Paste the output into `auth.keys.<hash>` and into
`rate_limit.per_key.overrides.<hash>` if a per-key bucket override is
needed. Validation rejects entries that don't start with `sha256:` so a
pasted plaintext key fails fast at startup.

### `auth.enabled` vs `auth.required`

| `enabled` | `required` | Behavior |
|---|---|---|
| `false` | (ignored) | v0.3 behavior. Every request is `Anonymous`. |
| `true` | `false` | Known keys carry their label; unknown / missing keys become `Anonymous` and the request continues. |
| `true` | `true` | Unknown / missing keys are rejected with HTTP 401 + `WWW-Authenticate: Bearer`. The request never reaches a provider. |

## Retries

When a provider call fails with an eligible error, the retry loop
re-tries against the **same** upstream before any fallback decision.
Cross-upstream movement is the fallback chain's job (see below).

| Field | Default | Meaning |
|---|---|---|
| `max_attempts` | `2` | Total attempts including the first call. `1` disables retry. |
| `initial_backoff_ms` | `100` | First sleep duration. |
| `multiplier` | `2.0` | Each subsequent sleep multiplies the previous. Must be `> 1.0`. |
| `jitter_pct` | `25.0` | Symmetric jitter `± jitter_pct%`. Avoids thundering-herd. |
| `total_budget_ms` | `5000` | Wall-clock cap. If the next sleep would exceed the budget, the loop exits. |
| `retry_on` | `[network, upstream_5xx, upstream_429]` | Eligible error classes. |

Eligible classes:

- `network` — connect / DNS / TLS / read errors before any HTTP status.
- `upstream_5xx` — HTTP 500-599 (Anthropic 529 falls in this band).
- `upstream_429` — HTTP 429 from upstream.

Other errors (4xx other than 429, decode, encode, capability mismatch)
are **terminal**: no retry, no fallback. The client gets the upstream's
status passed through.

## Fallback Chains

Routes use the `upstreams` array form to declare an ordered chain:

```yaml
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstreams:
      - { name: openai,   model: gpt-4o-2024-11-20 }
      - { name: copilot,  model: gpt-4o }
      - { name: deepseek, model: deepseek-v3 }
```

The chain is walked head-to-tail. Each element runs its own retry loop;
on retry exhaustion with an **eligible** last error, the gateway moves
to the next element. A terminal error (4xx other than 429, decode,
capability mismatch) short-circuits the chain — trying the next element
would just reproduce the bad request.

The v0.3 singular form (`upstream:` + `upstream_model:`) keeps working
and is internally a one-element chain. Mixing both shapes on the same
route is rejected at startup.

Fallback is **pre-stream only**. Once the provider returns a streaming
body and bytes start flowing, the call is committed; a mid-stream
upstream failure surfaces as a frontend-shaped error event, not a
fallback. Streaming SDKs cannot generally recover from a body switch
mid-flight.

## Circuit Breakers

Breakers are scoped per `(upstream, model)` pair with a sliding-window
failure-rate state machine: **Closed** observes traffic; **Open**
short-circuits the chain element; **HalfOpen** allows one probe after
the cooldown elapses. A failed probe re-opens for another cooldown.

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Opt-in. |
| `failure_threshold_pct` | `50` | Open when `failures / total >= threshold` over the window. |
| `min_requests` | `20` | Below this many requests in the window, the breaker stays Closed regardless of failure rate. |
| `window_secs` | `60` | Sliding window for the failure-rate calculation. |
| `open_cooldown_secs` | `30` | How long Open before a probe is allowed. |

A tripped breaker **does not** consume retry budget or rate-limit
tokens — the chain element is skipped and the next one is tried. If
every chain element has its breaker open, the gateway returns HTTP 503
`all_breakers_open`.

## Rate Limiting

Four bucket dimensions, all token-bucket (`governor` crate):

| Dimension | Key | Use |
|---|---|---|
| `per_key` | hashed API key (or `anonymous`) | Tenant-fairness across known callers. |
| `per_route` | `<frontend>/<model>` | Cap traffic on a specific public route. |
| `per_upstream` | upstream name | Honor a third-party quota; rejection here does **not** fall back. |
| `per_ip` | client IP from `x-forwarded-for` | DoS / per-tenant limits behind a known reverse proxy. |

A request must satisfy **all applicable buckets**. The first to reject
names itself in the error envelope and the structured log line.

`per_ip` requires `enabled: true` and a trusted reverse proxy that
asserts `x-forwarded-for`. Default per-IP values when enabled are
`rate_per_sec: 5, burst: 20`. Without a known proxy, the IP read off
the socket may always be the load balancer — leave `per_ip` disabled
in that case.

`per_key.anonymous` applies when no key was presented (or `auth` is
disabled). A tight anonymous bucket combined with generous per-key
defaults is the v0.4-recommended way to give known callers headroom
while still rate-limiting the open internet.

## When Breakers Trip vs Fallback Fires

Gates run outermost-first:

```
1. AUTH GATE                  (wrong key + auth.required=true → 401)
2. PRE-CHAIN RATE LIMIT       (per_key, per_route, per_ip → 429)
3. CHAIN WALK, per element:
     a. BREAKER GATE          (Open + cooldown not elapsed → skip)
     b. PER-UPSTREAM RATE LIMIT
                              (rejection → 429, NO fallback)
     c. RETRY LOOP            (within this upstream only)
     d. ON RETRY EXHAUSTION:
        - eligible error      → continue to next chain element
        - terminal error      → return to client (passes through 4xx)
4. CHAIN EXHAUSTED:
     - every element tried    → 503 no_upstream_available
     - every breaker open     → 503 all_breakers_open
```

Invariants:

- **Rate-limit before chain.** A 429 costs zero upstream calls.
- **Breaker before retry.** A tripped breaker does not consume retry
  budget or per-upstream rate-limit tokens.
- **Per-upstream rate-limit does not fall back.** Operator intent is
  "do not exceed this quota", not "find another upstream".
- **Terminal errors do not fall back.** A 4xx from the first chain
  element is a client bug; the second element would reproduce it.

## Operator Log Lines

Resilience events emit structured `tracing` fields. The default log
filter `info,agent_shim=debug` surfaces them. Warn-level events:

| Message | Fields | Fires when |
|---|---|---|
| `retrying provider call after eligible error` | `attempt`, `backoff_ms`, `total_elapsed_ms`, `error` | Inside retry loop after an eligible failure. |
| `falling back to next upstream` | `from`, `to`, `chain_position` | Retry exhausted with eligible error; chain has another element. |
| `breaker open; skipping chain element` | `provider`, `model`, `chain_position` | Breaker is Open and cooldown not elapsed. |
| `terminal error; not falling back` | `provider`, `model`, `error` | Provider returned a non-eligible error. |
| `rate limited (pre-chain)` | `identity`, `frontend_model`, `client_ip`, `dimension`, `retry_after_secs` | A pre-chain bucket rejected. |
| `per-upstream rate limit exceeded; not falling back` | `upstream`, `chain_position`, `dimension` | Per-upstream bucket rejected. |

Info-level: `chain element succeeded` (`provider`, `model`,
`chain_position`, `elapsed_ms`) and `breaker half-open; attempting
probe`. API keys and client IPs are logged only at `debug`; at `info`
and `warn` the identity is a label (e.g. `key:alice-ci`) or
`anonymous`.

## Error Envelopes

| HTTP | Cause | Headers |
|---|---|---|
| `401 Unauthorized` | `auth.required=true` and the key is unknown / missing. | `WWW-Authenticate: Bearer` |
| `429 Too Many Requests` | A bucket rejected. | `Retry-After: <secs>` |
| `503 Service Unavailable` | `no_upstream_available` or `all_breakers_open`. | (no `Retry-After`) |
| Pass-through 4xx | First chain element returned a terminal 4xx (other than 429). | Forwarded from upstream. |

OpenAI-dialect envelopes carry a `code` discriminator
(`rate_limited_per_key`, `rate_limited_per_route`,
`rate_limited_per_upstream`, `rate_limited_per_ip`,
`no_upstream_available`, `all_breakers_open`). Anthropic-dialect
envelopes use `type: "rate_limit_error"` or `"overloaded_error"` with no
`code` field; the dimension is in the human message and the operator
log. 503 responses never carry `Retry-After` — the gateway cannot
predict upstream recovery.

## Multi-Instance Deployments (v0.4 Caveat)

In v0.4, breaker and rate-limit state lives in process memory only. Two
gateway instances behind a load balancer each have independent buckets,
so the effective rate limit is `2 × configured` until the upstream
itself rejects.

For strict cross-instance enforcement, front the gateway with a
dedicated reverse proxy (envoy, nginx, HAProxy) that does the rate
limiting, and configure AgentShim with very loose buckets as a safety
net. Distributed state via Redis is a v0.5 candidate.

## Known Limitations (v0.4)

- **`per_ip` trusts `x-forwarded-for`.** Front the gateway with a known
  proxy or leave `per_ip.enabled: false`. The gateway does not
  cross-check the immediate socket against an allowlist of trusted
  proxies in v0.4.
- **OpenAI Responses passthrough bypasses rate-limit + breaker.** When
  `BackendProvider::proxy_raw` matches and serves bytes directly, the
  request never enters `ResilientCaller::complete`, so neither gate
  fires. The known-gap comment lives in `crates/gateway/src/pipeline.rs`.
  Routes that need enforcement on Responses traffic should use the
  canonical path (any non-Anthropic upstream type) until the v0.5 fix
  lands. The canonical path always honors the gates.
- **Auth is header-only.** mTLS, OIDC, and JWT validation are out of
  scope for v0.4. The gateway compares SHA-256 hashes of the bearer
  token against `auth.keys`.
- **Single-process state.** See "Multi-Instance Deployments" above.

## Troubleshooting

**Validation says `must start with sha256:`.** Plaintext was pasted
into `auth.keys` or `rate_limit.per_key.overrides`. Run the SHA-256
recipe and paste the `sha256:<hex>` output.

**401 Unauthorized but I sent a key.** The key's SHA-256 hash isn't in
`auth.keys`. Re-hash (use `echo -n` on Unix to avoid a trailing newline)
and confirm the hex matches the config.

**429 when I'm under the per-route limit.** A request must clear all
four buckets. Check the `dimension` field on the `rate limited` warn
line. Common causes: a tight `anonymous` bucket when `auth` is on but
no key was sent; a `per_upstream` quota that fires on whichever chain
element was selected.

**Breaker keeps tripping on a healthy upstream.** Raise `min_requests`
so a noisy low-traffic route doesn't trip on three failures out of
five, or extend `window_secs` so transient blips don't dominate the
failure rate.

**Where did the fallback fire?** Look for `falling back to next
upstream` (warn) — it carries `from`, `to`, and `chain_position`. The
successor's outcome is logged shortly after.
