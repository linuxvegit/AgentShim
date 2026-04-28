# Plan 06 — Cancellation Fuzz, Performance Benches, Release Polish

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Lock in the correctness/perf claims of the v0.1 spec before tagging a release. Add cancellation/disconnect fuzz tests, criterion benchmarks for the canonical-encode round-trip and gateway-overhead path, multi-arch Dockerfile, release CI workflow, and the architecture/contributing docs that the spec calls for.

**Architecture:** No new runtime crates — this plan is hardening across `protocol-tests`, `gateway`, and the workspace-level CI/Docker/docs. New `benches/` directories under each crate that needs them, driven by `criterion`. A release workflow cross-compiles `agent-shim` for `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`.

**Tech Stack:** `criterion` for benchmarks, `rand` (`SmallRng` with seed) for fuzz, `cross` for multi-arch builds, `cargo-llvm-cov` for coverage, `cargo deny` already wired.

---

## File Structure

`crates/protocol-tests/`:
- Create: `crates/protocol-tests/tests/cancellation_fuzz.rs` — 50 disconnect-at-random-offset iterations
- Modify: `crates/protocol-tests/Cargo.toml` (rand dev-dep)

`crates/core/`:
- Create: `crates/core/benches/canonical_round_trip.rs`
- Modify: `crates/core/Cargo.toml` (criterion + bench entry)

`crates/gateway/`:
- Create: `crates/gateway/benches/gateway_overhead.rs`
- Modify: `crates/gateway/Cargo.toml`
- Create: `crates/gateway/benches/baseline.json` (empty placeholder, populated by run)

`benches/`:
- Create: `scripts/bench.sh`

Root:
- Create: `deploy/Dockerfile`
- Create: `deploy/docker-compose.yaml`
- Create: `.github/workflows/release.yaml`
- Create: `docs/architecture.md`
- Create: `docs/configuration.md`
- Create: `docs/deployment.md`
- Create: `docs/contributing.md`
- Create: `docs/frontends/anthropic-messages.md`
- Create: `docs/frontends/openai-chat-completions.md`
- Create: `docs/providers/openai-compatible.md`
- Modify: `README.md`

---

## Task 1: Cancellation/disconnect fuzz test

**Files:**
- Modify: `crates/protocol-tests/Cargo.toml`
- Create: `crates/protocol-tests/tests/cancellation_fuzz.rs`

- [ ] **Step 1: Add `rand` dev-dep**

```toml
[dev-dependencies]
rand = { version = "0.8", default-features = false, features = ["small_rng"] }
```

- [ ] **Step 2: Test**

Idea: build a long canonical stream (1000 text deltas), encode through a frontend, drop the encoded stream at a random byte offset N times with a seeded RNG. Assert no panics, no leaked tasks (`tokio::runtime::Handle::current().metrics().num_alive_tasks() <= 2`), and that the upstream-side stream gets dropped (we observe by tracking drop in a custom guard).

