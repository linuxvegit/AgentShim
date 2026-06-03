# Performance Baseline Harness — Design

**Date:** 2026-06-03
**Baseline commit:** f599854
**Status:** Approved (brainstorming complete)
**Owner:** agent-shim
**Next artifact:** implementation plan under `docs/superpowers/plans/`

## 1. Goal

Build a reusable, in-repo benchmark harness that produces a first performance
baseline for AgentShim. The harness is the deliverable; the baseline numbers
are a side effect that validates the harness works. No optimization work is in
scope — that follows from what the baseline reveals.

Scope was set during brainstorming:

| Decision | Value |
|---|---|
| Goal dimension | Build a baseline first, then decide |
| Measurement layers | Micro-bench + local end-to-end |
| Run environment | Local Windows dev machine |
| Scenarios | Anthropic passthrough (byte-identity) + Copilot upstream |
| Workload driver | criterion only (no wrk/k6) |
| Mock upstream | In-process Rust (hyper) |
| Profiling | Not integrated in v1 |
| Deliverable shape | Reusable harness committed to the repo |

Out of scope: real network/TLS, real upstreams, plugins, hot reload, CI
execution of `cargo bench`, automated cross-commit comparison tooling.

## 2. Architecture

The harness lives inside the existing `crates/protocol-tests/` crate, which
is already dev-only and already owns cross-dialect fixture and lifecycle
validator code. No new workspace member is added.

```
crates/protocol-tests/
├── src/                       (existing: lifecycle validator, fixture builders)
├── tests/                     (existing: integration tests)
│   └── bench_harness_smoke.rs (NEW: 3 smoke tests for the harness)
└── benches/                   (NEW)
    ├── common/
    │   ├── mod.rs
    │   ├── mock_upstream.rs   — in-process hyper server, configurable fixture
    │   ├── fixtures.rs        — pre-recorded Anthropic/Copilot byte streams
    │   └── harness.rs         — minimal gateway (AppCore + AppSnapshot), reqwest client + URL
    ├── passthrough.rs         — Anthropic Messages → Anthropic upstream (byte-identity)
    └── copilot.rs             — OpenAI Chat / Responses → Copilot upstream
```

Responsibility boundaries:

- `benches/common/` is bench-only glue. The two bench files `use common::*`
  but never `use protocol_tests::` directly — avoids any circular dependency
  with the crate's `src/` and `tests/`.
- Each mock upstream is spawned per benchmark group, bound to
  `127.0.0.1:0` (OS-assigned port), and shut down on drop.
- Cross-dialect fixtures and the lifecycle validator already in
  `protocol-tests/src/` are reused for fixture validation but not for the
  benches themselves.

## 3. Mock upstream

`common/mock_upstream.rs` exposes:

```rust
pub struct MockHandle {
    pub url: String,                  // http://127.0.0.1:<port>
    _shutdown: oneshot::Sender<()>,   // Drop = server stop
}

pub fn spawn_mock(fixtures: Arc<HashMap<&'static str, FixtureBody>>) -> MockHandle;
```

Routing is by path only — no request-body validation, so bench measurements
reflect gateway decode/encode cost, not the mock's parsing cost:

| Path | Behavior |
|------|----------|
| `POST /v1/messages` | Anthropic Messages — returns fixture selected by `?fixture=<name>` |
| `POST /chat/completions` | OpenAI Chat — same pattern |
| `POST /responses` | OpenAI Responses — same pattern |
| `GET /github/api/v3/copilot_internal/v2/token` | Copilot token exchange — returns a long-lived fake token (no renewal pressure during benches) |
| `GET /v1/models` | Returns a fixed 4-model catalog so `ModelIndex` populates at startup |

### Fixture format

```rust
pub enum FixtureBody {
    Sse(&'static [u8]),     // server may chunk + sleep per config; defaults: send whole body once, 0 delay
    Unary(&'static [u8]),
}
```

All fixture bytes are compiled into the binary (`include_bytes!` or byte
literals). No I/O at bench startup.

### Fixture inventory

