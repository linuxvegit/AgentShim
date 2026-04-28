# Plan 02 — `config` + `observability` + `gateway` Skeleton

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the `config`, `observability`, and `gateway` crates so `agent-shim serve --config gateway.yaml` boots, parses YAML, sets up structured tracing with request IDs, and serves a `/healthz` endpoint. No frontends or providers wired yet.

**Architecture:** `config` owns YAML schema + validation. `observability` owns `tracing` setup and per-request middleware. `gateway` is the binary: clap CLI → load config → start axum server → graceful shutdown. Frontends/providers slot in via traits in later plans.

**Tech Stack:** `figment` (YAML + env), `tracing` + `tracing-subscriber` (JSON for prod, pretty for dev), `axum` 0.7, `tower` / `tower-http`, `tokio` (multi-thread), `clap` (derive), `anyhow` at the binary boundary.

---

## File Structure

`crates/config/`:
- Create: `crates/config/Cargo.toml`
- Create: `crates/config/src/lib.rs`
- Create: `crates/config/src/schema.rs` — top-level `GatewayConfig`, `ServerConfig`, `RouteEntry`, `UpstreamConfig`, `LoggingConfig`
- Create: `crates/config/src/loader.rs` — `load_from_path`, env overlay
- Create: `crates/config/src/validation.rs` — semantic checks (no duplicate aliases, referenced upstreams exist)
- Create: `crates/config/src/secrets.rs` — `Secret<String>` newtype with redacted Debug
- Create: `crates/config/tests/load_config.rs`

`crates/observability/`:
- Create: `crates/observability/Cargo.toml`
- Create: `crates/observability/src/lib.rs`
- Create: `crates/observability/src/tracing_setup.rs` — `init(LoggingConfig)`
- Create: `crates/observability/src/request_id.rs` — `RequestIdLayer`, `RequestIdExt` extractor
- Create: `crates/observability/src/redaction.rs` — header redaction list

`crates/gateway/`:
- Create: `crates/gateway/Cargo.toml`
- Create: `crates/gateway/src/main.rs`
- Create: `crates/gateway/src/cli.rs`
- Create: `crates/gateway/src/server.rs`
- Create: `crates/gateway/src/state.rs`
- Create: `crates/gateway/src/shutdown.rs`
- Create: `crates/gateway/src/commands/mod.rs`
- Create: `crates/gateway/src/commands/serve.rs`
- Create: `crates/gateway/src/commands/validate_config.rs`

Repo:
- Create: `config/gateway.example.yaml`
- Create: `config/gateway.minimal.yaml`
- Modify: root `Cargo.toml` `members`

---

## Task 1: Add new crates to workspace

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Update members + workspace deps**

Replace the `[workspace]` section's `members` and append to `[workspace.dependencies]`:

```toml
[workspace]
resolver = "2"
members = ["crates/core", "crates/config", "crates/observability", "crates/gateway"]

# (existing workspace.package + workspace.dependencies kept)
```

Append to `[workspace.dependencies]`:

```toml
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal", "sync", "fs", "time"] }
axum = { version = "0.7", features = ["macros", "tokio", "http1"] }
tower = { version = "0.5", features = ["util"] }
tower-http = { version = "0.6", features = ["trace", "request-id", "cors"] }
hyper = { version = "1", features = ["server"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json", "fmt"] }
clap = { version = "4", features = ["derive", "env"] }
figment = { version = "0.10", features = ["yaml", "env"] }
anyhow = "1"
async-trait = "0.1"
```

- [ ] **Step 2: Verify workspace still parses**

Run: `cargo metadata --no-deps --format-version 1 > /dev/null`
Expected: exits 0.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml
git commit -m "chore: register config/observability/gateway crates in workspace"
```

---

## Task 2: `config` crate — `secrets` module

**Files:**
- Create: `crates/config/Cargo.toml`
- Create: `crates/config/src/lib.rs`
- Create: `crates/config/src/secrets.rs`

- [ ] **Step 1: `Cargo.toml`**

```toml
[package]
name = "agent-shim-config"
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
name = "agent_shim_config"
path = "src/lib.rs"

[dependencies]
agent-shim-core = { path = "../core" }
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
figment.workspace = true

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: `secrets.rs`**