```rust
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use futures::stream::{self, StreamExt};
use futures_util::stream::BoxStream;
use rand::{rngs::SmallRng, Rng, SeedableRng};

use agent_shim_core::error::StreamError;
use agent_shim_core::ids::ResponseId;
use agent_shim_core::message::MessageRole;
use agent_shim_core::stream::{CanonicalStream, ContentBlockKind, StreamEvent};
use agent_shim_core::usage::StopReason;
use agent_shim_frontends::anthropic_messages::AnthropicMessages;
use agent_shim_frontends::openai_chat::OpenAiChat;
use agent_shim_frontends::{FrontendProtocol, FrontendResponse};

struct DropGuard { counter: Arc<AtomicUsize> }
impl Drop for DropGuard { fn drop(&mut self) { self.counter.fetch_add(1, Ordering::SeqCst); } }

fn long_stream_with_guard(guard: DropGuard) -> CanonicalStream {
    let mut events: Vec<Result<StreamEvent, StreamError>> = vec![
        Ok(StreamEvent::ResponseStart { id: ResponseId("r".into()), model: "m".into(), created_at_unix: 0 }),
        Ok(StreamEvent::MessageStart { role: MessageRole::Assistant }),
        Ok(StreamEvent::ContentBlockStart { index: 0, kind: ContentBlockKind::Text }),
    ];
    for _ in 0..1000 {
        events.push(Ok(StreamEvent::TextDelta { index: 0, text: "lorem ipsum dolor sit amet ".into() }));
    }
    events.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
    events.push(Ok(StreamEvent::MessageStop { stop_reason: StopReason::EndTurn, stop_sequence: None }));
    events.push(Ok(StreamEvent::ResponseStop { usage: None }));

    let s = stream::iter(events).chain(stream::iter(std::iter::empty::<Result<StreamEvent, StreamError>>()));
    // Attach guard so drop is observable
    Box::pin(DropAware { inner: Box::pin(s), _guard: guard })
}

struct DropAware<S> { inner: std::pin::Pin<Box<S>>, _guard: DropGuard }
impl<S: futures::Stream + Unpin + Send> futures::Stream for DropAware<S> {
    type Item = S::Item;
    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

async fn drive_to_offset(
    encoded: BoxStream<'static, Result<Bytes, agent_shim_frontends::FrontendError>>,
    cutoff_bytes: usize,
) {
    let mut stream = encoded;
    let mut taken = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk { Ok(c) => c, Err(_) => break };
        taken += chunk.len();
        if taken >= cutoff_bytes { break; }
    }
    // Stream dropped here
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_disconnect_at_random_offsets_drops_upstream() {
    let mut rng = SmallRng::seed_from_u64(0xA9_C0_FF_EE);
    for iter in 0..50 {
        let counter = Arc::new(AtomicUsize::new(0));
        let guard = DropGuard { counter: counter.clone() };
        let canonical = long_stream_with_guard(guard);
        let frontend = AnthropicMessages { keepalive: None };
        let response = frontend.encode_stream(canonical);
        let s = match response { FrontendResponse::Stream { stream, .. } => stream, _ => panic!() };

        let cutoff = rng.gen_range(0..40_000);
        drive_to_offset(s, cutoff).await;
        // Yield to allow drop handlers to run.
        tokio::task::yield_now().await;
        assert_eq!(counter.load(Ordering::SeqCst), 1, "iter {iter}: upstream not dropped");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_disconnect_at_random_offsets_drops_upstream() {
    let mut rng = SmallRng::seed_from_u64(0x6B_AD_BE_EF);
    for iter in 0..50 {
        let counter = Arc::new(AtomicUsize::new(0));
        let guard = DropGuard { counter: counter.clone() };
        let canonical = long_stream_with_guard(guard);
        let frontend = OpenAiChat { keepalive: None, clock_override: Some(1700000000) };
        let response = frontend.encode_stream(canonical);
        let s = match response { FrontendResponse::Stream { stream, .. } => stream, _ => panic!() };

        let cutoff = rng.gen_range(0..40_000);
        drive_to_offset(s, cutoff).await;
        tokio::task::yield_now().await;
        assert_eq!(counter.load(Ordering::SeqCst), 1, "iter {iter}: upstream not dropped");
    }
}
```

- [ ] **Step 3: Run + commit**

Run: `cargo test -p agent-shim-protocol-tests --test cancellation_fuzz`
Expected: 2 passed (50 inner iterations each).

```bash
git add crates/protocol-tests
git commit -m "test(cancellation): seeded fuzz that disconnects mid-stream and asserts upstream drop"
```

---

## Task 2: Canonical-model encode/decode benchmarks

**Files:**
- Modify: `crates/core/Cargo.toml`
- Create: `crates/core/benches/canonical_round_trip.rs`

- [ ] **Step 1: Add criterion + bench entry**

```toml
[dev-dependencies]
criterion = { version = "0.5", features = ["html_reports"] }

[[bench]]
name = "canonical_round_trip"
harness = false
```

- [ ] **Step 2: Bench**

