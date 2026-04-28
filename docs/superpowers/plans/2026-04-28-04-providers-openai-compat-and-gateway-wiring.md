# Plan 04 — `providers` (OpenAI-Compatible) + Router Stub + Gateway Wiring

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the first end-to-end request path. Add the `providers` crate with the `BackendProvider` trait and a generic OpenAI-compatible adapter (covers DeepSeek, Kimi, vLLM, Ollama, etc. with config). Add a static-map `router`. Mount `/v1/messages` and `/v1/chat/completions` in the gateway. After this plan, `agent-shim serve` proxies real requests to a real upstream.

**Architecture:** `BackendProvider::complete(req, target) -> CanonicalStream` is the single contract. The OpenAI-compatible adapter encodes a `CanonicalRequest` to upstream JSON, opens a streaming POST, parses upstream SSE chunks back to `StreamEvent`, and returns the canonical stream. Non-streaming requests reuse the same code path (collected by `encode_unary` on the frontend side). Router lookup is a `BTreeMap<(FrontendKind, String), BackendTarget>` built from config at startup.

**Tech Stack:** `reqwest` with `rustls-tls` and `stream` feature, `eventsource-stream` for SSE parsing, `tokio`, `tracing`, `serde`. `mockito` for upstream mocking in tests.

---

## File Structure

`crates/providers/`:
- Create: `crates/providers/Cargo.toml`
- Create: `crates/providers/src/lib.rs` — `BackendProvider` trait, `ProviderError`, `ProviderRegistry`
- Create: `crates/providers/src/openai_compatible/mod.rs` — `OpenAiCompatibleProvider`
- Create: `crates/providers/src/openai_compatible/encode_request.rs` — `pub(crate)` body builder (also reused by Copilot in Plan 05)
- Create: `crates/providers/src/openai_compatible/parse_stream.rs` — `pub(crate)` SSE parser
- Create: `crates/providers/src/openai_compatible/parse_unary.rs` — JSON-body collector → `CanonicalStream`
- Create: `crates/providers/src/openai_compatible/wire.rs` — wire types (mostly re-using OpenAI shapes)
- Create: `crates/providers/tests/openai_compatible_smoke.rs`

`crates/router/`:
- Create: `crates/router/Cargo.toml`
- Create: `crates/router/src/lib.rs` — `Router` trait, `Resolution`, `RouteError`
- Create: `crates/router/src/static_routes.rs` — `StaticRouter`
- Create: `crates/router/src/fallback.rs` — stub for Phase 4
- Create: `crates/router/src/rate_limit.rs` — stub
- Create: `crates/router/src/circuit_breaker.rs` — stub

`crates/gateway/`:
- Modify: `crates/gateway/Cargo.toml`
- Modify: `crates/gateway/src/state.rs` — add `frontend_registry`, `router`, `provider_registry`
- Modify: `crates/gateway/src/server.rs` — mount `/v1/messages` and `/v1/chat/completions`
- Create: `crates/gateway/src/handlers/mod.rs`
- Create: `crates/gateway/src/handlers/anthropic_messages.rs`
- Create: `crates/gateway/src/handlers/openai_chat.rs`
- Modify: `crates/gateway/src/main.rs` — register handlers module
- Modify: `crates/gateway/src/commands/serve.rs` — build registry from config

Workspace:
- Modify: root `Cargo.toml` `members`

---

## Task 1: Register `providers` and `router` in workspace

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Update**

```toml
members = [
  "crates/core", "crates/config", "crates/observability",
  "crates/frontends", "crates/providers", "crates/router",
  "crates/gateway", "crates/protocol-tests",
]
```

Append to `[workspace.dependencies]`:

```toml
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json", "stream", "gzip"] }
eventsource-stream = "0.2"
mockito = "1"
```

- [ ] **Step 2: Commit (combined with Task 2)**

---

## Task 2: `providers` crate skeleton + `BackendProvider` trait

**Files:**
- Create: `crates/providers/Cargo.toml`
- Create: `crates/providers/src/lib.rs`

- [ ] **Step 1: `Cargo.toml`**

```toml
[package]
name = "agent-shim-providers"
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
name = "agent_shim_providers"
path = "src/lib.rs"

[dependencies]
agent-shim-core = { path = "../core" }
agent-shim-config = { path = "../config" }
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
bytes.workspace = true
futures = { workspace = true }
futures-util = { workspace = true }
async-trait.workspace = true
tracing.workspace = true
tokio = { workspace = true, features = ["macros", "rt", "time"] }
reqwest.workspace = true
eventsource-stream.workspace = true
http = "1"

[dev-dependencies]
mockito.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "time"] }
```

- [ ] **Step 2: `src/lib.rs`**

```rust
#![forbid(unsafe_code)]

pub mod openai_compatible;

use std::collections::BTreeMap;
use std::sync::Arc;

use agent_shim_core::{
    capabilities::ProviderCapabilities,
    request::CanonicalRequest,
    stream::CanonicalStream,
    target::BackendTarget,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider `{provider}` not configured")]
    UnknownProvider { provider: String },
    #[error("upstream HTTP error {status}: {body}")]
    Upstream { status: u16, body: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("encode error: {0}")]
    Encode(String),
    #[error("capability mismatch: {0}")]
    CapabilityMismatch(String),
}

#[async_trait::async_trait]
pub trait BackendProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> &ProviderCapabilities;
    async fn complete(&self, req: CanonicalRequest, target: BackendTarget)
        -> Result<CanonicalStream, ProviderError>;
}

#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn BackendProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self { Self::default() }
    pub fn insert(&mut self, key: impl Into<String>, p: Arc<dyn BackendProvider>) {
        self.providers.insert(key.into(), p);
    }
    pub fn get(&self, key: &str) -> Option<&Arc<dyn BackendProvider>> {
        self.providers.get(key)
    }
}
```

- [ ] **Step 3: Stub `openai_compatible` so it compiles**

`crates/providers/src/openai_compatible/mod.rs`:
```rust
pub mod encode_request;
pub mod parse_stream;
pub mod parse_unary;
pub mod wire;
```

Each of `encode_request.rs`, `parse_stream.rs`, `parse_unary.rs`, `wire.rs`: empty file or `// TODO`. Real content lands in tasks 4-7.

- [ ] **Step 4: Build**