| Name | Format | Size | Purpose |
|---|---|---|---|
| `anthropic_sse_short` | SSE | ~800 B | 3 text deltas + message_stop |
| `anthropic_sse_long` | SSE | ~30 KB | 100 text deltas + tool_use block — streaming steady state |
| `anthropic_unary` | Unary | ~600 B | simple message response |
| `copilot_chat_sse_short` | SSE | ~800 B | OpenAI Chat SSE, short |
| `copilot_chat_sse_long` | SSE | ~30 KB | OpenAI Chat SSE, long |
| `copilot_responses_sse_short` | SSE | ~1 KB | OpenAI Responses SSE |

Every new fixture must pass the existing `protocol-tests` lifecycle validator
in a `#[test]` next to its declaration. This keeps benches from measuring
performance on invalid streams.

## 4. Gateway harness

`common/harness.rs`:

```rust
pub struct GatewayHandle {
    pub client: reqwest::Client,
    pub base_url: String,
    _shutdown: oneshot::Sender<()>,
    _mock: MockHandle,
}

pub fn spawn_anthropic_passthrough() -> GatewayHandle;
pub fn spawn_copilot_chat() -> GatewayHandle;
pub fn spawn_copilot_responses() -> GatewayHandle;
```

Each `spawn_*` function:

1. Spawns the mock upstream, captures `mock.url`.
2. Builds an in-memory `GatewayConfig` (no YAML), pointing the upstream base
   URL at the mock.
3. Constructs `AppCore + AppSnapshot + axum Router` via the gateway crate's
   existing wiring. If the existing wiring cannot be called from outside the
   binary, expose a single `for_bench()` constructor as either a `pub` API
   marked `#[doc(hidden)]` or behind a `bench` cargo feature on `agent-shim`.
   `cfg(test)` is **not** a valid seam here — `cfg(test)` is per-crate, and
   a bench in `protocol-tests` consuming `agent-shim` sees `agent-shim`
   compiled without `cfg(test)`.
4. Binds the axum router to `127.0.0.1:0` via `axum::serve`, spawns the
   server task.
5. Issues one warmup request so steady-state caches (Copilot token, tiktoken
   encoder, ModelIndex) are populated before the bench loop starts.

Config simplification:

- Single route, single upstream.
- No plugins, no rate-limit, no cost-filter, no OpenTelemetry export, no
  Prometheus exporter.
- `metrics_layer` stays in the request path (matches production default
  shape).
- File logging off; tracing subscriber is a noop.
- Copilot: token manager points at the mock's token endpoint.

A `spawn_copilot_chat_with_plugins(...)` hook is reserved in the signature
plan but **not implemented** in v1. It exists so a future plugin-overhead
comparison can land without churning the harness API.

### Why not reuse `tests/` setup

Existing protocol-tests integration tests use `tower::ServiceExt::oneshot`,
which bypasses hyper/tokio scheduling and TCP. Benches require real TCP so
they reflect reqwest, hyper server, and SSE streaming cost. This is the only
material divergence between the test setup and the bench setup.

## 5. Bench files

### Criterion configuration (shared)

```rust
fn config() -> Criterion {
    Criterion::default()
        .sample_size(50)
        .measurement_time(Duration::from_secs(10))
        .warm_up_time(Duration::from_secs(3))
        .noise_threshold(0.05)
}
```

Async benches use `tokio::runtime::Builder::new_multi_thread()`. The runtime
is constructed **once per group**, passed to `b.to_async(&runtime).iter(...)`,
not rebuilt per iteration.

### `passthrough.rs` — Anthropic Messages → Anthropic upstream

| Group | Throughput unit | What it measures |
|---|---|---|
| `passthrough_unary` | bytes | Single unary request — admission + `proxy_raw` + reqwest forward |
| `passthrough_sse_short` | elements | 800 B SSE fixture consumed to completion |
| `passthrough_sse_long` | bytes | 30 KB SSE fixture, mock sends in 2 KB chunks with 0 delay — steady-state byte throughput + backpressure |
| `passthrough_concurrent_8` | elements | 8 in-flight requests via `tokio::task::JoinSet` — lock contention, Arc refcount, breaker `RwLock` |