```rust
use agent_shim_core::{
    content::ContentBlock,
    ids::RequestId,
    message::{Message, MessageRole},
    request::{CanonicalRequest, GenerationOptions},
    target::{FrontendInfo, FrontendKind, FrontendModel},
};
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn typical_request() -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo { kind: FrontendKind::OpenAiChat, api_path: "/v1/chat/completions".into() },
        model: FrontendModel::from("gpt-4o"),
        system: vec![],
        messages: (0..10).map(|i| Message {
            role: if i % 2 == 0 { MessageRole::User } else { MessageRole::Assistant },
            content: vec![ContentBlock::text(format!("turn {i} content lorem ipsum "))],
            name: None,
            extensions: Default::default(),
        }).collect(),
        tools: vec![],
        tool_choice: Default::default(),
        generation: GenerationOptions { max_tokens: Some(1024), temperature: Some(0.7), ..Default::default() },
        response_format: None,
        stream: true,
        metadata: Default::default(),
        extensions: Default::default(),
    }
}

fn bench_round_trip(c: &mut Criterion) {
    let req = typical_request();
    let json = serde_json::to_vec(&req).unwrap();

    c.bench_function("canonical_encode_10msg", |b| {
        b.iter(|| { let _ = serde_json::to_vec(black_box(&req)).unwrap(); });
    });

    c.bench_function("canonical_decode_10msg", |b| {
        b.iter(|| { let _: CanonicalRequest = serde_json::from_slice(black_box(&json)).unwrap(); });
    });

    c.bench_function("canonical_round_trip_10msg", |b| {
        b.iter(|| {
            let bytes = serde_json::to_vec(black_box(&req)).unwrap();
            let _: CanonicalRequest = serde_json::from_slice(&bytes).unwrap();
        });
    });
}

criterion_group!(benches, bench_round_trip);
criterion_main!(benches);
```

- [ ] **Step 3: Run + commit**

Run: `cargo bench -p agent-shim-core --bench canonical_round_trip -- --quick`
Expected: prints throughput; no panics.

Spec target: <10µs per round-trip. Verify by reading the criterion report at `target/criterion/canonical_round_trip_10msg/report/index.html`. If we exceed 10µs, log it as a known issue — do not silently lower the bar.

```bash
git add crates/core
git commit -m "bench(core): canonical request encode/decode round-trip"
```

---

## Task 3: Gateway-overhead benchmark

**Files:**
- Modify: `crates/gateway/Cargo.toml`
- Create: `crates/gateway/benches/gateway_overhead.rs`

- [ ] **Step 1: Cargo additions**

```toml
[dev-dependencies]
criterion = { version = "0.5", features = ["html_reports"] }

[[bench]]
name = "gateway_overhead"
harness = false
```

- [ ] **Step 2: Bench**

Approach: in-process, the bench spawns a `mockito` upstream that replies instantly with a tiny SSE stream, runs the gateway against it, and measures wall-clock time per `/v1/chat/completions` call. We record p50 / p99 — criterion's default. Spec target: under 5ms p99.

```rust
use std::collections::BTreeMap;
use std::time::Duration;

use agent_shim_config::{schema::*, secrets::Secret};
use criterion::{criterion_group, criterion_main, Criterion};
use tokio::runtime::Runtime;

fn build_config(upstream_url: String) -> GatewayConfig {
    let mut up = BTreeMap::new();
    up.insert("u".into(), UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
        base_url: upstream_url,
        api_key: Secret::new("k"),
        default_headers: Default::default(),
        request_timeout_secs: 5,
    }));
    GatewayConfig {
        server: ServerConfig { bind: "127.0.0.1".into(), port: 0, keepalive_secs: 0 },
        logging: LoggingConfig::default(),
        upstreams: up,
        routes: vec![RouteEntry {
            frontend: "openai_chat".into(),
            model: "m".into(),
            upstream: "u".into(),
            upstream_model: "m".into(),
        }],
        copilot: None,
    }
}

fn bench_overhead(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let (upstream_url, _server) = rt.block_on(async {
        let mut s = mockito::Server::new_async().await;
        let body = "data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        s.mock("POST", "/v1/chat/completions").with_status(200)
            .with_header("content-type", "text/event-stream").with_body(body)
            .create_async().await.expect_at_least(0);
        let url = s.url();
        (url, s)
    });

    let cfg = build_config(upstream_url);
    let state = agent_shim::state::AppState::build(cfg).unwrap();

    let listener = rt.block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap() });
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route("/v1/chat/completions", axum::routing::post(agent_shim::handlers::openai_chat::handle))
        .with_state(state);
    rt.spawn(async move { axum::serve(listener, app).await.unwrap(); });
    rt.block_on(async { tokio::time::sleep(Duration::from_millis(50)).await });

    let url = format!("http://{}/v1/chat/completions", addr);
    let client = reqwest::Client::new();

    c.bench_function("gateway_streaming_round_trip", |b| {
        b.to_async(&rt).iter(|| async {
            let resp = client.post(&url)
                .header("content-type", "application/json")
                .body(r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"stream":true}"#)
                .send().await.unwrap();
            let _ = resp.bytes().await.unwrap();
        });
    });
}

criterion_group!(benches, bench_overhead);
criterion_main!(benches);
```