Run: `cargo build -p agent-shim-providers`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/providers
git commit -m "feat(providers): BackendProvider trait, ProviderRegistry, error type"
```

---

## Task 3: `router` crate

**Files:**
- Create: `crates/router/Cargo.toml`
- Create: `crates/router/src/lib.rs`
- Create: `crates/router/src/static_routes.rs`
- Create: `crates/router/src/fallback.rs`
- Create: `crates/router/src/rate_limit.rs`
- Create: `crates/router/src/circuit_breaker.rs`

- [ ] **Step 1: `Cargo.toml`**

```toml
[package]
name = "agent-shim-router"
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
name = "agent_shim_router"
path = "src/lib.rs"

[dependencies]
agent-shim-core = { path = "../core" }
agent-shim-config = { path = "../config" }
thiserror.workspace = true
serde.workspace = true
```

- [ ] **Step 2: `lib.rs`**

```rust
#![forbid(unsafe_code)]

pub mod circuit_breaker;
pub mod fallback;
pub mod rate_limit;
pub mod static_routes;

use agent_shim_core::target::{BackendTarget, FrontendKind};
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum RouteError {
    #[error("no route for frontend={frontend:?} model=`{model}`")]
    NoRoute { frontend: FrontendKind, model: String },
}

pub trait Router: Send + Sync {
    fn resolve(&self, frontend: FrontendKind, model: &str) -> Result<BackendTarget, RouteError>;
}

pub use static_routes::StaticRouter;
```

- [ ] **Step 3: `static_routes.rs`**

```rust
use std::collections::BTreeMap;

use agent_shim_config::schema::GatewayConfig;
use agent_shim_core::target::{BackendTarget, FrontendKind};
use thiserror::Error;

use crate::{RouteError, Router};

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("unknown frontend `{0}` (expected anthropic_messages|openai_chat)")]
    UnknownFrontend(String),
}

#[derive(Default, Debug, Clone)]
pub struct StaticRouter {
    routes: BTreeMap<(FrontendKind, String), BackendTarget>,
}

impl StaticRouter {
    pub fn from_config(cfg: &GatewayConfig) -> Result<Self, BuildError> {
        let mut routes = BTreeMap::new();
        for r in &cfg.routes {
            let kind = match r.frontend.as_str() {
                "anthropic_messages" => FrontendKind::AnthropicMessages,
                "openai_chat" => FrontendKind::OpenAiChat,
                other => return Err(BuildError::UnknownFrontend(other.into())),
            };
            let target = BackendTarget {
                provider: provider_kind_for_upstream(cfg, &r.upstream),
                upstream_model: r.upstream_model.clone(),
                upstream: Some(r.upstream.clone()),
            };
            routes.insert((kind, r.model.clone()), target);
        }
        Ok(Self { routes })
    }
}

fn provider_kind_for_upstream(cfg: &GatewayConfig, key: &str) -> String {
    match cfg.upstreams.get(key) {
        Some(agent_shim_config::schema::UpstreamConfig::OpenAiCompatible(_)) => "openai_compatible".into(),
        Some(agent_shim_config::schema::UpstreamConfig::GithubCopilot) => "github_copilot".into(),
        None => "openai_compatible".into(),
    }
}