### `copilot.rs` — Copilot upstream

| Group | Throughput unit | What it measures |
|---|---|---|
| `copilot_chat_unary` | bytes | OpenAI Chat in → Copilot Chat upstream, unary |
| `copilot_chat_sse_short` | elements | OpenAI Chat in → SSE consumed — `chat_sse_parser` + `encode_stream` |
| `copilot_responses_sse_short` | elements | OpenAI Responses in → Copilot upstream (Chat protocol) → Responses encoded — **the longest hot path** |
| `copilot_chat_concurrent_8` | elements | 8-way concurrency, mirrors passthrough |

### Explicit non-goals (commented in each bench file)

- TLS / HTTPS
- DNS resolution
- Real upstreams (Anthropic, Copilot)
- Plugin hooks
- Hot reload

## 6. Testing & quality

### Smoke tests — `crates/protocol-tests/tests/bench_harness_smoke.rs`

Three `#[tokio::test]` cases, one per `spawn_*`. Each:

1. Spawns the handle.
2. Issues a representative request.
3. Asserts HTTP 200 and a body shape sentinel (e.g. `body.starts_with(b"{\"id\":")`).

These run on every PR via `cargo test`. They are the **harness's own
correctness gate**: if a bench number suddenly looks 80% better, the smoke
test catches the case where a component was short-circuited by mistake.

### Fixture validation

Every fixture in `fixtures.rs` has a paired `#[test]` that runs the existing
`protocol-tests` lifecycle validator over the bytes. Invalid fixtures fail at
`cargo test`, never reach bench runs.

### CI hooks

- `cargo test -p agent-shim-protocol-tests` — runs smoke + fixture validation
  every PR.
- `cargo build -p agent-shim-protocol-tests --benches` — runs every PR. Cargo
  does not build `[[bench]]` targets by default, so this is an explicit
  compile gate.
- `cargo bench -p agent-shim-protocol-tests` — **not in CI** in v1. Run by
  hand on a quiet machine.

### Noise control (documented in `docs/perf/README.md`)

Windows-specific factors that affect criterion numbers:

- Defender real-time scan paused or `target/` excluded
- Browser / IDE / Slack closed
- Laptop plugged in, performance power mode
- At least two consecutive runs; the second run is canonical (first has page
  faults and cold filesystem cache)

The README explicitly does **not** promise stability to any percentage point.
That is a property of Linux bare-metal or dedicated perf runners.

### Regression rule

Criterion auto-flags ≥5% changes given the configured `noise_threshold`.
Formal regression judgement requires **two consecutive flagged runs**.
Documented in `docs/perf/README.md`, not automated.

## 7. Deliverable: baseline report

### Location

```
docs/perf/
├── README.md
└── baseline-YYYY-MM-DD-<commit7>.md
```

Each baseline run produces a new file. Old baselines stay. Cross-commit
comparison is `git diff docs/perf/baseline-A.md docs/perf/baseline-B.md`.

### Report template

```markdown
# Performance Baseline — <commit> — <date>

**Host:** Windows <ver>, <CPU model>, <RAM>
**Rust:** <rustc -V>
**Build:** cargo bench profile (opt-level=3, lto=thin per workspace default)
**Workload:** in-process mock upstream, http://127.0.0.1
**Excluded:** TLS, DNS, real upstream, plugins, hot reload

## Summary table

| Bench group                  | Throughput (median) | p50 (µs/req) | p95 | p99 | Notes |
|------------------------------|---------------------|--------------|-----|-----|-------|
| passthrough_unary            | …                   | …            | …   | …   |       |
| passthrough_sse_short        | …                   | …            | …   | …   |       |
| passthrough_sse_long         | … MB/s              | …            | …   | …   |       |
| passthrough_concurrent_8     | …                   | …            | …   | …   |       |
| copilot_chat_unary           | …                   | …            | …   | …   |       |
| copilot_chat_sse_short       | …                   | …            | …   | …   |       |
| copilot_responses_sse_short  | …                   | …            | …   | …   |       |
| copilot_chat_concurrent_8    | …                   | …            | …   | …   |       |

## Observations

- Most expensive group / cheapest group
- Canonical translation cost relative to passthrough
- Concurrency scaling (ideal 8×, actual N×)
- Anything surprising

## Next candidates

(Optimization candidates ranked by this baseline — listed only, not acted on)
```