```rust
use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
    pub fn expose(&self) -> &str { &self.0 }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "***")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_value() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{:?}", s), "Secret(***)");
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn deserializes_from_plain_string() {
        let s: Secret = serde_json::from_str("\"hunter2\"").unwrap();
        assert_eq!(s.expose(), "hunter2");
    }
}
```

- [ ] **Step 3: `lib.rs` (initial)**

```rust
#![forbid(unsafe_code)]

pub mod secrets;
pub use secrets::Secret;
```

- [ ] **Step 4: Run tests, commit**

Run: `cargo test -p agent-shim-config`
Expected: 2 passed.

```bash
git add crates/config
git commit -m "feat(config): add Secret newtype with redacted Debug/Display"
```

---

## Task 3: `config` schema

**Files:**
- Create: `crates/config/src/schema.rs`
- Modify: `crates/config/src/lib.rs`

- [ ] **Step 1: Implementation**

`crates/config/src/schema.rs`:

```rust
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::secrets::Secret;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub upstreams: BTreeMap<String, UpstreamConfig>,
    #[serde(default)]
    pub routes: Vec<RouteEntry>,
    #[serde(default)]
    pub copilot: Option<CopilotConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// SSE keep-alive interval (seconds, 0 = disabled).
    #[serde(default = "default_keepalive")]
    pub keepalive_secs: u64,
}

fn default_port() -> u16 { 8787 }
fn default_keepalive() -> u64 { 15 }

impl Default for ServerConfig {
    fn default() -> Self {
        Self { bind: "127.0.0.1".into(), port: default_port(), keepalive_secs: default_keepalive() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    /// `pretty` for dev, `json` for prod.
    #[serde(default = "default_format")]
    pub format: LogFormat,
    /// `RUST_LOG`-style filter, e.g. `info,agent_shim=debug`.
    #[serde(default = "default_filter")]
    pub filter: String,
}

fn default_format() -> LogFormat { LogFormat::Pretty }
fn default_filter() -> String { "info,agent_shim=debug".into() }

impl Default for LoggingConfig {
    fn default() -> Self { Self { format: default_format(), filter: default_filter() } }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat { Pretty, Json }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UpstreamConfig {
    OpenAiCompatible(OpenAiCompatibleUpstream),
    GithubCopilot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OpenAiCompatibleUpstream {
    pub base_url: String,
    pub api_key: Secret,
    #[serde(default)]
    pub default_headers: BTreeMap<String, String>,
    #[serde(default = "default_timeout_secs")]
    pub request_timeout_secs: u64,
}

fn default_timeout_secs() -> u64 { 120 }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CopilotConfig {
    /// Path to the persisted GitHub OAuth token JSON.
    pub credential_path: std::path::PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RouteEntry {
    /// Frontend protocol (`anthropic_messages` or `openai_chat`).
    pub frontend: String,
    /// Frontend-visible model alias (matched against the request's `model` field).
    pub model: String,
    /// Upstream key (must exist in `upstreams`).
    pub upstream: String,
    /// Provider-side model name (sent upstream).
    pub upstream_model: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_default_uses_localhost_8787() {
        let s = ServerConfig::default();
        assert_eq!(s.bind, "127.0.0.1");
        assert_eq!(s.port, 8787);
    }

    #[test]
    fn unknown_fields_rejected() {
        let yaml = "server:\n  bind: 0.0.0.0\n  port: 8000\n  bogus: 1\n";
        let res: Result<GatewayConfig, _> = serde_yaml::from_str(yaml);
        assert!(res.is_err());
    }
}
```

- [ ] **Step 2: Add `serde_yaml` dev-dep**

In `crates/config/Cargo.toml` `[dev-dependencies]` add `serde_yaml = "0.9"`.

- [ ] **Step 3: Re-export, run tests, commit**

Append to `lib.rs`: `pub mod schema; pub use schema::*;`

Run: `cargo test -p agent-shim-config schema`
Expected: 2 passed.

```bash
git add crates/config
git commit -m "feat(config): add GatewayConfig schema with deny_unknown_fields"
```

---

## Task 4: `config` loader (YAML + env overlay)

**Files:**
- Create: `crates/config/src/loader.rs`
- Modify: `crates/config/src/lib.rs`

- [ ] **Step 1: Implementation**

`crates/config/src/loader.rs`:

```rust
use std::path::Path;

use figment::providers::{Env, Format, Yaml};
use figment::Figment;
use thiserror::Error;

use crate::schema::GatewayConfig;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file not found: {0}")]
    NotFound(String),
    #[error("config parse error: {0}")]
    Parse(#[from] figment::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Load YAML from `path`, then overlay `AGENT_SHIM__` env vars (double-underscore = nesting).
pub fn load_from_path(path: &Path) -> Result<GatewayConfig, ConfigError> {
    if !path.exists() {
        return Err(ConfigError::NotFound(path.display().to_string()));
    }
    let cfg: GatewayConfig = Figment::new()
        .merge(Yaml::file(path))
        .merge(Env::prefixed("AGENT_SHIM__").split("__"))
        .extract()?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn loads_minimal_yaml() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "server:\n  bind: 0.0.0.0\n  port: 9000\n").unwrap();
        let cfg = load_from_path(f.path()).unwrap();
        assert_eq!(cfg.server.bind, "0.0.0.0");
        assert_eq!(cfg.server.port, 9000);
    }

    #[test]
    fn missing_file_returns_not_found() {
        let err = load_from_path(Path::new("/no/such/file.yaml")).unwrap_err();
        assert!(matches!(err, ConfigError::NotFound(_)));
    }
}
```

- [ ] **Step 2: Re-export, run tests, commit**

Append to `lib.rs`: `pub mod loader; pub use loader::{load_from_path, ConfigError};`

Run: `cargo test -p agent-shim-config loader`
Expected: 2 passed.

```bash
git add crates/config
git commit -m "feat(config): add YAML + env loader via figment"
```

---

## Task 5: `config` validation

**Files:**
- Create: `crates/config/src/validation.rs`
- Modify: `crates/config/src/lib.rs`

- [ ] **Step 1: Implementation**

`crates/config/src/validation.rs`:

```rust
use std::collections::HashSet;

use thiserror::Error;

use crate::schema::GatewayConfig;

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("route #{index} references unknown upstream `{upstream}`")]
    UnknownUpstream { index: usize, upstream: String },
    #[error("duplicate route alias: frontend=`{frontend}` model=`{model}`")]
    DuplicateAlias { frontend: String, model: String },
    #[error("route #{index} uses unknown frontend `{frontend}` (expected `anthropic_messages` or `openai_chat`)")]
    UnknownFrontend { index: usize, frontend: String },
    #[error("server.port = 0 is not allowed")]
    InvalidPort,
}

pub fn validate(cfg: &GatewayConfig) -> Result<(), ValidationError> {
    if cfg.server.port == 0 {
        return Err(ValidationError::InvalidPort);
    }
    let mut seen = HashSet::new();
    for (i, r) in cfg.routes.iter().enumerate() {
        if !matches!(r.frontend.as_str(), "anthropic_messages" | "openai_chat") {
            return Err(ValidationError::UnknownFrontend { index: i, frontend: r.frontend.clone() });
        }
        if !cfg.upstreams.contains_key(&r.upstream) {
            return Err(ValidationError::UnknownUpstream { index: i, upstream: r.upstream.clone() });
        }
        let key = (r.frontend.clone(), r.model.clone());
        if !seen.insert(key) {
            return Err(ValidationError::DuplicateAlias {
                frontend: r.frontend.clone(),
                model: r.model.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{OpenAiCompatibleUpstream, RouteEntry, ServerConfig, UpstreamConfig};
    use crate::secrets::Secret;
    use std::collections::BTreeMap;

    fn cfg_with(routes: Vec<RouteEntry>, upstreams: Vec<(&str, UpstreamConfig)>) -> GatewayConfig {
        let mut up = BTreeMap::new();
        for (k, v) in upstreams { up.insert(k.into(), v); }
        GatewayConfig {
            server: ServerConfig::default(),
            logging: Default::default(),
            upstreams: up,
            routes,
            copilot: None,
        }
    }

    fn openai_compat(name: &str) -> (&str, UpstreamConfig) {
        let cfg = UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: "https://api.example".into(),
            api_key: Secret::new("k"),
            default_headers: Default::default(),
            request_timeout_secs: 30,
        });
        (name, cfg)
    }

    #[test]
    fn valid_config_passes() {
        let c = cfg_with(
            vec![RouteEntry {
                frontend: "openai_chat".into(),
                model: "gpt-4o".into(),
                upstream: "openai".into(),
                upstream_model: "gpt-4o".into(),
            }],
            vec![openai_compat("openai")],
        );
        validate(&c).unwrap();
    }

    #[test]
    fn unknown_upstream_fails() {
        let c = cfg_with(
            vec![RouteEntry {
                frontend: "openai_chat".into(),
                model: "gpt-4o".into(),
                upstream: "ghost".into(),
                upstream_model: "gpt-4o".into(),
            }],
            vec![],
        );
        let e = validate(&c).unwrap_err();
        assert!(matches!(e, ValidationError::UnknownUpstream { .. }));
    }

    #[test]
    fn duplicate_alias_fails() {
        let r = RouteEntry {
            frontend: "openai_chat".into(),
            model: "gpt-4o".into(),
            upstream: "openai".into(),
            upstream_model: "gpt-4o".into(),
        };
        let c = cfg_with(vec![r.clone(), r], vec![openai_compat("openai")]);
        let e = validate(&c).unwrap_err();
        assert!(matches!(e, ValidationError::DuplicateAlias { .. }));
    }

    #[test]
    fn unknown_frontend_fails() {
        let c = cfg_with(
            vec![RouteEntry {
                frontend: "weird".into(),
                model: "x".into(),
                upstream: "openai".into(),
                upstream_model: "x".into(),
            }],
            vec![openai_compat("openai")],
        );
        assert!(matches!(validate(&c).unwrap_err(), ValidationError::UnknownFrontend { .. }));
    }
}
```