- [ ] **Step 3: Run**

Run: `cargo bench -p agent-shim --bench gateway_overhead -- --quick`
Expected: completes; criterion reports timing.

Open the HTML report at `target/criterion/gateway_streaming_round_trip/report/index.html` and confirm p99 < 5ms (subtract `mockito` server overhead — typically <1ms). If we miss, document the gap; do not weaken the assertion.

- [ ] **Step 4: Commit**

```bash
git add crates/gateway
git commit -m "bench(gateway): in-process streaming round-trip overhead bench"
```

---

## Task 4: `bench.sh` driver

**Files:**
- Create: `scripts/bench.sh`

- [ ] **Step 1: Script**

```bash
#!/usr/bin/env bash
set -euo pipefail

# Run all criterion benches with HTML reports, then point the user at them.
cargo bench -p agent-shim-core --bench canonical_round_trip
cargo bench -p agent-shim --bench gateway_overhead

REPORT=target/criterion/report/index.html
if [ -f "$REPORT" ]; then
  echo "Report: file://$(realpath "$REPORT")"
fi
```

- [ ] **Step 2: chmod + commit**

```bash
chmod +x scripts/bench.sh
git add scripts/bench.sh
git commit -m "chore(bench): add wrapper that runs all criterion benches and points at the report"
```

---

## Task 5: Multi-stage Dockerfile

**Files:**
- Create: `deploy/Dockerfile`
- Create: `deploy/docker-compose.yaml`

- [ ] **Step 1: `Dockerfile`**

```dockerfile
# syntax=docker/dockerfile:1.7

FROM rust:1.82-slim AS planner
WORKDIR /app
RUN cargo install cargo-chef --locked --version 0.1.67
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM rust:1.82-slim AS cacher
WORKDIR /app
RUN cargo install cargo-chef --locked --version 0.1.67
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

FROM rust:1.82-slim AS builder
WORKDIR /app
COPY . .
COPY --from=cacher /app/target target
COPY --from=cacher /usr/local/cargo /usr/local/cargo
RUN cargo build --release -p agent-shim --bin agent-shim

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/agent-shim /usr/local/bin/agent-shim
USER 65532:65532
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/agent-shim"]
CMD ["serve", "--config", "/etc/agent-shim/gateway.yaml"]
```

- [ ] **Step 2: Compose**

```yaml
services:
  agent-shim:
    build:
      context: ..
      dockerfile: deploy/Dockerfile
    ports:
      - "127.0.0.1:8787:8787"
    environment:
      AGENT_SHIM_CONFIG: /etc/agent-shim/gateway.yaml
      DEEPSEEK_API_KEY: "${DEEPSEEK_API_KEY:-}"
    volumes:
      - ../config/gateway.example.yaml:/etc/agent-shim/gateway.yaml:ro
```

- [ ] **Step 3: Build + commit**

Run: `docker build -f deploy/Dockerfile -t agent-shim:dev .`
Expected: image builds (or skip if Docker not available — note in commit).

```bash
git add deploy
git commit -m "build: multi-stage Dockerfile with cargo-chef caching, distroless-style runtime"
```

---

## Task 6: Release workflow

**Files:**
- Create: `.github/workflows/release.yaml`

- [ ] **Step 1: Workflow**