impl Router for StaticRouter {
    fn resolve(&self, frontend: FrontendKind, model: &str) -> Result<BackendTarget, RouteError> {
        self.routes.get(&(frontend, model.to_string()))
            .cloned()
            .ok_or_else(|| RouteError::NoRoute { frontend, model: model.to_string() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_config::schema::*;
    use agent_shim_config::secrets::Secret;

    fn cfg() -> GatewayConfig {
        let mut up = std::collections::BTreeMap::new();
        up.insert("deepseek".into(), UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: "https://api.deepseek.com".into(),
            api_key: Secret::new("k"),
            default_headers: Default::default(),
            request_timeout_secs: 30,
        }));
        GatewayConfig {
            server: ServerConfig::default(),
            logging: Default::default(),
            upstreams: up,
            routes: vec![RouteEntry {
                frontend: "openai_chat".into(),
                model: "deepseek-chat".into(),
                upstream: "deepseek".into(),
                upstream_model: "deepseek-chat".into(),
            }],
            copilot: None,
        }
    }

    #[test]
    fn resolves_known_route() {
        let r = StaticRouter::from_config(&cfg()).unwrap();
        let t = r.resolve(FrontendKind::OpenAiChat, "deepseek-chat").unwrap();
        assert_eq!(t.provider, "openai_compatible");
        assert_eq!(t.upstream_model, "deepseek-chat");
    }

    #[test]
    fn unknown_route_errors() {
        let r = StaticRouter::from_config(&cfg()).unwrap();
        let e = r.resolve(FrontendKind::OpenAiChat, "missing").unwrap_err();
        assert!(matches!(e, RouteError::NoRoute { .. }));
    }
}
```

- [ ] **Step 4: Phase-4 stubs**

`crates/router/src/fallback.rs`:
```rust
//! Phase 4 — fallback chains. Empty for v0.1.
```

`crates/router/src/rate_limit.rs`:
```rust
//! Phase 4 — per-key rate limiting. Empty for v0.1.
```

`crates/router/src/circuit_breaker.rs`:
```rust
//! Phase 4 — circuit breakers. Empty for v0.1.
```

- [ ] **Step 5: Run tests + commit**

Run: `cargo test -p agent-shim-router`
Expected: 2 passed.

```bash
git add crates/router
git commit -m "feat(router): static-route resolver with config-derived BackendTarget mapping"
```

---

## Task 4: Provider — OpenAI-compatible request encoder

**Files:**
- Modify: `crates/providers/src/openai_compatible/wire.rs`
- Modify: `crates/providers/src/openai_compatible/encode_request.rs`

- [ ] **Step 1: `wire.rs` — request shape**

```rust
//! Wire shapes for outbound chat.completions request body.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
pub struct ChatBody {
    pub model: String,
    pub messages: Vec<MsgOut>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptionsOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct StreamOptionsOut { pub include_usage: bool }

#[derive(Debug, Serialize)]
pub struct MsgOut {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ToolCallOut {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: FnOut,
}

#[derive(Debug, Serialize)]
pub struct FnOut { pub name: String, pub arguments: String }

#[derive(Debug, Serialize)]
pub struct ToolOut {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: ToolFnOut,
}

#[derive(Debug, Serialize)]
pub struct ToolFnOut {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
}
```

- [ ] **Step 2: `encode_request.rs`**

```rust
use serde_json::{json, Value};

use agent_shim_core::content::ContentBlock;
use agent_shim_core::message::{Message, MessageRole, SystemSource};
use agent_shim_core::request::{CanonicalRequest, ResponseFormat};
use agent_shim_core::tool::{ToolCallArguments, ToolChoice};

use super::wire::*;

pub(crate) fn build(req: &CanonicalRequest, upstream_model: &str) -> ChatBody {
    let mut messages: Vec<MsgOut> = Vec::with_capacity(req.system.len() + req.messages.len());
    for sys in &req.system {
        let role = match sys.source {
            SystemSource::OpenAiDeveloper => "developer",
            _ => "system",
        };
        messages.push(MsgOut {
            role: role.into(),
            content: Some(Value::String(blocks_to_text(&sys.content))),
            tool_calls: vec![],
            tool_call_id: None,
            name: None,
        });
    }
    for m in &req.messages {
        messages.push(message_to_wire(m));
    }

    let tools = req.tools.iter().map(|t| ToolOut {
        kind: "function",
        function: ToolFnOut {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.input_schema.clone(),
        },
    }).collect();

    let tool_choice = match &req.tool_choice {
        ToolChoice::Auto => None,
        ToolChoice::None => Some(json!("none")),
        ToolChoice::Required => Some(json!("required")),
        ToolChoice::Specific { name } => Some(json!({
            "type": "function",
            "function": { "name": name }
        })),
    };

    let response_format = req.response_format.as_ref().map(|r| match r {
        ResponseFormat::Text => json!({ "type": "text" }),
        ResponseFormat::JsonObject => json!({ "type": "json_object" }),
        ResponseFormat::JsonSchema { name, schema, strict } => json!({
            "type": "json_schema",
            "json_schema": { "name": name, "schema": schema, "strict": strict }
        }),
    });

    ChatBody {
        model: upstream_model.to_string(),
        messages,
        tools,
        tool_choice,
        max_tokens: req.generation.max_tokens,
        temperature: req.generation.temperature,
        top_p: req.generation.top_p,
        presence_penalty: req.generation.presence_penalty,
        frequency_penalty: req.generation.frequency_penalty,
        stop: req.generation.stop_sequences.clone(),
        seed: req.generation.seed,
        stream: req.stream,
        stream_options: if req.stream { Some(StreamOptionsOut { include_usage: true }) } else { None },
        response_format,
    }
}

fn message_to_wire(m: &Message) -> MsgOut {
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut tool_call_id: Option<String> = None;

    for b in &m.content {
        match b {
            ContentBlock::Text(t) => text_parts.push(t.text.clone()),
            ContentBlock::ToolCall(tc) => {
                let args = match &tc.arguments {
                    ToolCallArguments::Complete(v) => v.to_string(),
                    ToolCallArguments::Streaming(r) => r.get().to_string(),
                };
                tool_calls.push(ToolCallOut {
                    id: tc.id.0.clone(),
                    kind: "function",
                    function: FnOut { name: tc.name.clone(), arguments: args },
                });
            }
            ContentBlock::ToolResult(tr) => {
                tool_call_id = Some(tr.tool_call_id.0.clone());
                // Tool results may carry multimodal content; we flatten to JSON for now.
                text_parts.push(tr.content.to_string());
            }
            // Vision/audio/file/reasoning passthrough is Phase 2.
            _ => {}
        }
    }

    let role = match (m.role, tool_call_id.is_some()) {
        (_, true) => "tool".to_string(),
        (MessageRole::User, _) => "user".into(),
        (MessageRole::Assistant, _) => "assistant".into(),
        (MessageRole::Tool, _) => "tool".into(),
    };

    let content = if text_parts.is_empty() && !tool_calls.is_empty() {
        None
    } else {
        Some(Value::String(text_parts.join("")))
    };

    MsgOut { role, content, tool_calls, tool_call_id, name: m.name.clone() }
}

fn blocks_to_text(blocks: &[ContentBlock]) -> String {
    blocks.iter().filter_map(|b| match b {
        ContentBlock::Text(t) => Some(t.text.clone()),
        _ => None,
    }).collect::<Vec<_>>().join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::ids::RequestId;
    use agent_shim_core::message::{Message, MessageRole};
    use agent_shim_core::request::GenerationOptions;
    use agent_shim_core::target::{FrontendInfo, FrontendKind, FrontendModel};

    fn req() -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo { kind: FrontendKind::OpenAiChat, api_path: "/v1/chat/completions".into() },
            model: FrontendModel::from("alias"),
            system: vec![],
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::text("hi")],
                name: None,
                extensions: Default::default(),
            }],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            generation: GenerationOptions { max_tokens: Some(50), ..Default::default() },
            response_format: None,
            stream: true,
            metadata: Default::default(),
            extensions: Default::default(),
        }
    }

    #[test]
    fn streaming_body_includes_stream_options_include_usage() {
        let body = build(&req(), "deepseek-chat");
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["stream"], true);
        assert_eq!(json["stream_options"]["include_usage"], true);
        assert_eq!(json["max_tokens"], 50);
        assert_eq!(json["model"], "deepseek-chat");
    }

    #[test]
    fn non_streaming_omits_stream_options() {
        let mut r = req();
        r.stream = false;
        let body = build(&r, "x");
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["stream"], false);
        assert!(json.get("stream_options").is_none());
    }
}
```

- [ ] **Step 3: Run tests + commit**

Run: `cargo test -p agent-shim-providers encode_request`
Expected: 2 passed.

```bash
git add crates/providers
git commit -m "feat(providers/openai_compat): request body encoder with stream_options.include_usage"
```

---

## Task 5: Provider — SSE chunk parser

**Files:**
- Modify: `crates/providers/src/openai_compatible/parse_stream.rs`

- [ ] **Step 1: Implementation**

```rust
//! Convert an upstream `chat.completions` SSE byte stream to `CanonicalStream`.

use std::pin::Pin;

use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures::stream::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::Value;

use agent_shim_core::error::StreamError;
use agent_shim_core::ids::{ResponseId, ToolCallId};
use agent_shim_core::message::MessageRole;
use agent_shim_core::stream::{CanonicalStream, ContentBlockKind, StreamEvent};
use agent_shim_core::usage::{StopReason, Usage};

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<UsageWire>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Deserialize)]
struct ToolCallDelta {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FnDelta>,
}

#[derive(Deserialize, Default)]
struct FnDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct UsageWire {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
}

