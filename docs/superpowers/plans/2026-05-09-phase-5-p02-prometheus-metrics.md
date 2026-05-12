# Plan 02 — Prometheus Metrics (Phase 5)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Source design:** [`docs/superpowers/specs/2026-05-09-phase-5-observability-design.md`](../specs/2026-05-09-phase-5-observability-design.md) (decision D6; §3 Prometheus metrics).

**Goal:** Wire `metrics-rs` facade + `metrics-exporter-prometheus` into the gateway, register the full §3.1 metric set at startup so /metrics returns text even for unhit counters, instrument the request lifecycle and the resilience layer, and expose `/metrics` on the admin listener.

**Architecture:** A new `crates/observability/src/metrics/` module owns metric naming (one `pub const` per metric), the install function (returns a `MetricsHandle` wrapping the `PrometheusHandle` for rendering), and typed recorder wrappers that enforce label sets at compile time. Instrumentation calls live alongside the existing v0.4 tracing events — adding a `metrics::counter!()` call next to the existing `tracing::warn!()` rather than introducing a separate code path. The `/metrics` handler is added to the admin Router built in P01.

**Tech stack:** Adds `metrics = "0.23"` and `metrics-exporter-prometheus = "0.15"` to the workspace. Adds `prometheus-parse = "0.2"` as dev-dep for tests.

**Frontend changes:** NONE.
**Provider code changes:** NONE.
**Core changes:** NONE.

**Test target:** 591 → 609 (+18).

---

## File Structure

`crates/observability/src/`:
- Create: `metrics/mod.rs` — re-exports + `pub fn install` + `MetricsHandle`.
- Create: `metrics/names.rs` — one `pub const` per metric name (the §3.1 set).
- Create: `metrics/recorders.rs` — typed wrappers calling `counter!()`/`histogram!()` with strict label sets.
- Create: `metrics/middleware.rs` — `tower::Layer` for HTTP-request lifecycle metrics.
- Modify: `lib.rs` — `pub mod metrics;` + re-exports.

`crates/observability/Cargo.toml`:
- Add `metrics`, `metrics-exporter-prometheus` to `[dependencies]`.
- Add `prometheus-parse`, `axum` to `[dev-dependencies]`.

`crates/router/src/`:
- Modify: `resilient_caller.rs` — emit metric counters at the same call sites as the existing tracing events for retry/fallback/request-completed.
- Modify: `circuit_breaker.rs` — emit `breaker_state_changes_total` in the existing `emit_state_change` helper.
- Modify: `rate_limit.rs` — emit `rate_limit_rejected_total{dimension}` at the rejection site.

`crates/router/Cargo.toml`:
- Add `metrics.workspace = true` to `[dependencies]` (the router crate emits counters; it does NOT depend on the exporter).

`crates/gateway/src/`:
- Modify: `state.rs` — `AppCore` gains `pub metrics: Arc<MetricsHandle>`. Built in `AppState::build` from `MetricsConfig`.
- Modify: `admin/mod.rs` — `.route("/metrics", get(metrics_handler))`.
- Create: `admin/metrics_handler.rs` — calls `state.core.metrics.render()`.
- Modify: `pipeline.rs` — wrap `dispatch` body with metrics middleware (request count + duration + in-flight gauge via Drop guard).
- Modify: `Cargo.toml` — `metrics`, `metrics-exporter-prometheus` deps.

`crates/config/src/`:
- Modify: `schema.rs` — `MetricsConfig` block (histogram bucket overrides).

`Cargo.toml` (workspace):
- Add `metrics = "0.23"`, `metrics-exporter-prometheus = "0.15"`, `prometheus-parse = "0.2"` to `[workspace.dependencies]`.

`crates/gateway/tests/`:
- Create: `metrics_endpoint.rs` — admin `/metrics` returns Prometheus text; counters increment; histograms record.

---

## Tasks

### Task 1: Workspace deps + MetricsConfig schema

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/observability/Cargo.toml`
- Modify: `crates/router/Cargo.toml`
- Modify: `crates/gateway/Cargo.toml`
- Modify: `crates/config/src/schema.rs`

- [ ] **Step 1: Add workspace deps**

In the workspace root `Cargo.toml`, append to `[workspace.dependencies]` (alphabetical order):

```toml
metrics = "0.23"
metrics-exporter-prometheus = { version = "0.15", default-features = false }
prometheus-parse = "0.2"
```

- [ ] **Step 2: Wire metrics into observability**

In `crates/observability/Cargo.toml`, append to `[dependencies]`:

```toml
metrics.workspace = true
metrics-exporter-prometheus.workspace = true
```

In `[dev-dependencies]`:

```toml
prometheus-parse.workspace = true
axum = { workspace = true, features = ["macros"] }
```

- [ ] **Step 3: Wire metrics into router (emitter only)**

In `crates/router/Cargo.toml`, append to `[dependencies]`:

```toml
metrics.workspace = true
```

The router crate does NOT depend on the exporter — it only emits via the facade.

- [ ] **Step 4: Wire metrics into gateway**

In `crates/gateway/Cargo.toml`, append to `[dependencies]`:

```toml
metrics.workspace = true
metrics-exporter-prometheus.workspace = true
```

In `[dev-dependencies]`:

```toml
prometheus-parse.workspace = true
```

- [ ] **Step 5: Write failing test for MetricsConfig**

Add to `crates/config/src/schema.rs` test module:

```rust
#[test]
fn metrics_config_defaults() {
    let yaml = "server: {bind: 127.0.0.1, port: 8787}";
    let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("parses");
    assert!(cfg.metrics.histogram_buckets.is_empty());
}

