# Plan 01 — Outbound traceparent + ProviderHttpClient (Phase 6)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-12-phase-6-cost-aware-gateway-design.md`](../specs/2026-05-12-phase-6-cost-aware-gateway-design.md) (D5, §4.1 D4, §2 frozen-core scope change).

**Goal:** Inject W3C `traceparent` (and `tracestate`) onto every upstream HTTP call, completing end-to-end distributed traces. Closes the v0.5 P03 T4 step 5 deferral.

**Architecture:** A new `ProviderHttpClient` wrapper around `reqwest::Client`, owned by `crates/providers/src/http_client.rs` (already centralizes 5 of 7 client constructions). The wrapper's `post`/`get` methods auto-inject `traceparent` from `Span::current()`'s OTel context before `.send()`. Every provider's `client: reqwest::Client` field becomes `client: ProviderHttpClient`. The existing `http_client::build` is promoted from `pub(crate)` to `pub` and returns `ProviderHttpClient` instead of `reqwest::Client`.

**Tech stack:** Adds `agent-shim-observability` to `crates/providers/Cargo.toml` as a workspace dep (one new crate-level edge — providers → observability). No new external crates; uses `opentelemetry::propagation::TraceContextPropagator` already in the workspace via v0.5.

**Frozen-core impact:** This plan **lifts the freeze on `crates/providers/src/`** per spec §2. Discipline: exactly one new module (`http_client.rs` already exists — we modify it) + one new pub type (`ProviderHttpClient`) + mechanical type-renames on each provider's `client` field. No behavioural changes to provider HTTP logic, no signature changes on `BackendProvider`.

**Test target:** 641 (current) → 643 (+2 integration).

---

## File Structure

`crates/providers/src/`:
- Modify: `http_client.rs` — promote `build` to `pub`, return `ProviderHttpClient`. New struct definition + impl block.
- Modify: `lib.rs` — change `pub(crate) mod http_client;` to `pub mod http_client;`, add `pub use http_client::ProviderHttpClient;`.
- Modify: `anthropic/mod.rs`, `deepseek/mod.rs`, `gemini/mod.rs`, `openai_compatible/mod.rs`, `github_copilot/mod.rs` — change `client: reqwest::Client` field type to `client: ProviderHttpClient`. Each provider's `.post()` / `.get()` call sites work unchanged because `ProviderHttpClient` re-exposes those methods.

`crates/providers/Cargo.toml`:
- Add `agent-shim-observability = { path = "../observability" }` to `[dependencies]`.

`crates/observability/src/otel/`:
- Modify: `extract.rs` — rename/refactor to expose `inject_traceparent_into_headers(&mut HeaderMap)`. The reqwest `RequestBuilder` uses `http::HeaderMap`, same as the existing extractor uses.

`crates/gateway/tests/`:
- Create: `outbound_traceparent.rs` — mockito captures outbound request, asserts `traceparent` header present and well-formed.
- Create: `outbound_traceparent_inherits_inbound.rs` — when inbound `traceparent` set, outbound trace_id matches.

---

## Tasks

### Task 1: Add observability dep + promote `http_client` module

**Files:**
- Modify: `crates/providers/Cargo.toml`
- Modify: `crates/providers/src/lib.rs`

- [ ] **Step 1: Add observability workspace dep**

In `crates/providers/Cargo.toml`, add to `[dependencies]`:

```toml
agent-shim-observability = { path = "../observability" }
```

(Keep alphabetical with the other `agent-shim-*` deps.)

- [ ] **Step 2: Promote http_client module to pub**

In `crates/providers/src/lib.rs`, change:

```rust
pub(crate) mod http_client;
```

to:

```rust
pub mod http_client;
pub use http_client::ProviderHttpClient;
```

- [ ] **Step 3: Verify it still compiles**

```bash
cargo check -p agent-shim-providers 2>&1 | tail -5
```