pub(crate) fn parse<S>(byte_stream: S) -> CanonicalStream
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let events = byte_stream
        .map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)))
        .eventsource();

    let mut emitted_response_start = false;
    let mut emitted_message_start = false;
    let mut emitted_text_block = false;
    let mut tool_blocks_open: std::collections::BTreeSet<u32> = Default::default();

    let s = events.flat_map(move |evt| {
        let mut out: Vec<Result<StreamEvent, StreamError>> = Vec::new();
        let evt = match evt {
            Ok(e) => e,
            Err(e) => {
                out.push(Err(StreamError::Decode(e.to_string())));
                return futures::stream::iter(out);
            }
        };
        if evt.data.trim() == "[DONE]" {
            // Emit ResponseStop only if we haven't already.
            out.push(Ok(StreamEvent::ResponseStop { usage: None }));
            return futures::stream::iter(out);
        }
        let chunk: Chunk = match serde_json::from_str(&evt.data) {
            Ok(c) => c,
            Err(e) => {
                out.push(Err(StreamError::Decode(format!("chunk parse: {e}"))));
                return futures::stream::iter(out);
            }
        };

        if !emitted_response_start {
            emitted_response_start = true;
            out.push(Ok(StreamEvent::ResponseStart {
                id: ResponseId(chunk.id.clone().unwrap_or_else(|| format!("resp_{}", uuid_like()))),
                model: chunk.model.clone().unwrap_or_default(),
                created_at_unix: 0,
            }));
        }

        for choice in chunk.choices {
            if !emitted_message_start && choice.delta.role.as_deref() == Some("assistant") {
                emitted_message_start = true;
                out.push(Ok(StreamEvent::MessageStart { role: MessageRole::Assistant }));
            }

            if let Some(content) = choice.delta.content.filter(|c| !c.is_empty()) {
                if !emitted_message_start {
                    emitted_message_start = true;
                    out.push(Ok(StreamEvent::MessageStart { role: MessageRole::Assistant }));
                }
                if !emitted_text_block {
                    emitted_text_block = true;
                    out.push(Ok(StreamEvent::ContentBlockStart { index: 0, kind: ContentBlockKind::Text }));
                }
                out.push(Ok(StreamEvent::TextDelta { index: 0, text: content }));
            }

            for tc in choice.delta.tool_calls {
                if !emitted_message_start {
                    emitted_message_start = true;
                    out.push(Ok(StreamEvent::MessageStart { role: MessageRole::Assistant }));
                }
                let block_index = tc.index + 1; // text reserved at 0
                if !tool_blocks_open.contains(&block_index) {
                    tool_blocks_open.insert(block_index);
                    if let Some(fn_) = &tc.function {
                        out.push(Ok(StreamEvent::ToolCallStart {
                            index: block_index,
                            id: ToolCallId::from_provider(tc.id.clone().unwrap_or_default()),
                            name: fn_.name.clone().unwrap_or_default(),
                        }));
                    } else {
                        out.push(Ok(StreamEvent::ToolCallStart {
                            index: block_index,
                            id: ToolCallId::from_provider(tc.id.clone().unwrap_or_default()),
                            name: String::new(),
                        }));
                    }
                }
                if let Some(fn_) = tc.function {
                    if let Some(args) = fn_.arguments {
                        if !args.is_empty() {
                            out.push(Ok(StreamEvent::ToolCallArgumentsDelta {
                                index: block_index,
                                json_fragment: args,
                            }));
                        }
                    }
                }
            }

            if let Some(reason) = choice.finish_reason {
                // Close text block if open
                if emitted_text_block {
                    out.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
                }
                for &idx in &tool_blocks_open {
                    out.push(Ok(StreamEvent::ContentBlockStop { index: idx }));
                }
                out.push(Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::from_provider_string(&reason),
                    stop_sequence: None,
                }));
            }
        }

        if let Some(u) = chunk.usage {
            let usage = Usage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                ..Default::default()
            };
            out.push(Ok(StreamEvent::UsageDelta { usage }));
        }

        let _ = Value::Null; // silence unused
        futures::stream::iter(out)
    });

    Box::pin(s) as Pin<Box<dyn Stream<Item = Result<StreamEvent, StreamError>> + Send>>
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos().to_string()).unwrap_or_default()
}
```

- [ ] **Step 2: Build**

Run: `cargo build -p agent-shim-providers`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/providers
git commit -m "feat(providers/openai_compat): SSE parser → CanonicalStream with tool-call deltas"
```

---

## Task 6: Provider — unary parser

**Files:**
- Modify: `crates/providers/src/openai_compatible/parse_unary.rs`

- [ ] **Step 1: Implementation**

```rust
//! Parse a non-streaming `chat.completions` JSON response into `CanonicalStream`
//! events (one of each, in order). Lets the gateway always work with streams.

use std::pin::Pin;

use futures::stream::{self, Stream};
use serde::Deserialize;

use agent_shim_core::error::StreamError;
use agent_shim_core::ids::{ResponseId, ToolCallId};
use agent_shim_core::message::MessageRole;
use agent_shim_core::stream::{CanonicalStream, ContentBlockKind, StreamEvent};
use agent_shim_core::usage::{StopReason, Usage};

#[derive(Deserialize)]
struct Body {
    id: String,
    model: String,
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<UsageWire>,
}

#[derive(Deserialize)]
struct Choice { message: Msg, finish_reason: String }

#[derive(Deserialize)]
struct Msg {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

#[derive(Deserialize)]
struct ToolCall { id: String, function: Fn_ }

#[derive(Deserialize)]
struct Fn_ { name: String, arguments: String }

#[derive(Deserialize)]
struct UsageWire {
    #[serde(default)] prompt_tokens: Option<u32>,
    #[serde(default)] completion_tokens: Option<u32>,
}

pub(crate) fn parse(body_bytes: &[u8]) -> Result<CanonicalStream, crate::ProviderError> {
    let body: Body = serde_json::from_slice(body_bytes)
        .map_err(|e| crate::ProviderError::Decode(e.to_string()))?;
    let mut events: Vec<Result<StreamEvent, StreamError>> = vec![];
    events.push(Ok(StreamEvent::ResponseStart {
        id: ResponseId(body.id), model: body.model, created_at_unix: 0,
    }));
    events.push(Ok(StreamEvent::MessageStart { role: MessageRole::Assistant }));
    let choice = body.choices.into_iter().next();
    if let Some(c) = choice {
        if let Some(text) = c.message.content.filter(|s| !s.is_empty()) {
            events.push(Ok(StreamEvent::ContentBlockStart { index: 0, kind: ContentBlockKind::Text }));
            events.push(Ok(StreamEvent::TextDelta { index: 0, text }));
            events.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
        }
        for (i, tc) in c.message.tool_calls.into_iter().enumerate() {
            let idx = (i as u32) + 1;
            events.push(Ok(StreamEvent::ToolCallStart {
                index: idx,
                id: ToolCallId::from_provider(tc.id),
                name: tc.function.name,
            }));
            if !tc.function.arguments.is_empty() {
                events.push(Ok(StreamEvent::ToolCallArgumentsDelta {
                    index: idx, json_fragment: tc.function.arguments,
                }));
            }
            events.push(Ok(StreamEvent::ToolCallStop { index: idx }));
            events.push(Ok(StreamEvent::ContentBlockStop { index: idx }));
        }
        events.push(Ok(StreamEvent::MessageStop {
            stop_reason: StopReason::from_provider_string(&c.finish_reason),
            stop_sequence: None,
        }));
    }
    if let Some(u) = body.usage {
        events.push(Ok(StreamEvent::UsageDelta { usage: Usage {
            input_tokens: u.prompt_tokens, output_tokens: u.completion_tokens, ..Default::default()
        }}));
    }
    events.push(Ok(StreamEvent::ResponseStop { usage: None }));
    Ok(Box::pin(stream::iter(events)) as Pin<Box<dyn Stream<Item = _> + Send>>)
}
```