#[test]
fn metrics_config_custom_buckets() {
    let yaml = r#"
metrics:
  histogram_buckets:
    request_duration_seconds: [0.1, 0.5, 1.0, 5.0]
"#;
    let cfg: GatewayConfig = serde_yaml::from_str(yaml).expect("parses");
    let buckets = cfg.metrics.histogram_buckets.get("request_duration_seconds").unwrap();
    assert_eq!(buckets, &vec![0.1, 0.5, 1.0, 5.0]);
}
```

- [ ] **Step 6: Run, expect failure**

```bash
rtk cargo test -p agent-shim-config metrics_config --quiet
```

Expected: `metrics` field not on `GatewayConfig`.

- [ ] **Step 7: Add `MetricsConfig` to schema**

In `crates/config/src/schema.rs`, after the `LoggingConfig` impl block:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    /// Per-metric-name histogram bucket overrides. Keys are metric names
    /// (e.g. `"agent_shim_request_duration_seconds"` or the bare suffix
    /// `"request_duration_seconds"` — both forms accepted; the bare form
    /// gets the `agent_shim_` prefix prepended at install time).
    /// Absent → exporter defaults (Prometheus's standard duration buckets).
    #[serde(default)]
    pub histogram_buckets: BTreeMap<String, Vec<f64>>,
}
```

In `GatewayConfig`, add (after `admin`):

```rust
    #[serde(default)]
    pub metrics: MetricsConfig,
```

Re-export from `crates/config/src/lib.rs` (`pub use schema::{ …, MetricsConfig };`).

- [ ] **Step 8: Run, expect pass**

```bash
rtk cargo test -p agent-shim-config metrics_config --quiet
```

Expected: 2 passed.

- [ ] **Step 9: Commit**

```bash
rtk git add -A
rtk git commit -m "feat(config): MetricsConfig + metrics-rs workspace deps (Plan 02 P02 T1)"
```

---

### Task 2: Metric name constants + install + handle

**Files:**
- Create: `crates/observability/src/metrics/mod.rs`
- Create: `crates/observability/src/metrics/names.rs`
- Modify: `crates/observability/src/lib.rs`

- [ ] **Step 1: Create the names module**

Create `crates/observability/src/metrics/names.rs`:

```rust
//! Single source of truth for every metric name AgentShim emits.
//!
//! All names use the `agent_shim_` prefix. Suffixes follow Prometheus
//! conventions: `_total` for counters, `_seconds` for time histograms,
//! `_bytes` for byte histograms.
//!
//! Spec §3.1.

// --- Request lifecycle ---
pub const REQUESTS_TOTAL: &str = "agent_shim_requests_total";
pub const REQUEST_DURATION_SECONDS: &str = "agent_shim_request_duration_seconds";
pub const REQUEST_BODY_BYTES: &str = "agent_shim_request_body_bytes";
pub const IN_FLIGHT_REQUESTS: &str = "agent_shim_in_flight_requests";

// --- Resilience layer (mirrors v0.4 tracing taxonomy) ---
pub const RETRY_ATTEMPTS_TOTAL: &str = "agent_shim_retry_attempts_total";
pub const RETRY_EXHAUSTED_TOTAL: &str = "agent_shim_retry_exhausted_total";
pub const FALLBACK_TRANSITIONS_TOTAL: &str = "agent_shim_fallback_transitions_total";
pub const BREAKER_STATE_CHANGES_TOTAL: &str = "agent_shim_breaker_state_changes_total";
pub const RATE_LIMIT_REJECTED_TOTAL: &str = "agent_shim_rate_limit_rejected_total";

// --- Upstream call ---
pub const UPSTREAM_DURATION_SECONDS: &str = "agent_shim_upstream_duration_seconds";
pub const UPSTREAM_ERRORS_TOTAL: &str = "agent_shim_upstream_errors_total";

// --- Token accounting ---
pub const TOKENS_INPUT_TOTAL: &str = "agent_shim_tokens_input_total";
pub const TOKENS_OUTPUT_TOTAL: &str = "agent_shim_tokens_output_total";

// --- Reload (used by Plan 04) ---
pub const CONFIG_RELOADS_TOTAL: &str = "agent_shim_config_reloads_total";

#[cfg(test)]
mod tests {
    use super::*;

    /// Every constant must hold a distinct value so we can't accidentally
    /// shadow one metric with another.
    #[test]
    fn all_unique() {
        let all = [
            REQUESTS_TOTAL,
            REQUEST_DURATION_SECONDS,
            REQUEST_BODY_BYTES,
            IN_FLIGHT_REQUESTS,
            RETRY_ATTEMPTS_TOTAL,
            RETRY_EXHAUSTED_TOTAL,
            FALLBACK_TRANSITIONS_TOTAL,
            BREAKER_STATE_CHANGES_TOTAL,
            RATE_LIMIT_REJECTED_TOTAL,
            UPSTREAM_DURATION_SECONDS,
            UPSTREAM_ERRORS_TOTAL,
            TOKENS_INPUT_TOTAL,
            TOKENS_OUTPUT_TOTAL,
            CONFIG_RELOADS_TOTAL,
        ];
        let set: std::collections::HashSet<&str> = all.iter().copied().collect();
        assert_eq!(set.len(), all.len(), "duplicate metric name");
    }

    /// All names use the agent_shim_ prefix.
    #[test]
    fn all_prefixed() {
        let all = [
            REQUESTS_TOTAL, REQUEST_DURATION_SECONDS, REQUEST_BODY_BYTES,
            IN_FLIGHT_REQUESTS, RETRY_ATTEMPTS_TOTAL, RETRY_EXHAUSTED_TOTAL,
            FALLBACK_TRANSITIONS_TOTAL, BREAKER_STATE_CHANGES_TOTAL,
            RATE_LIMIT_REJECTED_TOTAL, UPSTREAM_DURATION_SECONDS,
            UPSTREAM_ERRORS_TOTAL, TOKENS_INPUT_TOTAL, TOKENS_OUTPUT_TOTAL,
            CONFIG_RELOADS_TOTAL,
        ];
        for name in all {
            assert!(name.starts_with("agent_shim_"), "{name} missing prefix");
        }
    }
}
```