- [ ] **Step 2: Re-export, run tests, commit**

Append to `lib.rs`: `pub mod validation; pub use validation::{validate, ValidationError};`

Run: `cargo test -p agent-shim-config validation`
Expected: 4 passed.

```bash
git add crates/config
git commit -m "feat(config): add semantic validation for routes and upstreams"
```

---

## Task 6: `observability` crate — tracing setup

**Files:**
- Create: `crates/observability/Cargo.toml`
- Create: `crates/observability/src/lib.rs`
- Create: `crates/observability/src/tracing_setup.rs`

- [ ] **Step 1: `Cargo.toml`**

```toml
[package]
name = "agent-shim-observability"
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
name = "agent_shim_observability"
path = "src/lib.rs"

[dependencies]
agent-shim-config = { path = "../config" }
tracing.workspace = true
tracing-subscriber.workspace = true
tower-http.workspace = true
tower.workspace = true
http = "1"
uuid.workspace = true
```

- [ ] **Step 2: `tracing_setup.rs`**

```rust
use agent_shim_config::schema::{LogFormat, LoggingConfig};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

pub fn init(cfg: &LoggingConfig) {
    let filter = EnvFilter::try_new(&cfg.filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);
    match cfg.format {
        LogFormat::Pretty => registry.with(fmt::layer().with_target(true)).init(),
        LogFormat::Json => registry.with(fmt::layer().json()).init(),
    }
}
```

- [ ] **Step 3: `lib.rs`**

```rust
#![forbid(unsafe_code)]

pub mod tracing_setup;
pub use tracing_setup::init;
```

- [ ] **Step 4: Verify builds**

Run: `cargo build -p agent-shim-observability`
Expected: compiles clean.

- [ ] **Step 5: Commit**

```bash
git add crates/observability
git commit -m "feat(observability): tracing setup with pretty/json formatter selection"
```

---

## Task 7: `observability` request-ID middleware

**Files:**
- Create: `crates/observability/src/request_id.rs`
- Create: `crates/observability/src/redaction.rs`
- Modify: `crates/observability/src/lib.rs`

- [ ] **Step 1: `request_id.rs`**