### Generation flow

No automation script in v1. Manual flow:

1. `cargo bench -p agent-shim-protocol-tests --bench passthrough --bench copilot`
2. Read numbers from criterion stderr (`time:`, `thrpt:`) and from
   `target/criterion/<group>/<bench>/new/estimates.json`.
3. Hand-fill the template.
4. Write at least three observations.
5. Commit `docs/perf/baseline-YYYY-MM-DD-<sha>.md`.

Raw criterion HTML output stays in `target/criterion/` (gitignored). Not
committed — too large, too noisy.

## 8. Implementation order

### Stage 1 — Fixtures + mock (no dependencies)

New:
- `crates/protocol-tests/benches/common/mod.rs`
- `crates/protocol-tests/benches/common/fixtures.rs` + paired validation tests
- `crates/protocol-tests/benches/common/mock_upstream.rs`

### Stage 2 — Gateway harness (depends on stage 1)

New:
- `crates/protocol-tests/benches/common/harness.rs`
- If needed: `for_bench()` constructor seam in `crates/gateway/`, only if the
  existing wiring cannot already be invoked from outside the binary.

Modified:
- `crates/protocol-tests/Cargo.toml` — add `[[bench]] name = "passthrough" harness = false`
  and `name = "copilot" harness = false`; dev-deps add `criterion`, `reqwest`,
  `hyper` server feature, `tokio` rt-multi-thread. If the gateway exposes the
  seam behind a `bench` feature, the dev-dep on `agent-shim` declares
  `features = ["bench"]`.

### Stage 3 — Smoke tests (depends on stage 2, gates stage 4)

New:
- `crates/protocol-tests/tests/bench_harness_smoke.rs`

**Gate:** stage 3 must be green before stage 4 starts. This is the
hardest-to-debug failure surface; isolate it.

### Stage 4 — Bench files (depends on stage 3)

New:
- `crates/protocol-tests/benches/passthrough.rs`
- `crates/protocol-tests/benches/copilot.rs`

### Stage 5 — Baseline report + docs

New:
- `docs/perf/README.md`
- `docs/perf/baseline-YYYY-MM-DD-<sha>.md` — actual numbers

Modified:
- `CLAUDE.md` — add `cargo bench -p agent-shim-protocol-tests` to the
  Build & Test section.
- `docs/architecture.md` — one-line pointer to `docs/perf/README.md`.

## 9. Out of scope (will not be done in this spec)

- Any change to `crates/core/`, `crates/router/`, `crates/gateway/src/`
  production code, except a minimal `for_bench()` wiring seam if required.
- CI execution of `cargo bench`.
- Flamegraph / dtrace / perf integration.
- Production `Cargo.toml` changes (workspace deps, feature flags).
- Any optimization work informed by the baseline — that is the **next**
  spec.
- Cross-commit baseline diff tooling beyond `git diff`.

## 10. Definition of done

1. `cargo build -p agent-shim-protocol-tests --benches` succeeds.
2. `cargo test -p agent-shim-protocol-tests` is green, including the three
   smoke tests and all fixture validation tests.
3. `cargo bench -p agent-shim-protocol-tests` completes all 8 groups without
   panic or timeout on a quiet local Windows machine.
4. `docs/perf/baseline-YYYY-MM-DD-<sha>.md` exists with numbers filled in
   for every group and at least three observations.
5. `cargo fmt --all --check` passes.
6. `cargo clippy --workspace --all-targets -- -D warnings` passes.
7. `git diff crates/core crates/router crates/gateway/src` is empty or
   restricted to the `for_bench()` seam.