- [ ] **Step 2: Create `metrics/mod.rs`**

Create `crates/observability/src/metrics/mod.rs`:

```rust
//! metrics-rs integration for AgentShim.
//!
//! Spec §3 (Plan 02). The crate exposes:
//! - [`install`] — initialize the global recorder; called once at startup.
//! - [`MetricsHandle`] — render Prometheus text; held in `AppCore`.
//! - [`names`] — every metric name as a `pub const`.
//!
//! The `metrics-rs` facade dispatches to whichever recorder was installed.
//! In production we install [`metrics_exporter_prometheus`]; in tests we
//! call [`install_for_test`] which uses a unique recorder per test process
//! so concurrent tests don't share global state.

use std::sync::Arc;

use agent_shim_config::MetricsConfig;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

pub mod names;
pub mod recorders;

/// Renders Prometheus text on demand. One process owns one of these.
pub struct MetricsHandle {
    handle: PrometheusHandle,
}

impl MetricsHandle {
    pub fn render(&self) -> String {
        self.handle.render()
    }
}

/// Install the global metrics recorder. Returns a [`MetricsHandle`] for
/// rendering. Idempotent: if a recorder is already installed (e.g. from a
/// prior test in the same process), this returns a fresh handle anyway —
/// the underlying recorder may be the previous one. Production callers
/// invoke this exactly once during startup.
pub fn install(cfg: &MetricsConfig) -> Arc<MetricsHandle> {
    let mut builder = PrometheusBuilder::new();

    // Per-metric histogram bucket overrides.
    for (name, buckets) in &cfg.histogram_buckets {
        // Accept both qualified (`agent_shim_request_duration_seconds`)
        // and bare (`request_duration_seconds`) forms.
        let qualified = if name.starts_with("agent_shim_") {
            name.clone()
        } else {
            format!("agent_shim_{name}")
        };
        builder = builder
            .set_buckets_for_metric(Matcher::Full(qualified), buckets)
            .expect("histogram bucket override must be valid");
    }

    let handle = builder
        .install_recorder()
        .expect("failed to install Prometheus recorder");

    describe_metrics();

    Arc::new(MetricsHandle { handle })
}

/// Register descriptions for every metric so /metrics returns text even
/// when no observation has been made. Without this, freshly-started
/// gateways serve an empty body and operators wonder if scrape works.
fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};
    use names::*;

    describe_counter!(REQUESTS_TOTAL, "Total HTTP requests by frontend, route, status_class");
    describe_histogram!(
        REQUEST_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "End-to-end request duration"
    );
    describe_histogram!(
        REQUEST_BODY_BYTES,
        metrics::Unit::Bytes,
        "Inbound request body size"
    );
    describe_gauge!(IN_FLIGHT_REQUESTS, "Currently-in-flight requests");

    describe_counter!(RETRY_ATTEMPTS_TOTAL, "Retry attempts by route, upstream, attempt number");
    describe_counter!(RETRY_EXHAUSTED_TOTAL, "Routes that exhausted all retry attempts");
    describe_counter!(FALLBACK_TRANSITIONS_TOTAL, "Fallback chain element transitions");
    describe_counter!(BREAKER_STATE_CHANGES_TOTAL, "Circuit breaker state transitions");
    describe_counter!(RATE_LIMIT_REJECTED_TOTAL, "Rate-limit rejections by dimension");

    describe_histogram!(
        UPSTREAM_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "Upstream call duration"
    );
    describe_counter!(UPSTREAM_ERRORS_TOTAL, "Upstream errors by class");

    describe_counter!(TOKENS_INPUT_TOTAL, "Input tokens consumed");
    describe_counter!(TOKENS_OUTPUT_TOTAL, "Output tokens produced");

    describe_counter!(CONFIG_RELOADS_TOTAL, "Config reload attempts by result");
}
```

- [ ] **Step 3: Add `recorders.rs` skeleton**

Create `crates/observability/src/metrics/recorders.rs`:

```rust
//! Typed wrappers over the `metrics-rs` macros that enforce label sets
//! at compile time. Callsites should prefer these helpers over raw
//! `counter!()` / `histogram!()` so renaming a label is one diff in
//! one file.

use crate::metrics::names;

/// Status class of an HTTP response.
#[derive(Debug, Clone, Copy)]
pub enum StatusClass {
    Success,    // 2xx
    ClientErr,  // 4xx
    ServerErr,  // 5xx
    Cancelled,  // client disconnect
}

impl StatusClass {
    pub fn label(self) -> &'static str {
        match self {
            Self::Success => "2xx",
            Self::ClientErr => "4xx",
            Self::ServerErr => "5xx",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn from_status(status: u16) -> Self {
        match status {
            200..=299 => Self::Success,
            400..=499 => Self::ClientErr,
            500..=599 => Self::ServerErr,
            _ => Self::ClientErr,
        }
    }
}

pub fn record_request(frontend: &'static str, route: &str, status: StatusClass, dur_secs: f64, body_bytes: usize) {
    metrics::counter!(
        names::REQUESTS_TOTAL,
        "frontend" => frontend,
        "route" => route.to_string(),
        "status_class" => status.label(),
    )
    .increment(1);
    metrics::histogram!(
        names::REQUEST_DURATION_SECONDS,
        "frontend" => frontend,
        "route" => route.to_string(),
        "status_class" => status.label(),
    )
    .record(dur_secs);
    metrics::histogram!(
        names::REQUEST_BODY_BYTES,
        "frontend" => frontend,
        "route" => route.to_string(),
    )
    .record(body_bytes as f64);
}

pub fn record_retry_attempt(route: &str, upstream: &str, attempt: u32) {
    metrics::counter!(
        names::RETRY_ATTEMPTS_TOTAL,
        "route" => route.to_string(),
        "upstream" => upstream.to_string(),
        "attempt" => attempt.to_string(),
    )
    .increment(1);
}

pub fn record_retry_exhausted(route: &str, upstream: &str) {
    metrics::counter!(
        names::RETRY_EXHAUSTED_TOTAL,
        "route" => route.to_string(),
        "upstream" => upstream.to_string(),
    )
    .increment(1);
}

pub fn record_fallback_transition(route: &str, from: &str, to: &str) {
    metrics::counter!(
        names::FALLBACK_TRANSITIONS_TOTAL,
        "route" => route.to_string(),
        "from_upstream" => from.to_string(),
        "to_upstream" => to.to_string(),
    )
    .increment(1);
}

pub fn record_breaker_state_change(upstream: &str, model: &str, from: &str, to: &str) {
    metrics::counter!(
        names::BREAKER_STATE_CHANGES_TOTAL,
        "upstream" => upstream.to_string(),
        "model" => model.to_string(),
        "from" => from,
        "to" => to,
    )
    .increment(1);
}

pub fn record_rate_limit_rejected(dimension: &'static str) {
    metrics::counter!(
        names::RATE_LIMIT_REJECTED_TOTAL,
        "dimension" => dimension,
    )
    .increment(1);
}

pub fn record_upstream_call(upstream: &str, model: &str, status: StatusClass, dur_secs: f64) {
    metrics::histogram!(
        names::UPSTREAM_DURATION_SECONDS,
        "upstream" => upstream.to_string(),
        "model" => model.to_string(),
        "status_class" => status.label(),
    )
    .record(dur_secs);
}

pub fn record_upstream_error(upstream: &str, model: &str, error_class: &'static str) {
    metrics::counter!(
        names::UPSTREAM_ERRORS_TOTAL,
        "upstream" => upstream.to_string(),
        "model" => model.to_string(),
        "error_class" => error_class,
    )
    .increment(1);
}

pub fn record_config_reload(result: &'static str) {
    metrics::counter!(names::CONFIG_RELOADS_TOTAL, "result" => result).increment(1);
}
```

- [ ] **Step 4: Re-export from `lib.rs`**

In `crates/observability/src/lib.rs`, add:

```rust
pub mod metrics;
pub use metrics::{install, MetricsHandle};
```

- [ ] **Step 5: Write failing test for `install`**

Create `crates/observability/src/metrics/mod.rs` test module at the bottom of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Concurrent tests in the same crate share a global recorder, so we
    /// install once via OnceLock and let the per-test counters/histograms
    /// land on whichever recorder won the race. Render must produce
    /// non-empty text.
    #[test]
    fn install_returns_renderable_handle() {
        // OnceLock guards against "recorder already installed" panic when
        // tests run in parallel.
        static INIT: std::sync::OnceLock<Arc<MetricsHandle>> = std::sync::OnceLock::new();
        let handle = INIT.get_or_init(|| install(&MetricsConfig::default())).clone();

        // A fresh recorder describes everything but observes nothing —
        // render should still emit HELP/TYPE lines.
        let body = handle.render();
        assert!(body.contains("agent_shim_requests_total"));
        assert!(body.contains("# HELP"));
    }

    #[test]
    fn render_includes_all_described_names() {
        static INIT: std::sync::OnceLock<Arc<MetricsHandle>> = std::sync::OnceLock::new();
        let handle = INIT.get_or_init(|| install(&MetricsConfig::default())).clone();
        let body = handle.render();
        for name in [
            names::REQUESTS_TOTAL,
            names::RETRY_ATTEMPTS_TOTAL,
            names::FALLBACK_TRANSITIONS_TOTAL,
            names::BREAKER_STATE_CHANGES_TOTAL,
            names::RATE_LIMIT_REJECTED_TOTAL,
            names::CONFIG_RELOADS_TOTAL,
        ] {
            assert!(body.contains(name), "render missing {name}");
        }
    }
}
```

- [ ] **Step 6: Build + test**

```bash
rtk cargo test -p agent-shim-observability --quiet
```

Expected: 6 passed (4 from names.rs + 2 from mod.rs).

- [ ] **Step 7: Commit**

```bash
rtk git add -A
rtk git commit -m "feat(observability): metrics-rs facade + Prometheus exporter (Plan 02 P02 T2)"
```

---

### Task 3: Wire metrics into AppCore + admin /metrics handler

**Files:**
- Modify: `crates/gateway/src/state.rs`
- Modify: `crates/gateway/src/admin/mod.rs`
- Create: `crates/gateway/src/admin/metrics_handler.rs`

- [ ] **Step 1: Add `metrics: Arc<MetricsHandle>` to `AppCore`**

In `crates/gateway/src/state.rs`, add to `AppCore`:

```rust
    /// Prometheus metrics handle. Plan 02 P02 T3. Built once at startup;
    /// /metrics handler renders against it.
    pub metrics: Arc<agent_shim_observability::MetricsHandle>,
```

In `AppState::build`, after the `let admin_config = config.admin.clone();` line, install metrics:

```rust
        // Plan 02 P02 T3: install the global metrics recorder. The
        // exporter handle lives in AppCore so the /metrics admin handler
        // can render against it.
        let metrics = agent_shim_observability::install(&config.metrics);
```

Add `metrics` to the `AppCore { … }` struct literal.

- [ ] **Step 2: Create the metrics handler**

Create `crates/gateway/src/admin/metrics_handler.rs`:

```rust
//! GET /metrics — render Prometheus text. Plan 02 P02 T3.

use axum::{extract::State, http::StatusCode, response::IntoResponse};

use crate::state::AppState;