Expected: clean (the re-export refers to a type we haven't added yet — this step expects FAIL with "cannot find type `ProviderHttpClient`"). That's the RED for Task 2.

- [ ] **Step 4: Commit deferred to end of Task 2**

Don't commit yet — Task 2's struct addition is needed for the workspace to compile.

---

### Task 2: Define `ProviderHttpClient` wrapper + outbound injection helper

**Files:**
- Modify: `crates/providers/src/http_client.rs`
- Modify: `crates/observability/src/otel/extract.rs` (rename internal helper)
- Modify: `crates/observability/src/otel/mod.rs`
- Modify: `crates/observability/src/lib.rs`

- [ ] **Step 1: Add outbound-injection helper to observability**

The v0.5 `extract.rs` defined `extract_context_from_headers` for inbound. Add a sibling for outbound. In `crates/observability/src/otel/extract.rs`, append:

```rust
use opentelemetry::propagation::Injector;

struct HeaderMapInjector<'a>(&'a mut http::HeaderMap);

impl<'a> Injector for HeaderMapInjector<'a> {
    fn set(&mut self, key: &str, value: String) {
        if let Ok(name) = http::header::HeaderName::try_from(key) {
            if let Ok(val) = http::HeaderValue::try_from(&value) {
                self.0.insert(name, val);
            }
        }
        // Malformed key/value pairs are silently dropped — observability
        // is diagnostic, never an admission gate. See spec §6.4.
    }
}

/// Inject the current `tracing::Span`'s OTel context as W3C `traceparent`
/// (and optional `tracestate`) into the given `HeaderMap`. Idempotent;
/// safe to call before every outbound HTTP request.
///
/// When `Span::current()` carries no OTel parent context (e.g. the request
/// arrived without a `traceparent` header and no internal span attached
/// one), the propagator writes nothing and this is a no-op. No 5xx is
/// ever produced from injection failure.
pub fn inject_context_into_headers(headers: &mut http::HeaderMap) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    let cx = tracing::Span::current().context();
    let propagator = TraceContextPropagator::new();
    propagator.inject_context(&cx, &mut HeaderMapInjector(headers));
}
```

- [ ] **Step 2: Re-export from otel/mod.rs and lib.rs**

In `crates/observability/src/otel/mod.rs`, change the re-export line:

```rust
pub use extract::{extract_context_from_headers, inject_context_into_headers};
```

In `crates/observability/src/lib.rs`, mirror:

```rust
pub use otel::{extract_context_from_headers, inject_context_into_headers, OtelHandle};
```

- [ ] **Step 3: Unit test the injector**

Append to `crates/observability/src/otel/extract.rs`'s `#[cfg(test)] mod tests`:

```rust
#[test]
fn inject_into_empty_headers_with_no_parent_writes_nothing() {
    let mut h = http::HeaderMap::new();
    inject_context_into_headers(&mut h);
    // With no active span carrying remote context, the propagator
    // writes nothing — the headers stay empty.
    assert!(h.is_empty(), "expected no injection without parent ctx; got {:?}", h);
}

#[tokio::test]
async fn inject_writes_traceparent_when_parent_set() {
    use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
    use opentelemetry_sdk::trace::TracerProvider;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    use tracing_subscriber::layer::SubscriberExt;
    use opentelemetry::trace::TracerProvider as _;

    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("test"));
    let subscriber = tracing_subscriber::registry().with(layer);
    let _g = tracing::subscriber::set_default(subscriber);

    // Build an inbound HeaderMap with a known traceparent, parse it,
    // attach the resulting context to a span, then inject from inside
    // that span and verify the trace_id propagates.
    let mut inbound = http::HeaderMap::new();
    inbound.insert(
        "traceparent",
        "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".parse().unwrap(),
    );
    let parent_ctx = extract_context_from_headers(&inbound);

    let span = tracing::info_span!("outbound_test");
    span.set_parent(parent_ctx);
    let _e = span.enter();

    let mut outbound = http::HeaderMap::new();
    inject_context_into_headers(&mut outbound);

    let tp = outbound.get("traceparent")
        .expect("traceparent must be injected when parent context is active")
        .to_str().unwrap();
    // W3C format: 00-<32 hex trace_id>-<16 hex span_id>-<flags>
    assert!(tp.contains("0af7651916cd43dd8448eb211c80319c"),
            "outbound trace_id must match inbound; got: {tp}");
}
```

- [ ] **Step 4: Define `ProviderHttpClient` in http_client.rs**

Append to `crates/providers/src/http_client.rs`, after the existing module-level docs and before the `CONNECT_TIMEOUT` const, add a new struct + impl. Then modify `build` to return the new type.

```rust
use agent_shim_observability::inject_context_into_headers;

/// Wrapper around `reqwest::Client` that auto-injects W3C `traceparent`
/// (and `tracestate`) into every outgoing request based on the current
/// `tracing::Span`'s OTel context.
///
/// All upstream providers hold this instead of a raw `reqwest::Client`
/// so distributed tracing continues across the gateway → upstream hop.
/// When no parent context is active, injection is a no-op — the wrapper
/// adds no observable overhead beyond a single HeaderMap probe.
///
/// Plan 06 P01 T2. The wrapper exposes `post`, `get`, `delete` so
/// existing provider call sites work unchanged.
#[derive(Clone, Debug)]
pub struct ProviderHttpClient {
    inner: reqwest::Client,
}

impl ProviderHttpClient {
    /// Wrap an existing `reqwest::Client`. Internal — external callers
    /// should go through [`build`].
    pub(crate) fn from_inner(inner: reqwest::Client) -> Self {
        Self { inner }
    }

    /// Start a POST request to `url` with `traceparent` auto-injected.
    pub fn post<U: reqwest::IntoUrl>(&self, url: U) -> reqwest::RequestBuilder {
        let mut headers = http::HeaderMap::new();
        inject_context_into_headers(&mut headers);
        self.inner.post(url).headers(headers)
    }

    /// Start a GET request to `url` with `traceparent` auto-injected.
    pub fn get<U: reqwest::IntoUrl>(&self, url: U) -> reqwest::RequestBuilder {
        let mut headers = http::HeaderMap::new();
        inject_context_into_headers(&mut headers);
        self.inner.get(url).headers(headers)
    }

    /// Start a DELETE request with `traceparent` auto-injected.
    /// Included for symmetry — none of the v0.6 providers use DELETE today.
    #[allow(dead_code)]
    pub fn delete<U: reqwest::IntoUrl>(&self, url: U) -> reqwest::RequestBuilder {
        let mut headers = http::HeaderMap::new();
        inject_context_into_headers(&mut headers);
        self.inner.delete(url).headers(headers)
    }

    /// Escape hatch: return the inner `reqwest::Client` for code paths
    /// that build their own `RequestBuilder` shape (e.g. `multipart`).
    /// Outbound `traceparent` will NOT be auto-injected on requests
    /// constructed this way — callers must handle it themselves.
    pub fn inner(&self) -> &reqwest::Client {
        &self.inner
    }
}
```

- [ ] **Step 5: Update `build` to return `ProviderHttpClient`**

Change the signature and body of `build` in `crates/providers/src/http_client.rs`:

```rust
pub fn build(read_timeout: Duration) -> Result<ProviderHttpClient, ProviderError> {
    build_with_keylog(read_timeout, std::env::var_os("SSLKEYLOGFILE").is_some())
}

fn build_with_keylog(
    read_timeout: Duration,
    keylog: bool,
) -> Result<ProviderHttpClient, ProviderError> {
    // ... existing body unchanged until the final .build() call ...
    let client = builder
        .build()
        .map_err(|e| ProviderError::Network(e.to_string()))?;
    Ok(ProviderHttpClient::from_inner(client))
}
```

The two existing unit tests at the bottom of the file (`build_succeeds_without_keylog`, `build_succeeds_with_keylog`) keep their existing shape — `result.is_ok()` is unaffected by the type change.

- [ ] **Step 6: Build & test observability + providers crates**

```bash
cargo build -p agent-shim-observability -p agent-shim-providers 2>&1 | tail -10
cargo test -p agent-shim-observability --lib 2>&1 | tail -10
```

Expected: observability builds clean; observability lib tests pass including the 2 new tests added in Step 3 (total observability tests grows by 2). Providers fails to build because callsites still reference removed methods — that's the RED for Task 3.

- [ ] **Step 7: Don't commit yet**

The workspace doesn't compile in this state. Commit after Task 3 wires up the providers.

---

### Task 3: Rewire every provider's `client` field

**Files:**
- Modify: `crates/providers/src/anthropic/mod.rs`
- Modify: `crates/providers/src/deepseek/mod.rs`
- Modify: `crates/providers/src/gemini/mod.rs`
- Modify: `crates/providers/src/openai_compatible/mod.rs`
- Modify: `crates/providers/src/github_copilot/mod.rs`
- Modify (likely): `crates/providers/src/oai_chat_wire/chat_sse_parser.rs` (one test client — check)

- [ ] **Step 1: Anthropic**

In `crates/providers/src/anthropic/mod.rs:40`, change:

```rust
client: reqwest::Client,
```

to:

```rust
client: crate::ProviderHttpClient,
```

Verify the existing `let client = http_client::build(...)?;` at line 62 — it already returns `ProviderHttpClient` after Task 2's change. No further code change in this file.

- [ ] **Step 2: Deepseek**

`crates/providers/src/deepseek/mod.rs:32` — same change.

- [ ] **Step 3: Gemini**

`crates/providers/src/gemini/mod.rs:55` — same change.

- [ ] **Step 4: OpenAI-compatible**

`crates/providers/src/openai_compatible/mod.rs:19` — same change.

- [ ] **Step 5: GitHub Copilot**

`crates/providers/src/github_copilot/mod.rs` — the field type. Search for `client: reqwest::Client` inside that file; if multiple matches (token_manager etc.), only change the one on the `CopilotProvider` struct that's constructed via `http_client::build(...)` at line 52. The auth-token-fetching `reqwest::Client::builder()` at `auth.rs:21` is a SEPARATE client (no SSE streaming, no need for `traceparent` injection on a private OAuth call) and stays a plain `reqwest::Client`.

- [ ] **Step 6: Test client in oai_chat_wire/chat_sse_parser.rs (likely unaffected)**

`chat_sse_parser.rs:396` uses `reqwest::Client::new()` — this is inside `#[cfg(test)]` for the wire-protocol parser test. Leave it as `reqwest::Client::new()` — it's not part of the production path.

- [ ] **Step 7: Build the workspace**

```bash
cargo build --workspace 2>&1 | tail -15
```

Expected: clean. Every provider's `client.post(...)` / `client.get(...)` call site works because `ProviderHttpClient` exposes those same methods.

- [ ] **Step 8: Run provider tests**

```bash
cargo test -p agent-shim-providers 2>&1 | tail -10
```

Expected: all pass. Provider unit tests don't exercise traceparent — they just exercise wire-protocol encoding/decoding, unchanged.

- [ ] **Step 9: Commit Tasks 1–3 together**

```bash
git add crates/providers/Cargo.toml crates/providers/src/ crates/observability/src/
git commit -m "feat(providers): ProviderHttpClient wraps reqwest with outbound traceparent injection (Plan 01 P01 T1-T3)"
```

---

### Task 4: Integration test — outbound carries traceparent

**Files:**
- Create: `crates/gateway/tests/outbound_traceparent.rs`

- [ ] **Step 1: Add mockito to gateway dev-deps if missing**

```bash
grep mockito crates/gateway/Cargo.toml
```

Expected: present (Phase 4 added it). If missing, add `mockito.workspace = true` to `[dev-dependencies]`.

- [ ] **Step 2: Write the test**

Create `crates/gateway/tests/outbound_traceparent.rs`:

```rust
//! Plan 01 P01 T4: outbound HTTP requests carry a W3C `traceparent`
//! header injected by `ProviderHttpClient`.
//!
//! Test strategy: stand up a mockito server, configure the gateway with
//! an OpenAI-compatible upstream pointing at mockito, hit the public
//! port. The mockito matcher requires the outbound request to carry a
//! well-formed `traceparent` header.

use std::net::SocketAddr;

async fn pick_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
        .local_addr().unwrap().port()
}

#[tokio::test]
async fn outbound_request_carries_traceparent_header() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .match_header(
            "traceparent",
            mockito::Matcher::Regex(
                r"^00-[0-9a-f]{32}-[0-9a-f]{16}-(00|01)$".to_string(),
            ),
        )
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("data: [DONE]\n\n")
        .create_async()
        .await;

    let public_port = pick_port().await;
    let yaml = format!(
        r#"
server: {{bind: 127.0.0.1, port: {public_port}}}
upstreams:
  m:
    type: open_ai_compatible
    base_url: {}
    api_key: dummy
    tier: standard
routes:
  - frontend: openai_chat
    model: x
    upstream: m
    upstream_model: x
"#,
        server.url()
    );
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(&yaml).unwrap();
    let (state, _reload_rx) = agent_shim_gateway::state::AppState::new(cfg).await;
    let public_addr: SocketAddr = format!("127.0.0.1:{public_port}").parse().unwrap();
    let listener = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let app = agent_shim_gateway::server::build_router(state);
    tokio::spawn(async move { let _ = axum::serve(listener, app).await; });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let _ = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .body(r#"{"model":"x","messages":[{"role":"user","content":"hi"}],"stream":true}"#)
        .send()
        .await;

    // mockito.assert() panics if the matcher didn't fire — i.e. if the
    // outbound request lacked a well-formed traceparent header.
    mock.assert_async().await;
}
```

- [ ] **Step 3: Note about config schema dependency**

This test relies on `tier: standard` being parseable. The schema field is added in P03. **For P01 we expect this test to FAIL at config parse time until P03 lands.** Acceptable — P01 leaves the test file in place with a comment marker for P03 to remove.

For the immediate-pass version of this task, **omit** the `tier:` line from the YAML — the test still works because no route uses `min_tier`. Once P03 makes `tier` required, P03 T2 will add `tier: standard` back. Comment the test file accordingly:

```rust
//! NOTE: `upstreams.m.tier` is omitted here — added by P03 T2 once the
//! schema makes it required. Until then, the field defaults to None
//! and the test runs against the v0.5 schema shape.
```

- [ ] **Step 4: Run the test**

```bash
cargo test -p agent-shim --test outbound_traceparent 2>&1 | tail -10
```

Expected: 1 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/gateway/tests/outbound_traceparent.rs
git commit -m "test(gateway): outbound traceparent injection end-to-end (Plan 01 P01 T4)"
```

---

### Task 5: Integration test — outbound trace_id matches inbound

**Files:**
- Create: `crates/gateway/tests/outbound_traceparent_inherits_inbound.rs`

- [ ] **Step 1: Write the test**

Create `crates/gateway/tests/outbound_traceparent_inherits_inbound.rs`:

```rust
//! Plan 01 P01 T5: outbound `traceparent` inherits the inbound trace_id.
//!
//! The W3C trace_id is the 32-hex segment immediately after the version.
//! When an inbound request carries `traceparent: 00-<id>-<span>-<flags>`,
//! the outbound request to the upstream MUST carry the same `<id>` —
//! that's what makes it the same distributed trace.

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

const INBOUND_TRACE_ID: &str = "0af7651916cd43dd8448eb211c80319c";

async fn pick_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
        .local_addr().unwrap().port()
}