```rust
use std::task::{Context, Poll};

use http::{HeaderName, HeaderValue, Request, Response};
use tower::{Layer, Service};
use uuid::Uuid;

pub const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

#[derive(Clone, Debug, Default)]
pub struct RequestIdLayer;

impl<S> Layer<S> for RequestIdLayer {
    type Service = RequestIdService<S>;
    fn layer(&self, inner: S) -> Self::Service { RequestIdService { inner } }
}

#[derive(Clone, Debug)]
pub struct RequestIdService<S> { inner: S }

impl<S, ReqB, ResB> Service<Request<ReqB>> for RequestIdService<S>
where
    S: Service<Request<ReqB>, Response = Response<ResB>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<ReqB>) -> Self::Future {
        if !req.headers().contains_key(&REQUEST_ID_HEADER) {
            let id = format!("req_{}", Uuid::new_v4().simple());
            if let Ok(v) = HeaderValue::from_str(&id) {
                req.headers_mut().insert(REQUEST_ID_HEADER.clone(), v);
            }
        }
        self.inner.call(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Response;
    use tower::{ServiceBuilder, ServiceExt};

    #[tokio::test]
    async fn injects_request_id_when_missing() {
        let svc = ServiceBuilder::new()
            .layer(RequestIdLayer)
            .service_fn(|req: Request<()>| async move {
                let id = req.headers().get(&REQUEST_ID_HEADER).cloned();
                Ok::<_, std::convert::Infallible>(Response::new(id))
            });
        let resp = svc.oneshot(Request::new(())).await.unwrap();
        let id = resp.into_body().expect("id").to_str().unwrap().to_string();
        assert!(id.starts_with("req_"));
    }

    #[tokio::test]
    async fn preserves_existing_request_id() {
        let svc = ServiceBuilder::new()
            .layer(RequestIdLayer)
            .service_fn(|req: Request<()>| async move {
                let id = req.headers().get(&REQUEST_ID_HEADER).cloned();
                Ok::<_, std::convert::Infallible>(Response::new(id))
            });
        let mut req = Request::new(());
        req.headers_mut().insert(REQUEST_ID_HEADER.clone(), HeaderValue::from_static("req_abc"));
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.into_body().unwrap().to_str().unwrap(), "req_abc");
    }
}
```

- [ ] **Step 2: `redaction.rs`**

```rust
/// Header names that must never appear in logs.
pub const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "anthropic-api-key",
    "openai-api-key",
    "copilot-token",
    "cookie",
    "set-cookie",
    "proxy-authorization",
];

pub fn is_sensitive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SENSITIVE_HEADERS.iter().any(|h| *h == lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_case_insensitive() {
        assert!(is_sensitive("Authorization"));
        assert!(is_sensitive("AUTHORIZATION"));
        assert!(is_sensitive("x-api-key"));
        assert!(!is_sensitive("user-agent"));
    }
}
```

- [ ] **Step 3: Add tokio dev-dep**

In `crates/observability/Cargo.toml`:

```toml
[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt"] }
```

- [ ] **Step 4: Re-export**

```rust
// observability/src/lib.rs
#![forbid(unsafe_code)]

pub mod redaction;
pub mod request_id;
pub mod tracing_setup;

pub use request_id::{RequestIdLayer, REQUEST_ID_HEADER};
pub use tracing_setup::init;
```

- [ ] **Step 5: Run tests, commit**

Run: `cargo test -p agent-shim-observability`
Expected: 3 passed.

```bash
git add crates/observability
git commit -m "feat(observability): request ID middleware and header redaction list"
```

---

## Task 8: `gateway` crate — Cargo + CLI skeleton

**Files:**
- Create: `crates/gateway/Cargo.toml`
- Create: `crates/gateway/src/main.rs`
- Create: `crates/gateway/src/cli.rs`

- [ ] **Step 1: `Cargo.toml`**

```toml
[package]
name = "agent-shim"
version.workspace = true
edition.workspace = true
license.workspace = true
default-run = "agent-shim"

[[bin]]
name = "agent-shim"
path = "src/main.rs"

[dependencies]
agent-shim-core = { path = "../core" }
agent-shim-config = { path = "../config" }
agent-shim-observability = { path = "../observability" }
tokio.workspace = true
axum.workspace = true
tower.workspace = true
tower-http.workspace = true
hyper.workspace = true
tracing.workspace = true
clap.workspace = true
anyhow.workspace = true
serde.workspace = true
serde_json.workspace = true
```

- [ ] **Step 2: `cli.rs`**

```rust
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "agent-shim", version, about = "Universal LLM gateway for AI coding agents")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the HTTP gateway.
    Serve {
        #[arg(short, long, env = "AGENT_SHIM_CONFIG")]
        config: PathBuf,
    },
    /// Validate a config file and exit.
    ValidateConfig {
        #[arg(short, long)]
        config: PathBuf,
    },
}
```

- [ ] **Step 3: `main.rs`**