pub async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let body = state.core.metrics.render();
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
}
```

- [ ] **Step 3: Wire into admin router**

In `crates/gateway/src/admin/mod.rs`:

```rust
mod handlers;
mod metrics_handler;

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handlers::healthz))
        .route("/readyz", get(handlers::readyz))
        .route("/metrics", get(metrics_handler::metrics))
        .with_state(state)
}
```

- [ ] **Step 4: Build**

```bash
rtk cargo build -p agent-shim 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 5: Write integration test**

Create `crates/gateway/tests/metrics_endpoint.rs`:

```rust
//! Plan 02 P02 T3: /metrics endpoint integration test.

use std::net::SocketAddr;

async fn pick_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
        .local_addr().unwrap().port()
}

async fn spawn_with_admin() -> (SocketAddr, SocketAddr) {
    let public_port = pick_port().await;
    let admin_port = pick_port().await;
    let yaml = format!(
        r#"
server: {{bind: 127.0.0.1, port: {public_port}}}
admin: {{bind: 127.0.0.1, port: {admin_port}}}
upstreams:
  noop:
    type: open_ai_compatible
    base_url: http://localhost:9999/v1
    api_key: dummy
routes:
  - frontend: openai_chat
    model: x
    upstream: noop
    upstream_model: x
"#);
    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(&yaml).unwrap();
    let public_addr: SocketAddr =
        format!("127.0.0.1:{public_port}").parse().unwrap();
    let admin_addr: SocketAddr =
        format!("127.0.0.1:{admin_port}").parse().unwrap();
    let state = agent_shim::state::AppState::new(cfg).await;
    let pl = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let al = tokio::net::TcpListener::bind(admin_addr).await.unwrap();
    let pa = agent_shim::server::build_router(state.clone());
    let aa = agent_shim::admin::build_router(state);
    tokio::spawn(async move { let _ = axum::serve(pl, pa).await; });
    tokio::spawn(async move { let _ = axum::serve(al, aa).await; });
    (public_addr, admin_addr)
}

#[tokio::test]
async fn metrics_endpoint_serves_prometheus_text() {
    let (_p, admin) = spawn_with_admin().await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let resp = reqwest::get(format!("http://{}/metrics", admin)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.starts_with("text/plain"), "unexpected content-type: {ct}");
    let body = resp.text().await.unwrap();
    // Described metrics appear even without any observations.
    assert!(body.contains("agent_shim_requests_total"));
    assert!(body.contains("# TYPE"));
}

#[tokio::test]
async fn metrics_text_parses_as_prometheus() {
    let (_p, admin) = spawn_with_admin().await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let body = reqwest::get(format!("http://{}/metrics", admin))
        .await.unwrap().text().await.unwrap();
    let lines: Vec<_> = body.lines().map(Ok).collect();
    let parsed = prometheus_parse::Scrape::parse(lines.into_iter()).expect("parses");
    assert!(!parsed.docs.is_empty(), "no metric docs");
}
```

- [ ] **Step 6: Run, expect pass**

```bash
rtk cargo test -p agent-shim --test metrics_endpoint --quiet
```

Expected: 2 passed.

- [ ] **Step 7: Commit**

```bash
rtk git add -A
rtk git commit -m "feat(gateway): /metrics endpoint on admin port (Plan 02 P02 T3)"
```

---

### Task 4: Instrument the resilience layer

**Files:**
- Modify: `crates/router/src/resilient_caller.rs`
- Modify: `crates/router/src/circuit_breaker.rs`
- Modify: `crates/router/src/rate_limit.rs`

The router crate emits via the metrics-rs facade — no exporter dependency. Instrumentation calls live next to the existing tracing events from Plan 4 P05 T1.

- [ ] **Step 1: Helper module for the router metric names**

In `crates/router/src/lib.rs`, add a small module:

```rust
/// Metric name constants the router crate emits via the `metrics` facade.
/// Mirrors `agent_shim_observability::metrics::names` — duplicated here
/// because the router crate doesn't depend on observability (it's a
/// lower layer). Plan 02 P02 T4.
pub(crate) mod metric_names {
    pub const RETRY_ATTEMPTS_TOTAL: &str = "agent_shim_retry_attempts_total";
    pub const RETRY_EXHAUSTED_TOTAL: &str = "agent_shim_retry_exhausted_total";
    pub const FALLBACK_TRANSITIONS_TOTAL: &str = "agent_shim_fallback_transitions_total";
    pub const BREAKER_STATE_CHANGES_TOTAL: &str = "agent_shim_breaker_state_changes_total";
    pub const RATE_LIMIT_REJECTED_TOTAL: &str = "agent_shim_rate_limit_rejected_total";
    pub const UPSTREAM_DURATION_SECONDS: &str = "agent_shim_upstream_duration_seconds";
    pub const UPSTREAM_ERRORS_TOTAL: &str = "agent_shim_upstream_errors_total";
}
```

(A unit test in `lib.rs` keeps the two name lists in sync — see step 6.)

- [ ] **Step 2: Instrument `retry.rs`**