- [ ] **Step 2: Build + commit**

Run: `cargo build -p agent-shim-providers`

```bash
git add crates/providers
git commit -m "feat(providers/openai_compat): unary JSON parser produces a one-shot CanonicalStream"
```

---

## Task 7: Provider — `OpenAiCompatibleProvider` impl

**Files:**
- Modify: `crates/providers/src/openai_compatible/mod.rs`

- [ ] **Step 1: Implementation**

```rust
pub mod encode_request;
pub mod parse_stream;
pub mod parse_unary;
pub mod wire;

use std::time::Duration;

use agent_shim_config::schema::OpenAiCompatibleUpstream;
use agent_shim_core::{
    capabilities::ProviderCapabilities,
    request::CanonicalRequest,
    stream::CanonicalStream,
    target::BackendTarget,
};
use reqwest::Client;
use tracing::warn;

use crate::{BackendProvider, ProviderError};

pub struct OpenAiCompatibleProvider {
    name: &'static str,
    upstream: OpenAiCompatibleUpstream,
    capabilities: ProviderCapabilities,
    client: Client,
}

impl OpenAiCompatibleProvider {
    pub fn new(upstream: OpenAiCompatibleUpstream) -> Result<Self, ProviderError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(upstream.request_timeout_secs))
            .pool_idle_timeout(Some(Duration::from_secs(60)))
            .build()
            .map_err(|e| ProviderError::Network(e.to_string()))?;
        let caps = ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            json_mode: true,
            json_schema: true,
            system_prompts: true,
            developer_prompts: true,
            ..Default::default()
        };
        Ok(Self { name: "openai_compatible", upstream, capabilities: caps, client })
    }
}

#[async_trait::async_trait]
impl BackendProvider for OpenAiCompatibleProvider {
    fn name(&self) -> &'static str { self.name }
    fn capabilities(&self) -> &ProviderCapabilities { &self.capabilities }

    async fn complete(
        &self,
        req: CanonicalRequest,
        target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError> {
        let body = encode_request::build(&req, &target.upstream_model);
        let url = format!("{}/v1/chat/completions", self.upstream.base_url.trim_end_matches('/'));
        let mut http = self.client
            .post(&url)
            .bearer_auth(self.upstream.api_key.expose())
            .header("content-type", "application/json");
        for (k, v) in &self.upstream.default_headers {
            http = http.header(k, v);
        }
        let response = http.json(&body).send().await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Upstream { status: status.as_u16(), body });
        }

        if req.stream {
            let bytes = response.bytes_stream();
            Ok(parse_stream::parse(bytes))
        } else {
            let bytes = response.bytes().await
                .map_err(|e| ProviderError::Network(e.to_string()))?;
            parse_unary::parse(&bytes)
        }
    }
}

impl OpenAiCompatibleProvider {
    /// Convenience for tests / Copilot reuse: encode-only.
    pub fn encode_for_test(&self, req: &CanonicalRequest, model: &str) -> wire::ChatBody {
        encode_request::build(req, model)
    }
}

#[allow(dead_code)]
fn unused_warn() { warn!("unused"); }
```

- [ ] **Step 2: Build**

Run: `cargo build -p agent-shim-providers`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/providers
git commit -m "feat(providers/openai_compat): OpenAiCompatibleProvider wiring streaming + unary"
```

---

## Task 8: Smoke test against `mockito`

**Files:**
- Create: `crates/providers/tests/openai_compatible_smoke.rs`

- [ ] **Step 1: Test**

```rust
use agent_shim_config::schema::OpenAiCompatibleUpstream;
use agent_shim_config::secrets::Secret;
use agent_shim_core::{
    content::ContentBlock, ids::RequestId, message::{Message, MessageRole},
    request::{CanonicalRequest, GenerationOptions}, target::{BackendTarget, FrontendInfo, FrontendKind, FrontendModel},
    tool::ToolChoice,
};
use agent_shim_providers::openai_compatible::OpenAiCompatibleProvider;
use agent_shim_providers::BackendProvider;
use futures::StreamExt;

fn req(stream: bool) -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo { kind: FrontendKind::OpenAiChat, api_path: "/v1/chat/completions".into() },
        model: FrontendModel::from("alias"),
        system: vec![],
        messages: vec![Message {
            role: MessageRole::User,
            content: vec![ContentBlock::text("hello")],
            name: None,
            extensions: Default::default(),
        }],
        tools: vec![], tool_choice: ToolChoice::Auto,
        generation: GenerationOptions { max_tokens: Some(20), ..Default::default() },
        response_format: None, stream, metadata: Default::default(), extensions: Default::default(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unary_request_returns_canonical_stream_with_text() {
    let mut server = mockito::Server::new_async().await;
    let _m = server.mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{
            "id":"chatcmpl_1","model":"deepseek-chat",
            "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}
        }"#)
        .create_async().await;

    let upstream = OpenAiCompatibleUpstream {
        base_url: server.url(), api_key: Secret::new("k"),
        default_headers: Default::default(), request_timeout_secs: 5,
    };
    let provider = OpenAiCompatibleProvider::new(upstream).unwrap();
    let target = BackendTarget { provider: "openai_compatible".into(), upstream_model: "deepseek-chat".into(), upstream: Some("ds".into()) };

    let mut s = provider.complete(req(false), target).await.unwrap();
    let mut events = vec![];
    while let Some(e) = s.next().await { events.push(e.unwrap()); }
    // We expect at least: ResponseStart, MessageStart, BlockStart, TextDelta, BlockStop, MessageStop, UsageDelta, ResponseStop.
    let kinds: Vec<&'static str> = events.iter().map(|e| match e {
        agent_shim_core::stream::StreamEvent::ResponseStart {..} => "rs",
        agent_shim_core::stream::StreamEvent::MessageStart {..} => "ms",
        agent_shim_core::stream::StreamEvent::ContentBlockStart {..} => "cbs",
        agent_shim_core::stream::StreamEvent::TextDelta {..} => "td",
        agent_shim_core::stream::StreamEvent::ContentBlockStop {..} => "cbe",
        agent_shim_core::stream::StreamEvent::MessageStop {..} => "msend",
        agent_shim_core::stream::StreamEvent::UsageDelta {..} => "ud",
        agent_shim_core::stream::StreamEvent::ResponseStop {..} => "rsend",
        _ => "?"
    }).collect();
    assert_eq!(kinds, vec!["rs", "ms", "cbs", "td", "cbe", "msend", "ud", "rsend"]);
}