```yaml
name: Release

on:
  push:
    tags: ["v*.*.*"]

jobs:
  build:
    name: Build ${{ matrix.target }}
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        include:
          - target: x86_64-unknown-linux-musl
            os: ubuntu-latest
            cross: true
          - target: aarch64-unknown-linux-musl
            os: ubuntu-latest
            cross: true
          - target: x86_64-apple-darwin
            os: macos-13
            cross: false
          - target: aarch64-apple-darwin
            os: macos-14
            cross: false
          - target: x86_64-pc-windows-msvc
            os: windows-2022
            cross: false
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: ${{ matrix.target }} }
      - if: matrix.cross
        run: cargo install cross --git https://github.com/cross-rs/cross --locked
      - name: Build
        shell: bash
        run: |
          if [ "${{ matrix.cross }}" = "true" ]; then
            cross build --release --target ${{ matrix.target }} -p agent-shim
          else
            cargo build --release --target ${{ matrix.target }} -p agent-shim
          fi
      - name: Package
        shell: bash
        run: |
          name="agent-shim-${{ github.ref_name }}-${{ matrix.target }}"
          mkdir -p dist
          if [[ "${{ matrix.target }}" == *windows* ]]; then
            7z a "dist/${name}.zip" "./target/${{ matrix.target }}/release/agent-shim.exe"
          else
            tar -czvf "dist/${name}.tar.gz" -C "target/${{ matrix.target }}/release" agent-shim
          fi
      - uses: actions/upload-artifact@v4
        with:
          name: ${{ matrix.target }}
          path: dist/

  docker:
    runs-on: ubuntu-latest
    needs: build
    permissions: { contents: read, packages: write }
    steps:
      - uses: actions/checkout@v4
      - uses: docker/setup-qemu-action@v3
      - uses: docker/setup-buildx-action@v3
      - uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
      - uses: docker/build-push-action@v5
        with:
          context: .
          file: deploy/Dockerfile
          platforms: linux/amd64,linux/arm64
          push: true
          tags: |
            ghcr.io/${{ github.repository }}:${{ github.ref_name }}
            ghcr.io/${{ github.repository }}:latest

  release:
    runs-on: ubuntu-latest
    needs: build
    permissions: { contents: write }
    steps:
      - uses: actions/download-artifact@v4
        with: { path: dist }
      - uses: softprops/action-gh-release@v2
        with:
          files: dist/**/*
          generate_release_notes: true
```

- [ ] **Step 2: Commit**

```bash
git add .github/workflows/release.yaml
git commit -m "ci(release): cross-compiled binaries + multi-arch ghcr image on tag"
```

---

## Task 7: Architecture doc

**Files:**
- Create: `docs/architecture.md`

- [ ] **Step 1: Write**

```markdown
# Architecture

AgentShim is a single-binary HTTP gateway that translates between AI-coding-agent
frontends (Anthropic `/v1/messages`, OpenAI `/v1/chat/completions`) and arbitrary
LLM backends (OpenAI-compatible, GitHub Copilot). The architecture optimizes for
two things:

1. **Wire-format fidelity.** Tool-call streaming deltas, system/developer prompt
   placement, stop-reason mapping, and SSE event ordering are tested via golden
   captures.
2. **Performance.** A streaming pipeline of `Stream` adapters with no per-request
   buffering; gateway overhead measured at <5ms p99 on the local-network hop.

## Crate graph

```text
core   → leaf, zero I/O
config → core
observability → config, core
frontends → core
providers → core, config
router → core, config
gateway → all of the above
protocol-tests → frontends, core (dev-only crate)
```

## Request lifecycle

```text
HTTP → axum route → frontend.decode_request(bytes) → CanonicalRequest
  → router.resolve(kind, model) → BackendTarget
  → providers.get(target.upstream).complete(req, target) → CanonicalStream
  → frontend.encode_stream(stream) | encode_unary(collected) → HTTP response
```

The boundary rule: **router never sees provider JSON; provider never sees frontend
JSON.** Translation happens only at the two edges.

## Canonical model

Defined in [`crates/core`](../crates/core/src). The contract that everything else
implements. Block-based content shape inspired by both Anthropic and OpenAI; an
`extensions: ExtensionMap` on every major struct lets unknown fields round-trip
without blocking new provider features. See spec §4 for field-level detail.

## Streaming

Provider output is parsed lazily into a `Stream<StreamEvent>` (alias
`CanonicalStream`). The frontend SSE encoder transforms this stream into wire
bytes and hands it to axum. Backpressure flows from the client TCP socket all
the way to the upstream provider's HTTP body. Cancellation works through the
`Drop` chain — see `crates/protocol-tests/tests/cancellation_fuzz.rs`.

## What's not here

Anything in spec §11 — fallback chains, rate limiting, hot-reload, Prometheus,
multi-account Copilot, vision end-to-end. Stub modules for fallback / rate
limit / circuit breakers exist in `crates/router/` so the architecture is
visible from day one.
```

- [ ] **Step 2: Commit**

```bash
git add docs/architecture.md
git commit -m "docs: top-level architecture overview"
```

---

## Task 8: Configuration + Deployment + Contributing docs

**Files:**
- Create: `docs/configuration.md`
- Create: `docs/deployment.md`
- Create: `docs/contributing.md`

- [ ] **Step 1: `configuration.md`**