#[tokio::test]
async fn outbound_trace_id_matches_inbound() {
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let mut server = mockito::Server::new_async().await;
    let captured_clone = captured.clone();
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("data: [DONE]\n\n")
        .match_request(move |req| {
            // Capture the outbound traceparent for assertion after the call.
            if let Some(v) = req.header("traceparent").last() {
                if let Ok(s) = v.to_str() {
                    let captured_clone = captured_clone.clone();
                    let s = s.to_string();
                    tokio::spawn(async move {
                        *captured_clone.lock().await = Some(s);
                    });
                }
            }
            true
        })
        .create_async()
        .await;

    let public_port = pick_port().await;
    let yaml = format!(
        r#"
server: {{bind: 127.0.0.1, port: {public_port}}}
upstreams:
  m:
    type: open_ai_compatible
    base_url: {}
    api_key: dummy
routes:
  - frontend: openai_chat
    model: x
    upstream: m
    upstream_model: x
"#,
        server.url()
    );
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(&yaml).unwrap();
    let (state, _reload_rx) = agent_shim_gateway::state::AppState::new(cfg).await;
    let public_addr: SocketAddr = format!("127.0.0.1:{public_port}").parse().unwrap();
    let listener = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let app = agent_shim_gateway::server::build_router(state);
    tokio::spawn(async move { let _ = axum::serve(listener, app).await; });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let inbound_tp = format!("00-{INBOUND_TRACE_ID}-b7ad6b7169203331-01");
    let _ = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .header("traceparent", inbound_tp)
        .body(r#"{"model":"x","messages":[{"role":"user","content":"hi"}],"stream":true}"#)
        .send()
        .await;

    mock.assert_async().await;

    // Allow the spawned capture task to settle.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let outbound_tp = captured.lock().await.clone()
        .expect("outbound traceparent must have been captured");
    assert!(
        outbound_tp.contains(INBOUND_TRACE_ID),
        "outbound trace_id must match inbound; inbound={INBOUND_TRACE_ID} outbound={outbound_tp}",
    );
}
```

- [ ] **Step 2: Run the test**

```bash
cargo test -p agent-shim --test outbound_traceparent_inherits_inbound 2>&1 | tail -10
```

Expected: 1 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/gateway/tests/outbound_traceparent_inherits_inbound.rs
git commit -m "test(gateway): outbound trace_id matches inbound (Plan 01 P01 T5)"
```

---

### Task 6: Spec compliance + code quality review

- [ ] **Step 1: Reviewer dispatch (spec compliance)**

Dispatch a reviewer subagent with the following prompt:

> Review commits Plan 01 T1..T5 against spec `docs/superpowers/specs/2026-05-12-phase-6-cost-aware-gateway-design.md`. Verify:
>
> 1. **§2.1 frozen-core scope** — `git diff a0139fc -- crates/core/ crates/frontends/` is EMPTY.
> 2. **§2.2 providers/src/ discipline** — `git diff a0139fc -- crates/providers/src/` contains only: (a) `http_client.rs` modifications, (b) one-line `lib.rs` re-export change, (c) mechanical `client: reqwest::Client` → `client: ProviderHttpClient` field renames on 5 provider mods. NO other changes. List every hunk and classify.
> 3. **§3.2 boundary rule** — providers crate now depends on observability (one new edge). Verify `crates/providers/Cargo.toml` has exactly that one new dep; no other crate dep changes.
> 4. **§4.1 D4 mechanism** — `ProviderHttpClient` wraps `reqwest::Client` and auto-injects `traceparent` via `inject_context_into_headers` on every `post`/`get`/`delete`. Confirm `inner()` escape hatch exists but is documented as bypassing injection.
> 5. **§6.4 observability degradation** — injection failures (malformed HeaderName/HeaderValue) are silent; no 5xx codepath from injection.
> 6. **Test coverage** — both integration tests added; the trace_id continuity test asserts string-containment (not full equality, which would be wrong because the span_id segment differs).

- [ ] **Step 2: Reviewer dispatch (code quality)**

Dispatch a second reviewer with:

> Review commits Plan 01 T1..T5 for code quality.
>
> 1. `ProviderHttpClient::post/get/delete` — each call allocates a fresh HeaderMap then merges via `.headers(headers)`. Is the merge correct (won't clobber caller-supplied headers via `.header()` chained after `.post()`)? Verify against `reqwest::RequestBuilder::headers` semantics.
> 2. `inject_context_into_headers` — uses `TraceContextPropagator::new()` per call. Cheap, but consider a thread-local once-init.
> 3. Tests use `tokio::time::sleep(100ms)` to wait for the server to come up. Flaky on slow CI? Compare against neighbouring `crates/gateway/tests/*.rs` for the established pattern.
> 4. `outbound_traceparent_inherits_inbound.rs` uses `tokio::spawn` inside the matcher closure to update the `captured` Mutex. Is the matcher run in the test thread or mockito's worker thread? Confirm the `tokio::spawn` is appropriate.
> 5. The `inner()` escape hatch on `ProviderHttpClient` — needed for v0.6? Anyone calling it bypasses tracing. If unused today, consider removing.

- [ ] **Step 3: Apply CRITICAL/HIGH findings**

If reviewers flag CRITICAL or HIGH issues, fix them and commit as a followup:

```bash
git commit -m "fix(providers): address P01 review findings"
```

---

## Done when

- [ ] Workspace test count ≥ 643 (was 641, +2 integration).
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] `git diff a0139fc -- crates/core/ crates/frontends/` is empty.
- [ ] `git diff a0139fc -- crates/providers/src/` matches the discipline rule (only http_client.rs + lib.rs re-export + 5 field-type renames).
- [ ] Outbound `traceparent` header is well-formed on every upstream call (verified by mockito Regex matcher).
- [ ] When inbound request carries `traceparent`, the outbound `traceparent` contains the same 32-hex trace_id.
- [ ] T6 reviews clear of CRITICAL/HIGH findings.