#[tokio::test(flavor = "current_thread")]
async fn streaming_request_yields_text_deltas() {
    let mut server = mockito::Server::new_async().await;
    let body = "data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n\
                data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n\
                data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n\
                data: [DONE]\n\n";
    let _m = server.mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async().await;

    let upstream = OpenAiCompatibleUpstream {
        base_url: server.url(), api_key: Secret::new("k"),
        default_headers: Default::default(), request_timeout_secs: 5,
    };
    let provider = OpenAiCompatibleProvider::new(upstream).unwrap();
    let target = BackendTarget { provider: "openai_compatible".into(), upstream_model: "m".into(), upstream: None };

    let mut s = provider.complete(req(true), target).await.unwrap();
    let mut text = String::new();
    while let Some(e) = s.next().await {
        if let Ok(agent_shim_core::stream::StreamEvent::TextDelta { text: t, .. }) = e {
            text.push_str(&t);
        }
    }
    assert_eq!(text, "Hi");
}
```

- [ ] **Step 2: Run + commit**

Run: `cargo test -p agent-shim-providers --test openai_compatible_smoke`
Expected: 2 passed.

```bash
git add crates/providers/tests
git commit -m "test(providers): mockito smoke tests for OpenAI-compatible streaming + unary"
```

---

## Task 9: Gateway state — register registries

**Files:**
- Modify: `crates/gateway/Cargo.toml`
- Modify: `crates/gateway/src/state.rs`

- [ ] **Step 1: `Cargo.toml`**

Add to `[dependencies]`:

```toml
agent-shim-frontends = { path = "../frontends" }
agent-shim-providers = { path = "../providers" }
agent-shim-router = { path = "../router" }
futures = { workspace = true }
futures-util = { workspace = true }
http-body-util = { workspace = true }
async-trait.workspace = true
```

- [ ] **Step 2: `state.rs`**

```rust
use std::sync::Arc;

use agent_shim_config::schema::GatewayConfig;
use agent_shim_frontends::{anthropic_messages::AnthropicMessages, openai_chat::OpenAiChat};
use agent_shim_providers::{openai_compatible::OpenAiCompatibleProvider, BackendProvider, ProviderRegistry};
use agent_shim_router::StaticRouter;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<GatewayConfig>,
    pub anthropic: Arc<AnthropicMessages>,
    pub openai: Arc<OpenAiChat>,
    pub providers: Arc<ProviderRegistry>,
    pub router: Arc<StaticRouter>,
}

impl AppState {
    pub fn build(config: GatewayConfig) -> anyhow::Result<Self> {
        use agent_shim_config::schema::UpstreamConfig;

        let mut providers = ProviderRegistry::new();
        for (key, up) in &config.upstreams {
            match up {
                UpstreamConfig::OpenAiCompatible(c) => {
                    let p = OpenAiCompatibleProvider::new(c.clone())?;
                    providers.insert(key.clone(), Arc::new(p) as Arc<dyn BackendProvider>);
                }
                UpstreamConfig::GithubCopilot => {
                    // Provider registered in Plan 05.
                }
            }
        }
        let router = StaticRouter::from_config(&config)?;

        let keepalive = std::time::Duration::from_secs(config.server.keepalive_secs);
        let keepalive = if keepalive.is_zero() { None } else { Some(keepalive) };

        Ok(Self {
            config: Arc::new(config),
            anthropic: Arc::new(AnthropicMessages { keepalive }),
            openai: Arc::new(OpenAiChat { keepalive, clock_override: None }),
            providers: Arc::new(providers),
            router: Arc::new(router),
        })
    }
}
```

- [ ] **Step 3: Update `serve.rs`**

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
    let state = AppState::build(cfg).context("building app state")?;
    server::run(state).await
}
```

- [ ] **Step 4: Build, commit**

Run: `cargo build -p agent-shim`
Expected: clean.

```bash
git add crates/gateway
git commit -m "feat(gateway): app state builds providers/router from config at startup"
```

---

## Task 10: HTTP handlers — `/v1/messages` and `/v1/chat/completions`

**Files:**
- Create: `crates/gateway/src/handlers/mod.rs`
- Create: `crates/gateway/src/handlers/anthropic_messages.rs`
- Create: `crates/gateway/src/handlers/openai_chat.rs`
- Modify: `crates/gateway/src/server.rs`
- Modify: `crates/gateway/src/main.rs`

- [ ] **Step 1: `handlers/mod.rs`**

```rust
pub mod anthropic_messages;
pub mod openai_chat;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use agent_shim_frontends::FrontendError;
use agent_shim_providers::ProviderError;
use agent_shim_router::RouteError;

#[derive(Debug)]
pub enum HandlerError {
    Frontend(FrontendError),
    Route(RouteError),
    Provider(ProviderError),
    UnknownProvider(String),
}

impl IntoResponse for HandlerError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            HandlerError::Frontend(FrontendError::InvalidBody(m)) => (StatusCode::BAD_REQUEST, m),
            HandlerError::Frontend(FrontendError::Unsupported(m)) => (StatusCode::BAD_REQUEST, m),
            HandlerError::Frontend(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            HandlerError::Route(e) => (StatusCode::NOT_FOUND, e.to_string()),
            HandlerError::Provider(ProviderError::Upstream { status, body }) => {
                let st = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
                (st, body)
            }
            HandlerError::Provider(e) => (StatusCode::BAD_GATEWAY, e.to_string()),
            HandlerError::UnknownProvider(p) => (StatusCode::INTERNAL_SERVER_ERROR, format!("provider `{p}` not configured")),
        };
        let body = serde_json::json!({"error": {"message": msg}});
        (status, axum::Json(body)).into_response()
    }
}
```

- [ ] **Step 2: `handlers/anthropic_messages.rs`**