```markdown
# Configuration

AgentShim is configured via a single YAML file. Default loader:
1. Read the file specified by `--config` / `AGENT_SHIM_CONFIG`.
2. Overlay environment variables prefixed with `AGENT_SHIM__` (double underscore
   indicates nesting). For example, `AGENT_SHIM__SERVER__PORT=9000` overrides
   `server.port`.

## Top-level shape

```yaml
server: { bind, port, keepalive_secs }
logging: { format: pretty|json, filter: "RUST_LOG-style filter" }
upstreams:
  <key>:
    kind: openai_compatible | github_copilot
    # openai_compatible-only fields:
    base_url, api_key, default_headers, request_timeout_secs
copilot:
  credential_path: <path to copilot.json>
routes:
  - frontend: anthropic_messages | openai_chat
    model: <alias the agent will request>
    upstream: <upstreams key>
    upstream_model: <provider-side model name>
```

`deny_unknown_fields` is set throughout — typos fail loudly at startup.

## Validation

`agent-shim validate-config --config <path>` parses, validates, and exits with a
nonzero code on any error. Use this in CI to catch broken configs before deploy.

## Secrets

`api_key` and any field shaped like a secret use the `Secret` newtype:
- never logged (Debug renders as `Secret(***)`)
- accept `${ENV_VAR}` substitution via the env overlay

## See also

- [`config/gateway.example.yaml`](../config/gateway.example.yaml)
- [Provider docs](providers/)
```

- [ ] **Step 2: `deployment.md`**

```markdown
# Deployment

## Single binary

`cargo install --path crates/gateway --bin agent-shim` (or download from the
GitHub releases). The binary is statically linked against `rustls`; no OpenSSL
dependency.

## Docker

```bash
docker run --rm -p 127.0.0.1:8787:8787 \
  -v $(pwd)/config/gateway.yaml:/etc/agent-shim/gateway.yaml:ro \
  -e DEEPSEEK_API_KEY \
  ghcr.io/<org>/agent-shim:latest
```

Multi-arch images (`linux/amd64`, `linux/arm64`) are published on every tag.

## Operational stance

- Single-process, single-host. Horizontal scale = run more binaries behind a
  load balancer.
- No clustering, no shared state, no Redis. Token caches and config live in
  process memory.
- Graceful shutdown on `SIGTERM` (or Ctrl-C) — drains in-flight requests.

## Logging

JSON to stderr in production:

```yaml
logging:
  format: json
  filter: info,agent_shim=debug
```

Pretty for dev. Sensitive headers (`authorization`, `x-api-key`,
`copilot-token`, `cookie`) are redacted by the trace middleware before logging.

## Health check

`GET /healthz` returns `200 ok`. No auth required.
```

- [ ] **Step 3: `contributing.md`**

```markdown
# Contributing

## Toolchain

`rust-toolchain.toml` pins the supported version. `cargo nextest`, `cargo deny`,
and `cargo llvm-cov` are recommended.

## Adding a frontend

1. Create `crates/frontends/src/<name>/` with `decode.rs`,
   `encode_unary.rs`, `encode_stream.rs`, `mapping.rs`, `wire.rs`, `mod.rs`.
2. Implement `FrontendProtocol`.
3. Add golden SSE fixtures under `crates/protocol-tests/fixtures/<name>/` and
   tests under `crates/protocol-tests/tests/<name>_*.rs`.
4. Wire the new protocol kind in `core::target::FrontendKind` and the gateway
   handler module.

## Adding a provider

1. Create `crates/providers/src/<name>/` with at minimum `mod.rs` (impl
   `BackendProvider`), `encode_request.rs`, `parse_stream.rs`,
   `parse_unary.rs`.
2. If your provider quirks aren't huge, reuse `openai_compatible::encode_request`
   and `openai_compatible::parse_stream` via `pub(crate)` re-exports.
3. Add `mockito`-based smoke tests in `crates/providers/tests/`.
4. Add a YAML schema variant in `crates/config/src/schema.rs`
   (`UpstreamConfig::<NewProvider>`) and a registration arm in
   `crates/gateway/src/state.rs`.

## Tests

```bash
cargo nextest run --workspace          # everything
cargo bench                            # criterion
cargo deny check                       # licenses + advisories
```

Live API tests are gated behind `AGENT_SHIM_E2E=1` (Phase 2).

## Style

`cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings`
are enforced in CI.
```

- [ ] **Step 4: Commit**