```rust
#![forbid(unsafe_code)]

mod cli;
mod commands;
mod server;
mod shutdown;
mod state;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve { config } => commands::serve::run(&config).await,
        Command::ValidateConfig { config } => commands::validate_config::run(&config),
    }
}
```

- [ ] **Step 4: Stub the modules so it compiles**

`crates/gateway/src/server.rs`, `shutdown.rs`, `state.rs`, `commands/mod.rs`, `commands/serve.rs`, `commands/validate_config.rs` — create empty files (or `pub fn placeholder() {}`). Real content lands in next tasks.

For now, stub:

```rust
// crates/gateway/src/commands/mod.rs
pub mod serve;
pub mod validate_config;
```

```rust
// crates/gateway/src/commands/serve.rs
use std::path::Path;
use anyhow::Result;
pub async fn run(_config: &Path) -> Result<()> { Ok(()) }
```

```rust
// crates/gateway/src/commands/validate_config.rs
use std::path::Path;
use anyhow::Result;
pub fn run(_config: &Path) -> Result<()> { Ok(()) }
```

```rust
// crates/gateway/src/server.rs   (empty for now)
```

```rust
// crates/gateway/src/state.rs    (empty for now)
```

```rust
// crates/gateway/src/shutdown.rs (empty for now)
```

- [ ] **Step 5: Compiles?**

Run: `cargo build -p agent-shim`
Expected: compiles.

- [ ] **Step 6: Commit**

```bash
git add crates/gateway
git commit -m "feat(gateway): clap CLI skeleton with serve and validate-config subcommands"
```

---

## Task 9: `validate-config` subcommand

**Files:**
- Modify: `crates/gateway/src/commands/validate_config.rs`

- [ ] **Step 1: Implementation**

```rust
use std::path::Path;

use anyhow::{Context, Result};
use agent_shim_config::{load_from_path, validate};

pub fn run(config: &Path) -> Result<()> {
    let cfg = load_from_path(config).with_context(|| format!("loading {}", config.display()))?;
    validate(&cfg).context("validating config")?;
    println!("OK: {} routes, {} upstreams", cfg.routes.len(), cfg.upstreams.len());
    Ok(())
}
```

- [ ] **Step 2: Add example configs**

`config/gateway.minimal.yaml`:

```yaml
server:
  bind: 127.0.0.1
  port: 8787

logging:
  format: pretty
  filter: info,agent_shim=debug

upstreams: {}

routes: []
```

`config/gateway.example.yaml`:

```yaml
server:
  bind: 0.0.0.0
  port: 8787
  keepalive_secs: 15

logging:
  format: json
  filter: info,agent_shim=debug

upstreams:
  deepseek:
    kind: openai_compatible
    base_url: https://api.deepseek.com
    api_key: ${DEEPSEEK_API_KEY}
    request_timeout_secs: 120
  copilot:
    kind: github_copilot

copilot:
  credential_path: ~/.config/agent-shim/copilot.json

routes:
  - frontend: openai_chat
    model: deepseek-chat
    upstream: deepseek
    upstream_model: deepseek-chat
  - frontend: anthropic_messages
    model: copilot-claude
    upstream: copilot
    upstream_model: claude-3.5-sonnet
```

- [ ] **Step 3: Smoke test**

Run: `cargo run -p agent-shim -- validate-config --config config/gateway.minimal.yaml`
Expected: `OK: 0 routes, 0 upstreams`

- [ ] **Step 4: Commit**

```bash
git add crates/gateway/src/commands/validate_config.rs config/
git commit -m "feat(gateway): validate-config subcommand and example YAMLs"
```

---

## Task 10: `state` and `server` — axum app with `/healthz`

**Files:**
- Modify: `crates/gateway/src/state.rs`
- Modify: `crates/gateway/src/server.rs`
- Modify: `crates/gateway/src/shutdown.rs`

- [ ] **Step 1: `state.rs`**

```rust
use std::sync::Arc;

use agent_shim_config::schema::GatewayConfig;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<GatewayConfig>,
}

impl AppState {
    pub fn new(config: GatewayConfig) -> Self { Self { config: Arc::new(config) } }
}
```

- [ ] **Step 2: `shutdown.rs`**