```rust
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use agent_shim_frontends::{FrontendProtocol, FrontendResponse};

use super::HandlerError;
use crate::state::AppState;

pub async fn handle(
    State(state): State<AppState>,
    body: bytes::Bytes,
) -> Result<Response, HandlerError> {
    let req = state.anthropic.decode_request(&body).map_err(HandlerError::Frontend)?;
    let target = state.router.resolve(req.frontend.kind, &req.model.0).map_err(HandlerError::Route)?;
    let upstream_key = target.upstream.clone().unwrap_or_else(|| target.provider.clone());
    let provider = state.providers.get(&upstream_key)
        .ok_or_else(|| HandlerError::UnknownProvider(upstream_key.clone()))?
        .clone();

    let stream_mode = req.stream;
    let canonical = provider.complete(req, target).await.map_err(HandlerError::Provider)?;

    if stream_mode {
        match state.anthropic.encode_stream(canonical) {
            FrontendResponse::Stream { content_type, stream } => {
                let body = Body::from_stream(stream.map(|r| r.map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                })));
                Ok((StatusCode::OK,
                    [(header::CONTENT_TYPE, content_type),
                     (header::CACHE_CONTROL, "no-cache".into())],
                    body).into_response())
            }
            _ => unreachable!(),
        }
    } else {
        let collected = collect_canonical_to_response(canonical).await;
        let resp = state.anthropic.encode_unary(collected).map_err(HandlerError::Frontend)?;
        match resp {
            FrontendResponse::Unary { content_type, body } => {
                Ok((StatusCode::OK, [(header::CONTENT_TYPE, content_type)], body).into_response())
            }
            _ => unreachable!(),
        }
    }
}

use futures::StreamExt;
use agent_shim_core::{
    content::{ContentBlock, ReasoningBlock, TextBlock},
    extensions::ExtensionMap,
    ids::ResponseId,
    response::CanonicalResponse,
    stream::{CanonicalStream, StreamEvent},
    tool::{ToolCallArguments, ToolCallBlock},
    usage::{StopReason, Usage},
};
use serde_json::value::to_raw_value;

async fn collect_canonical_to_response(mut s: CanonicalStream) -> CanonicalResponse {
    let mut id = ResponseId::new();
    let mut model = String::new();
    let mut text_buf = String::new();
    let mut tool_calls: Vec<ToolCallBlock> = Vec::new();
    let mut tool_arg_buf: std::collections::BTreeMap<u32, String> = Default::default();
    let mut tool_meta: std::collections::BTreeMap<u32, (agent_shim_core::ids::ToolCallId, String)> = Default::default();
    let mut reasoning_buf = String::new();
    let mut stop_reason = StopReason::EndTurn;
    let mut stop_sequence: Option<String> = None;
    let mut usage: Option<Usage> = None;

    while let Some(e) = s.next().await {
        let Ok(e) = e else { continue };
        match e {
            StreamEvent::ResponseStart { id: rid, model: m, .. } => { id = rid; model = m; }
            StreamEvent::TextDelta { text, .. } => text_buf.push_str(&text),
            StreamEvent::ReasoningDelta { text, .. } => reasoning_buf.push_str(&text),
            StreamEvent::ToolCallStart { index, id, name } => {
                tool_meta.insert(index, (id, name));
                tool_arg_buf.entry(index).or_default();
            }
            StreamEvent::ToolCallArgumentsDelta { index, json_fragment } => {
                tool_arg_buf.entry(index).or_default().push_str(&json_fragment);
            }
            StreamEvent::MessageStop { stop_reason: r, stop_sequence: ss } => { stop_reason = r; stop_sequence = ss; }
            StreamEvent::UsageDelta { usage: u } => usage = Some(u),
            StreamEvent::ResponseStop { usage: Some(u) } => usage = Some(u),
            _ => {}
        }
    }

    let mut content: Vec<ContentBlock> = Vec::new();
    if !reasoning_buf.is_empty() {
        content.push(ContentBlock::Reasoning(ReasoningBlock { text: reasoning_buf, extensions: ExtensionMap::default() }));
    }
    if !text_buf.is_empty() {
        content.push(ContentBlock::Text(TextBlock { text: text_buf, extensions: ExtensionMap::default() }));
    }
    for (idx, (tc_id, name)) in tool_meta {
        let raw = tool_arg_buf.remove(&idx).unwrap_or_default();
        let args = if raw.is_empty() {
            ToolCallArguments::Complete(serde_json::Value::Object(Default::default()))
        } else {
            match to_raw_value(&serde_json::from_str::<serde_json::Value>(&raw).unwrap_or(serde_json::Value::String(raw))) {
                Ok(rv) => ToolCallArguments::Streaming(rv),
                Err(_) => ToolCallArguments::Complete(serde_json::Value::Null),
            }
        };
        tool_calls.push(ToolCallBlock { id: tc_id, name, arguments: args, extensions: ExtensionMap::default() });
    }
    for tc in tool_calls { content.push(ContentBlock::ToolCall(tc)); }

    CanonicalResponse { id, model, content, stop_reason, stop_sequence, usage }
}
```

- [ ] **Step 3: `handlers/openai_chat.rs`**

Same shape, using `state.openai`:

```rust
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;

use agent_shim_frontends::{FrontendProtocol, FrontendResponse};

use super::HandlerError;
use crate::state::AppState;

pub async fn handle(
    State(state): State<AppState>,
    body: bytes::Bytes,
) -> Result<Response, HandlerError> {
    let req = state.openai.decode_request(&body).map_err(HandlerError::Frontend)?;
    let target = state.router.resolve(req.frontend.kind, &req.model.0).map_err(HandlerError::Route)?;
    let upstream_key = target.upstream.clone().unwrap_or_else(|| target.provider.clone());
    let provider = state.providers.get(&upstream_key)
        .ok_or_else(|| HandlerError::UnknownProvider(upstream_key.clone()))?.clone();

    let stream_mode = req.stream;
    let canonical = provider.complete(req, target).await.map_err(HandlerError::Provider)?;

    if stream_mode {
        match state.openai.encode_stream(canonical) {
            FrontendResponse::Stream { content_type, stream } => {
                let body = Body::from_stream(stream.map(|r| r.map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                })));
                Ok((StatusCode::OK,
                    [(header::CONTENT_TYPE, content_type),
                     (header::CACHE_CONTROL, "no-cache".into())],
                    body).into_response())
            }
            _ => unreachable!(),
        }
    } else {
        // Reuse the same collector as anthropic; promote it to a shared helper.
        let collected = super::anthropic_messages::collect_canonical_to_response(canonical).await;
        match state.openai.encode_unary(collected).map_err(HandlerError::Frontend)? {
            FrontendResponse::Unary { content_type, body } => {
                Ok((StatusCode::OK, [(header::CONTENT_TYPE, content_type)], body).into_response())
            }
            _ => unreachable!(),
        }
    }
}
```