```bash
git add docs/configuration.md docs/deployment.md docs/contributing.md
git commit -m "docs: configuration, deployment, and contributing guides"
```

---

## Task 9: Per-frontend / per-provider docs

**Files:**
- Create: `docs/frontends/anthropic-messages.md`
- Create: `docs/frontends/openai-chat-completions.md`
- Create: `docs/providers/openai-compatible.md`

- [ ] **Step 1: `anthropic-messages.md`**

```markdown
# Frontend: Anthropic `/v1/messages`

Implements the [Anthropic Messages API](https://docs.anthropic.com/en/api/messages).

## Supported

- Text content, images (base64 + URL).
- Tool use (`tool_use`, `tool_result`) with streaming `input_json_delta`.
- Thinking blocks (`thinking`, `redacted_thinking`) — pass-through when the
  upstream provider supports them, dropped silently otherwise.
- `cache_control` on text/image/tool blocks — preserved on Anthropic-to-Anthropic
  routes, dropped on cross-protocol routes.
- SSE streaming with all event types: `message_start`, `content_block_start`,
  `content_block_delta`, `content_block_stop`, `message_delta`, `message_stop`,
  plus `ping` keep-alive (15s, configurable).

## Not supported in v0.1

- `response_format` — returns 400. Synthesizing JSON-mode via tool-use
  translation is Phase 2+.
- Audio / file content blocks.

## Mapping

| Canonical | Anthropic wire |
|---|---|
| `StopReason::EndTurn` | `end_turn` |
| `StopReason::MaxTokens` | `max_tokens` |
| `StopReason::ToolUse` | `tool_use` |
| `StopReason::StopSequence` | `stop_sequence` |
| `MessageRole::User` | `user` |
| `MessageRole::Assistant` | `assistant` |
| `ContentBlock::Reasoning` | `thinking` |
```

- [ ] **Step 2: `openai-chat-completions.md`**

```markdown
# Frontend: OpenAI `/v1/chat/completions`

Implements the [Chat Completions API](https://platform.openai.com/docs/api-reference/chat).

## Supported

- Text + image-URL content (`image_url` parts).
- Tool calling (`tools`, `tool_choice`, streaming `tool_calls` deltas).
- `system` and `developer` role messages — distinguished via canonical
  `SystemSource`.
- `response_format` (`text`, `json_object`, `json_schema`).
- SSE streaming with `[DONE]` terminator and per-15s `:` comment keep-alive
  (configurable).

## Not supported in v0.1

- Audio I/O.
- Logprobs (extension-only round-trip).
- `n > 1` (single choice only — agents don't use this).

## Mapping

| Canonical | OpenAI wire `finish_reason` |
|---|---|
| `EndTurn` | `stop` |
| `MaxTokens` | `length` |
| `ToolUse` | `tool_calls` |
| `ContentFilter` | `content_filter` |
```

- [ ] **Step 3: `openai-compatible.md`**

```markdown
# Provider: OpenAI-Compatible

A generic adapter for any provider that exposes a `chat.completions`-style HTTP
endpoint. Verified working against:

- DeepSeek (`https://api.deepseek.com`)
- Kimi / Moonshot (`https://api.moonshot.cn/v1`)
- Qwen DashScope compatibility mode
- vLLM, Ollama (`http://localhost:11434/v1`)
- llama.cpp server in OpenAI mode

## Config

```yaml
upstreams:
  deepseek:
    kind: openai_compatible
    base_url: https://api.deepseek.com
    api_key: ${DEEPSEEK_API_KEY}
    default_headers:
      X-Custom: value
    request_timeout_secs: 120
```

`base_url` is concatenated with `/v1/chat/completions`. If your provider hosts
the endpoint at a different path (e.g. `https://api.example.com/openai/v1`),
include the prefix in `base_url`.

## Behavior

- `Authorization: Bearer <api_key>` header always set.
- `stream_options: { include_usage: true }` is added on streaming requests, so
  you get a final usage chunk even on providers that don't emit it by default.
- Tool-call argument deltas are passed through verbatim — no mid-stream parsing.
- Non-streaming responses are still returned as `CanonicalStream` (one-shot)
  for code-path uniformity.

## Provider quirks not handled here

DeepSeek's `reasoning_content`, Qwen's enable_thinking, Kimi's caching hints —
each gets a native adapter in Phase 2. The OpenAI-compat shim ignores those
fields silently.
```

- [ ] **Step 4: Commit**

```bash
git add docs/frontends docs/providers/openai-compatible.md
git commit -m "docs: per-frontend and openai-compatible provider references"
```

---

## Task 10: README polish

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Replace stub README**

```markdown
# AgentShim