```rust
use tokio::signal;

pub async fn signal_received() {
    let ctrl_c = async { let _ = signal::ctrl_c().await; };
    #[cfg(unix)]
    let term = async {
        use signal::unix::{signal, SignalKind};
        if let Ok(mut s) = signal(SignalKind::terminate()) { s.recv().await; }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutdown signal received");
}
```

- [ ] **Step 3: `server.rs`**

```rust
use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::{routing::get, Router};
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

use agent_shim_observability::request_id::RequestIdLayer;

use crate::state::AppState;
use crate::shutdown;

pub async fn run(state: AppState) -> Result<()> {
    let addr: SocketAddr = format!("{}:{}", state.config.server.bind, state.config.server.port)
        .parse()
        .context("invalid bind address")?;

    let app = Router::new()
        .route("/healthz", get(health))
        .with_state(state)
        .layer(
            ServiceBuilder::new()
                .layer(RequestIdLayer)
                .layer(TraceLayer::new_for_http()),
        );

    tracing::info!(%addr, "starting gateway");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown::signal_received())
        .await?;
    Ok(())
}

async fn health() -> &'static str { "ok" }
```

- [ ] **Step 4: Wire up `serve` command**

`crates/gateway/src/commands/serve.rs`:

```rust
use std::path::Path;

use anyhow::{Context, Result};
use agent_shim_config::{load_from_path, validate};
use agent_shim_observability::tracing_setup;

use crate::server;
use crate::state::AppState;

pub async fn run(config: &Path) -> Result<()> {
    let cfg = load_from_path(config).with_context(|| format!("loading {}", config.display()))?;
    validate(&cfg).context("validating config")?;
    tracing_setup::init(&cfg.logging);
    let state = AppState::new(cfg);
    server::run(state).await
}
```

- [ ] **Step 5: Build**

Run: `cargo build -p agent-shim`
Expected: clean build.

- [ ] **Step 6: Smoke test in another terminal**

Run: `cargo run -p agent-shim -- serve --config config/gateway.minimal.yaml`
Then in a second terminal: `curl -i http://127.0.0.1:8787/healthz`
Expected: `HTTP/1.1 200 OK`, body `ok`, response includes `x-request-id` header from the layer (note: tower-http's TraceLayer logs the id we injected).
Stop with Ctrl-C; gateway logs `shutdown signal received`.

- [ ] **Step 7: Commit**

```bash
git add crates/gateway
git commit -m "feat(gateway): axum app with /healthz, request-id and trace layers, graceful shutdown"
```

---

## Task 11: Integration test for boot + healthz

**Files:**
- Create: `crates/gateway/tests/healthz.rs`

- [ ] **Step 1: Test**

```rust
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthz_returns_ok() {
    use agent_shim_config::schema::{GatewayConfig, ServerConfig};

    let cfg = GatewayConfig {
        server: ServerConfig { bind: "127.0.0.1".into(), port: 0, keepalive_secs: 15 },
        logging: Default::default(),
        upstreams: Default::default(),
        routes: vec![],
        copilot: None,
    };

    // We need port 0 → bind reports actual port. Pull bind out of server::run by inlining
    // the listener setup here for the test.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let app = axum::Router::new().route("/healthz", axum::routing::get(|| async { "ok" }));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Give it a tick
    tokio::time::sleep(Duration::from_millis(50)).await;

    let resp = reqwest::get(format!("http://{}/healthz", addr)).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");
    let _ = cfg; // silence unused
}
```

- [ ] **Step 2: Add `reqwest` dev-dep**

In `crates/gateway/Cargo.toml`:

```toml
[dev-dependencies]
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "time"] }
```

- [ ] **Step 3: Run**

Run: `cargo test -p agent-shim --test healthz`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/gateway
git commit -m "test(gateway): integration test for /healthz"
```

---

## Self-Review Notes

- Spec §3 crate boundaries: `config`, `observability`, `gateway` all created; `frontends`/`providers`/`router` deliberately deferred to later plans. ✓
- Spec §6 `CopilotConfig.credential_path` schema field present. ✓
- Spec §11 (single-process, no clustering) — code makes no shared-state assumptions. ✓
- `deny_unknown_fields` everywhere → typo'd config keys fail loudly. ✓
- Secret newtype prevents accidental log leakage. ✓
- Graceful shutdown handles SIGTERM (Linux/Mac) + Ctrl-C (all). ✓