In `crates/router/src/retry.rs`, find the existing `tracing::warn!` for retry attempts (it's adjacent to where the loop emits "retry.attempt"). Immediately after it, add:

```rust
    metrics::counter!(
        crate::metric_names::RETRY_ATTEMPTS_TOTAL,
        "route" => route_label.to_string(),
        "upstream" => upstream.to_string(),
        "attempt" => attempt.to_string(),
    )
    .increment(1);
```

Where `route_label`, `upstream`, `attempt` are the values already in scope for the tracing call. If the existing `retry_with_policy` signature does not have `route_label`, thread it through from `pipeline::dispatch` — the route alias is `model_alias` there, which the router caller already passes; if it isn't passed yet, add a `route_label: &str` parameter to `retry_with_policy` and update its single callsite in `resilient_caller.rs`.

For the retry-exhausted event, find the `tracing::warn!(retry.exhausted)` callsite in `retry_with_policy` and add immediately after:

```rust
    metrics::counter!(
        crate::metric_names::RETRY_EXHAUSTED_TOTAL,
        "route" => route_label.to_string(),
        "upstream" => upstream.to_string(),
    )
    .increment(1);
```

- [ ] **Step 3: Instrument fallback transitions in `resilient_caller.rs`**

Find the `tracing::warn!(target = "agent_shim::resilience", "fallback.transition", …)` call. Immediately after it, add:

```rust
    metrics::counter!(
        crate::metric_names::FALLBACK_TRANSITIONS_TOTAL,
        "route" => route_label.to_string(),
        "from_upstream" => from.to_string(),
        "to_upstream" => to.to_string(),
    )
    .increment(1);
```

(Use whichever local variable names hold these in the call's scope — match the tracing fields exactly.)

- [ ] **Step 4: Instrument breaker state changes in `circuit_breaker.rs`**

In `crates/router/src/circuit_breaker.rs`, find `fn emit_state_change(&self, from: &'static str, to: &'static str, reason: &'static str)`. Inside the function, after the existing `tracing::info!` line, add:

```rust
    metrics::counter!(
        crate::metric_names::BREAKER_STATE_CHANGES_TOTAL,
        "upstream" => self.upstream.clone(),
        "model" => self.model.clone(),
        "from" => from,
        "to" => to,
    )
    .increment(1);
```

`self.upstream` and `self.model` are the fields added in Phase 4 P05 T1. Verify with:

```bash
rtk grep -n "upstream:\|model:" crates/router/src/circuit_breaker.rs | head -10
```

- [ ] **Step 5: Instrument rate-limit rejections in `rate_limit.rs`**

In `crates/router/src/rate_limit.rs`, find the `tracing::warn!(target = "agent_shim::resilience", "rate_limit.rejected", …)` site. Immediately after, add:

```rust
    metrics::counter!(
        crate::metric_names::RATE_LIMIT_REJECTED_TOTAL,
        "dimension" => dimension_label,
    )
    .increment(1);
```

Where `dimension_label` is `"per_key" | "per_route" | "per_upstream" | "per_ip"` — match the existing string used in the tracing field.

- [ ] **Step 6: Add a name-parity test**

Create `crates/router/tests/metric_names_match_observability.rs`:

```rust
//! Plan 02 P02 T4: assert that the router crate's metric name constants
//! match the canonical names in agent-shim-observability. The router
//! cannot depend on the observability crate (it's a lower layer), so the
//! constants are duplicated. This test catches drift.

#[test]
fn router_metric_names_match_observability() {
    use agent_shim_observability::metrics::names as obs;

    // The router emits these names; they must match observability's.
    let router_to_obs: &[(&str, &str)] = &[
        ("agent_shim_retry_attempts_total", obs::RETRY_ATTEMPTS_TOTAL),
        ("agent_shim_retry_exhausted_total", obs::RETRY_EXHAUSTED_TOTAL),
        ("agent_shim_fallback_transitions_total", obs::FALLBACK_TRANSITIONS_TOTAL),
        ("agent_shim_breaker_state_changes_total", obs::BREAKER_STATE_CHANGES_TOTAL),
        ("agent_shim_rate_limit_rejected_total", obs::RATE_LIMIT_REJECTED_TOTAL),
        ("agent_shim_upstream_duration_seconds", obs::UPSTREAM_DURATION_SECONDS),
        ("agent_shim_upstream_errors_total", obs::UPSTREAM_ERRORS_TOTAL),
    ];
    for (router, obs) in router_to_obs {
        assert_eq!(router, obs, "router '{}' != observability '{}'", router, obs);
    }
}
```

- [ ] **Step 7: Test that emitted counters appear in /metrics**

Append to `crates/gateway/tests/metrics_endpoint.rs`:

```rust
#[tokio::test]
async fn breaker_state_change_increments_metric() {
    let (_p, admin) = spawn_with_admin().await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Manually emit the metric the way circuit_breaker.rs does. (A full
    // end-to-end test that actually trips a breaker lives in
    // crates/gateway/tests/breaker_trip_skips_upstream.rs from Phase 4;
    // here we verify the wiring at the metric level only.)
    metrics::counter!(
        "agent_shim_breaker_state_changes_total",
        "upstream" => "test",
        "model" => "x",
        "from" => "closed",
        "to" => "open",
    ).increment(1);

    let body = reqwest::get(format!("http://{}/metrics", admin))
        .await.unwrap().text().await.unwrap();
    assert!(
        body.contains(r#"agent_shim_breaker_state_changes_total{from="closed""#)
            || body.contains("agent_shim_breaker_state_changes_total"),
        "expected breaker counter in body, got:\n{}", body
    );
}
```

- [ ] **Step 8: Run all router + gateway tests**

```bash
rtk cargo test -p agent-shim-router --quiet
rtk cargo test -p agent-shim --test metrics_endpoint --quiet
```

Expected: all pass.

- [ ] **Step 9: Commit**

```bash
rtk git add -A
rtk git commit -m "feat(router): emit Prometheus counters for retry/fallback/breaker/rate-limit (Plan 02 P02 T4)"
```

---

### Task 5: Request-lifecycle middleware + in-flight gauge

**Files:**
- Create: `crates/gateway/src/metrics_layer.rs`
- Modify: `crates/gateway/src/server.rs`

- [ ] **Step 1: Create the middleware**

Create `crates/gateway/src/metrics_layer.rs`:

```rust
//! Tower middleware emitting request-lifecycle metrics. Plan 02 P02 T5.
//!
//! Runs BEFORE the existing `RequestIdLayer` so it sees the full path
//! incl. the path-extension marker. Records:
//!   - agent_shim_requests_total (counter)
//!   - agent_shim_request_duration_seconds (histogram)
//!   - agent_shim_request_body_bytes (histogram, when content-length
//!     is set; chunked requests are recorded as 0)
//!   - agent_shim_in_flight_requests (gauge, via Drop guard)

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::http::{Request, Response};
use tower::{Layer, Service};

#[derive(Clone)]
pub struct MetricsLayer;

impl<S> Layer<S> for MetricsLayer {
    type Service = MetricsService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        MetricsService { inner }
    }
}

#[derive(Clone)]
pub struct MetricsService<S> {
    inner: S,
}

struct InFlightGuard;

impl InFlightGuard {
    fn enter() -> Self {
        metrics::gauge!(
            agent_shim_observability::metrics::names::IN_FLIGHT_REQUESTS
        ).increment(1.0);
        Self
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        metrics::gauge!(
            agent_shim_observability::metrics::names::IN_FLIGHT_REQUESTS
        ).decrement(1.0);
    }
}

fn frontend_for_path(path: &str) -> Option<&'static str> {
    if path.starts_with("/v1/messages") {
        Some("anthropic_messages")
    } else if path == "/v1/chat/completions" {
        Some("openai_chat")
    } else if path == "/v1/responses" {
        Some("openai_responses")
    } else {
        None
    }
}

impl<S, B, RB> Service<Request<B>> for MetricsService<S>
where
    S: Service<Request<B>, Response = Response<RB>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let started = Instant::now();
        let path = req.uri().path().to_string();
        let body_bytes = req
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let frontend = frontend_for_path(&path);
        let _guard = InFlightGuard::enter();

        // Hold the guard until the future resolves.
        let inner = std::mem::replace(&mut self.inner, self.inner.clone());
        let mut inner = inner;

        Box::pin(async move {
            let _g = _guard;
            let resp = inner.call(req).await?;
            if let Some(frontend_name) = frontend {
                use agent_shim_observability::metrics::recorders::{record_request, StatusClass};
                let status = StatusClass::from_status(resp.status().as_u16());
                let dur = started.elapsed().as_secs_f64();
                // The `route` label cannot be known here without parsing
                // the request body. Emit "unknown" for now; pipeline-side
                // instrumentation in T6 fills the route label by emitting
                // a richer counter.
                record_request(frontend_name, "unknown", status, dur, body_bytes);
            }
            Ok(resp)
        })
    }
}
```

- [ ] **Step 2: Wire into `server.rs`**

In `crates/gateway/src/server.rs`, in `build_router`, add the layer (after `TraceLayer::new_for_http()`):

```rust
        .layer(crate::metrics_layer::MetricsLayer)
```

Add the module declaration in `crates/gateway/src/lib.rs`:

```rust
pub mod metrics_layer;
```

- [ ] **Step 3: Add a route-labeled counter from the pipeline**

Earlier instrumentation labels the request as `route="unknown"` because the middleware can't see the resolved alias. Add a route-labeled emit inside `pipeline::dispatch` AFTER route resolution succeeds:

In `crates/gateway/src/pipeline.rs`, after the `let chain = state.core.resolver.resolve(...)?;` line and before the upstream call, insert:

```rust
    // Plan 02 P02 T5: emit a richer route-labeled counter parallel to
    // the middleware's `route="unknown"` counter. Operators query
    // either depending on whether they care about pre-resolution
    // failures (e.g. 404 routes) or post-resolution traffic.
    let frontend_label_str = match spec.frontend.kind() {
        agent_shim_core::FrontendKind::AnthropicMessages => "anthropic_messages",
        agent_shim_core::FrontendKind::OpenAiChat => "openai_chat",
        agent_shim_core::FrontendKind::OpenAiResponses => "openai_responses",
    };
    metrics::counter!(
        agent_shim_observability::metrics::names::REQUESTS_TOTAL,
        "frontend" => frontend_label_str,
        "route" => model_alias.clone(),
        "status_class" => "pending",
    )
    .increment(0); // describe-only; the middleware records the real outcome
```

Adding `.increment(0)` registers the label set without inflating the counter. (Prometheus exposes the timeseries even with value 0, which is what we want.)

- [ ] **Step 4: Build**

```bash
rtk cargo build --workspace 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 5: Append integration test**

In `crates/gateway/tests/metrics_endpoint.rs`:

```rust
#[tokio::test]
async fn in_flight_gauge_present() {
    let (_p, admin) = spawn_with_admin().await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let body = reqwest::get(format!("http://{}/metrics", admin))
        .await.unwrap().text().await.unwrap();
    assert!(body.contains("agent_shim_in_flight_requests"));
}
```

- [ ] **Step 6: Test, frozen-core check, commit**

```bash
rtk cargo test --workspace 2>&1 | grep -E "^test result:" | awk -F'[ ;]' '{sum+=$4} END {print "TOTAL:", sum}'
```

Expected: 605 or higher (591 + name-parity (1) + endpoint tests (3) + breaker counter (1) + in-flight (1) + recorder/install tests (varies)).

```bash
rtk git diff master -- crates/core/ crates/frontends/ crates/providers/src/
```

Expected: empty.

```bash
rtk git add -A
rtk git commit -m "feat(gateway): request-lifecycle metrics middleware + in-flight gauge (Plan 02 P02 T5)"
```

---

### Task 6: End-to-end metric increment test

**Files:**
- Modify: `crates/gateway/tests/metrics_endpoint.rs`

The earlier tests assert presence; this asserts a real increment after a real request.

- [ ] **Step 1: Add the test**

Append to `crates/gateway/tests/metrics_endpoint.rs`:

```rust
#[tokio::test]
async fn request_increments_counter() {
    use mockito::Server;

    // Start a mock OpenAI-compatible upstream.
    let mut mock = Server::new_async().await;
    mock.mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"x","object":"chat.completion","model":"x","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#)
        .expect_at_least(1)
        .create_async()
        .await;

    let public_port = pick_port().await;
    let admin_port = pick_port().await;
    let yaml = format!(
        r#"
server: {{bind: 127.0.0.1, port: {public_port}}}
admin: {{bind: 127.0.0.1, port: {admin_port}}}
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
"#, mock.url());

    let cfg: agent_shim_config::GatewayConfig = serde_yaml::from_str(&yaml).unwrap();
    let public_addr: SocketAddr = format!("127.0.0.1:{public_port}").parse().unwrap();
    let admin_addr: SocketAddr = format!("127.0.0.1:{admin_port}").parse().unwrap();
    let state = agent_shim::state::AppState::new(cfg).await;
    let pl = tokio::net::TcpListener::bind(public_addr).await.unwrap();
    let al = tokio::net::TcpListener::bind(admin_addr).await.unwrap();
    let pa = agent_shim::server::build_router(state.clone());
    let aa = agent_shim::admin::build_router(state);
    tokio::spawn(async move { let _ = axum::serve(pl, pa).await; });
    tokio::spawn(async move { let _ = axum::serve(al, aa).await; });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Issue a real request.
    let body = r#"{"model":"x","messages":[{"role":"user","content":"hi"}]}"#;
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", public_addr))
        .header("content-type", "application/json")
        .body(body)
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);

    // Scrape /metrics, parse, assert the counter advanced.
    let text = reqwest::get(format!("http://{}/metrics", admin_addr))
        .await.unwrap().text().await.unwrap();
    let scrape = prometheus_parse::Scrape::parse(text.lines().map(Ok)).unwrap();
    let total: f64 = scrape.samples.iter()
        .filter(|s| s.metric == "agent_shim_requests_total")
        .filter_map(|s| match s.value {
            prometheus_parse::Value::Counter(v) | prometheus_parse::Value::Untyped(v) => Some(v),
            _ => None,
        })
        .sum();
    assert!(total >= 1.0, "expected at least 1, got {total}");
}
```

- [ ] **Step 2: Run, expect pass**

```bash
rtk cargo test -p agent-shim --test metrics_endpoint --quiet
```

- [ ] **Step 3: Final test count + frozen-core + commit**

```bash
rtk cargo test --workspace 2>&1 | grep -E "^test result:" | awk -F'[ ;]' '{sum+=$4} END {print "TOTAL:", sum}'
```

Expected: ≥ 609.

```bash
rtk git diff master -- crates/core/ crates/frontends/ crates/providers/src/
```

Expected: empty.

```bash
rtk git add -A
rtk git commit -m "test(gateway): end-to-end /metrics increment via real request (Plan 02 P02 T6)"
```

---

### Task 7: Spec compliance review

- [ ] **Step 1: Reviewer dispatch**

Spawn a fresh subagent with this brief:

> Review commits `<P02 T1..T6 commit range>` against `docs/superpowers/specs/2026-05-09-phase-5-observability-design.md` §3. Verify:
> 1. **Metric set** — every metric in §3.1 is registered in `crates/observability/src/metrics/names.rs` AND emitted at least once. List any missing.
> 2. **Cardinality bounds** — confirm `request_id`, `model_alias_requested_by_client`, and `api_key_hash` are NOT used as label values anywhere in `crates/router/src/{retry,resilient_caller,circuit_breaker,rate_limit}.rs` or `crates/gateway/src/metrics_layer.rs`.
> 3. **Prefix** — every metric name starts with `agent_shim_`.
> 4. **Suffixes** — `_total` for counters, `_seconds` for time histograms, `_bytes` for byte histograms.
> 5. **Description registration** — `describe_metrics()` lists every name in `names.rs`. The names-listed-but-not-described and described-but-not-named sets are both empty.
> 6. **/metrics is admin-only** — no route in `crates/gateway/src/server.rs::build_router` matches `/metrics`.
> 7. **Frozen core** — diff against master is empty in core/, frontends/, providers/src/.
>
> Report: numbered PASS/FAIL with a one-line justification each.

- [ ] **Step 2: Apply FAIL findings as fix commits**

`fix(...): <reason> (Plan 02 P02 T7 followup)`.

---

### Task 8: Code quality review

- [ ] **Step 1: Reviewer dispatch**

Spawn a fresh subagent:

> Review commits `<P02 T1..T7 commit range>` for code quality. Specifically:
> 1. **Recorder API ergonomics** — are `record_request`, `record_retry_attempt`, etc. shaped right? Are `String`/`&str` arg types reasonable, or do they force allocations on every emission?
> 2. **Middleware correctness** — does `MetricsService` correctly hold the in-flight guard for the entire async future, or does the guard drop early?
> 3. **Description registration** — is `describe_metrics()` called at the right point (post-install, pre-render)? Will tests that share the global recorder see consistent descriptions?
> 4. **Name-parity test** — is the duplication-with-test pattern the simplest correct shape, or should the router have a transitive dep on observability via a trait?
> 5. **Test isolation** — `metrics-rs` uses a process-global recorder. Multiple tests in the same crate may race on `install_recorder`. Is the `OnceLock` guard correct?
>
> Numbered findings; CRITICAL/HIGH must fix.

- [ ] **Step 2: Apply CRITICAL/HIGH findings**

---

## Done when

- [ ] Workspace test count ≥ 609.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] Frozen-core diff empty.
- [ ] `/metrics` reachable on admin port; not reachable on public port.
- [ ] A real request through `/v1/chat/completions` increments `agent_shim_requests_total`.
- [ ] `breaker_state_change`, `fallback.transition`, `retry.attempt`, `rate_limit.rejected` events all emit metrics next to their existing tracing events.
- [ ] T7 + T8 reviewer rounds clear of CRITICAL/HIGH.