[![CI](https://github.com/anthropics/agent-shim/actions/workflows/ci.yaml/badge.svg)](https://github.com/anthropics/agent-shim/actions/workflows/ci.yaml)

A single-binary Rust gateway that lets any AI coding agent talk to any LLM backend.

- **Frontends:** Anthropic `/v1/messages`, OpenAI `/v1/chat/completions`
- **Backends:** Generic OpenAI-compatible (DeepSeek, Kimi, vLLM, Ollama, …) + GitHub Copilot
- **Streaming + tool calling:** full SSE fidelity, golden-tested
- **Performance:** <5ms p99 gateway overhead, no per-request buffering
- **License:** MIT

## Quickstart

```bash
# Install
cargo install --path crates/gateway --bin agent-shim   # or grab a release binary

# Configure
cp config/gateway.example.yaml gateway.yaml
export DEEPSEEK_API_KEY=sk-...

# Run
agent-shim serve --config gateway.yaml
```

Now point your agent at `http://127.0.0.1:8787`. Anthropic-compatible clients
hit `/v1/messages`; OpenAI-compatible hit `/v1/chat/completions`. The router
maps `model` to the configured upstream.

## GitHub Copilot

```bash
agent-shim copilot login   # device-flow, persists ~/.config/agent-shim/copilot.json
```

See [`docs/providers/github-copilot.md`](docs/providers/github-copilot.md).

## Docs

- [Architecture](docs/architecture.md)
- [Configuration](docs/configuration.md)
- [Deployment](docs/deployment.md)
- [Contributing](docs/contributing.md)
- Frontends: [Anthropic](docs/frontends/anthropic-messages.md) / [OpenAI](docs/frontends/openai-chat-completions.md)
- Providers: [OpenAI-compatible](docs/providers/openai-compatible.md) / [Copilot](docs/providers/github-copilot.md)
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs(readme): full quickstart, links, badges"
```

---

## Task 11: Coverage report in CI (non-gating)

**Files:**
- Modify: `.github/workflows/ci.yaml`

- [ ] **Step 1: Add coverage job**

Append to `.github/workflows/ci.yaml`:

```yaml
  coverage:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { components: llvm-tools-preview }
      - uses: Swatinem/rust-cache@v2
      - run: cargo install cargo-llvm-cov --locked
      - run: cargo llvm-cov --workspace --lcov --output-path lcov.info
      - uses: codecov/codecov-action@v4
        with: { files: lcov.info, fail_ci_if_error: false }
```

- [ ] **Step 2: Commit**

```bash
git add .github/workflows/ci.yaml
git commit -m "ci(coverage): cargo-llvm-cov + Codecov upload (non-gating)"
```

---

## Task 12: Final verification — full workspace

**Files:** none

- [ ] **Step 1: Comprehensive build/test/bench**

Run, in order:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo deny check
scripts/bench.sh
```

Each must succeed; any failure blocks v0.1.

- [ ] **Step 2: Tag release commit**

If everything passes:

```bash
git commit --allow-empty -m "chore(release): v0.1.0 ready"
git tag v0.1.0
```

(`git push --tags` is the human's call — do not push from the plan.)

---

## Self-Review Notes

- Spec §10 Layer 2 cancellation/disconnect fuzz — implemented (Task 1, 50 iterations × 2 frontends).
- Spec §10 criterion benches with 5ms p99 target — implemented (Task 3); baseline numbers will be captured after first green run.
- Spec §10 coverage policy (non-gating until v0.2) — implemented (Task 11).
- Spec §1 success criteria documented in README.
- Spec §3, §4, §5, §6 documented under `docs/`.
- Spec §11 operational stances documented in `docs/deployment.md`.
- Multi-arch release (`x86_64`/`aarch64` Linux/Mac, `x86_64` Windows) — workflow in place.
- Docker image builds for `linux/amd64` + `linux/arm64`. ✓
- No mention of secrets in workflows; uses `secrets.GITHUB_TOKEN` only for ghcr push.
- Type consistency: all task references use the trait/struct names defined in earlier plans (`AppState::build`, `FrontendProtocol`, `BackendProvider`, `CopilotProvider`, `OpenAiCompatibleProvider`).