Note: the helper `collect_canonical_to_response` is `pub(crate)` in `anthropic_messages.rs` — make it so:

In `anthropic_messages.rs`, change `async fn collect_canonical_to_response` to `pub(crate) async fn collect_canonical_to_response`.

- [ ] **Step 4: Mount routes in `server.rs`**

```rust
use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::{routing::{get, post}, Router};
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

use agent_shim_observability::request_id::RequestIdLayer;

use crate::handlers;
use crate::state::AppState;
use crate::shutdown;

pub async fn run(state: AppState) -> Result<()> {
    let addr: SocketAddr = format!("{}:{}", state.config.server.bind, state.config.server.port)
        .parse()
        .context("invalid bind address")?;

    let app = Router::new()
        .route("/healthz", get(health))
        .route("/v1/messages", post(handlers::anthropic_messages::handle))
        .route("/v1/chat/completions", post(handlers::openai_chat::handle))
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

- [ ] **Step 5: Wire in `main.rs`**

Add `mod handlers;` near the other module declarations:

```rust
mod cli;
mod commands;
mod handlers;
mod server;
mod shutdown;
mod state;
```

- [ ] **Step 6: Build**

Run: `cargo build -p agent-shim`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/gateway
git commit -m "feat(gateway): mount /v1/messages and /v1/chat/completions handlers"
```

---

## Task 11: End-to-end gateway test against `mockito`

**Files:**
- Create: `crates/gateway/tests/e2e_openai_chat.rs`

- [ ] **Step 1: Test**

```rust
use std::collections::BTreeMap;
use std::time::Duration;

use agent_shim_config::schema::*;
use agent_shim_config::secrets::Secret;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_chat_streaming_round_trip() {
    let mut upstream_server = mockito::Server::new_async().await;
    let body = "data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n\
                data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n\
                data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                data: [DONE]\n\n";
    let _m = upstream_server.mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async().await;

    let mut upstreams = BTreeMap::new();
    upstreams.insert("ds".into(), UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
        base_url: upstream_server.url(),
        api_key: Secret::new("k"),
        default_headers: Default::default(),
        request_timeout_secs: 5,
    }));
    let cfg = GatewayConfig {
        server: ServerConfig { bind: "127.0.0.1".into(), port: 0, keepalive_secs: 0 },
        logging: LoggingConfig::default(),
        upstreams,
        routes: vec![RouteEntry {
            frontend: "openai_chat".into(),
            model: "deepseek-chat".into(),
            upstream: "ds".into(),
            upstream_model: "m".into(),
        }],
        copilot: None,
    };

    // Build gateway state and start it on a free port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = agent_shim::state::AppState::build(cfg).unwrap();
    let app = axum::Router::new()
        .route("/v1/chat/completions", axum::routing::post(agent_shim::handlers::openai_chat::handle))
        .with_state(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", addr))
        .header("content-type", "application/json")
        .body(r#"{"model":"deepseek-chat","messages":[{"role":"user","content":"hi"}],"stream":true}"#)
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let txt = resp.text().await.unwrap();
    assert!(txt.contains("\"role\":\"assistant\""));
    assert!(txt.contains("\"content\":\"Hi\""));
    assert!(txt.contains("\"finish_reason\":\"stop\""));
    assert!(txt.ends_with("data: [DONE]\n\n"));
}
```

For this test to work, expose the relevant gateway internals as a library. In `crates/gateway/Cargo.toml` add a `[lib]` entry:

```toml
[lib]
name = "agent_shim"
path = "src/lib.rs"
```

And create `crates/gateway/src/lib.rs`:

```rust
//! Library form of the gateway, used by integration tests.

pub mod handlers;
pub mod state;
```

(`main.rs` keeps its own `mod` declarations; that's fine — Cargo treats binary modules and library modules independently.)

Update `mockito` workspace dep usage in `crates/gateway/Cargo.toml`:

```toml
[dev-dependencies]
mockito.workspace = true
```

- [ ] **Step 2: Run + commit**

Run: `cargo test -p agent-shim --test e2e_openai_chat`
Expected: PASS.

```bash
git add crates/gateway
git commit -m "test(gateway): end-to-end OpenAI chat streaming through gateway → mockito upstream"
```

---

## Task 12: Smoke command for manual testing

**Files:**
- Modify: `config/gateway.example.yaml`

- [ ] **Step 1: Document a working setup**

Replace `config/gateway.example.yaml` with a config that works against DeepSeek out of the box (when `DEEPSEEK_API_KEY` is set):

```yaml
server:
  bind: 127.0.0.1
  port: 8787
  keepalive_secs: 15

logging:
  format: pretty
  filter: info,agent_shim=debug

upstreams:
  deepseek:
    kind: openai_compatible
    base_url: https://api.deepseek.com
    api_key: ${DEEPSEEK_API_KEY}
    request_timeout_secs: 120

routes:
  - frontend: openai_chat
    model: deepseek-chat
    upstream: deepseek
    upstream_model: deepseek-chat
```

- [ ] **Step 2: Commit**

```bash
git add config/gateway.example.yaml
git commit -m "docs(config): runnable DeepSeek example for openai_chat frontend"
```

---

## Self-Review Notes

- Spec §3 `BackendProvider` trait shape implemented (`name`, `capabilities`, `complete -> CanonicalStream`). ✓
- Spec §3 boundary rule: router doesn't touch provider JSON; provider doesn't touch frontend JSON. ✓
- Spec §5 unary path collapses canonical stream into a single response — `collect_canonical_to_response` does this. ✓
- Spec §5 cancellation: client disconnect → axum drops body stream → drops parser → drops reqwest stream. No special code needed; verified by next plan's fuzz test.
- Spec §6 deferral: Copilot provider key in registry is reserved (`UpstreamConfig::GithubCopilot` matched but not yet wired). ✓ Plan 05 adds it.
- Spec §7 #1 (tool-call delta passthrough): `ToolCallArgumentsDelta { json_fragment }` flows through encoder → SSE → upstream → parser → SSE → client. ✓
- Spec §7 #11 (header forwarding): only inbound `Authorization` is dropped (nothing forwards client auth upstream); upstream `Authorization: Bearer <api_key>` is added. Per-route forwarding lists are Phase 4.
- Performance: `Body::from_stream` keeps backpressure, no whole-response buffering on streaming path. Aligns with spec's "<5ms p99" target (verified in Plan 06 benches).
