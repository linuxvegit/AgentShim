# Plan 03 — `frontends` Crate (Anthropic + OpenAI)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the two MVP frontend protocols — Anthropic `/v1/messages` and OpenAI `/v1/chat/completions` — including request decoding, unary response encoding, and SSE stream encoding with full tool-call delta fidelity. Tested via golden SSE fixtures using a `MockProvider`.

**Architecture:** `FrontendProtocol` trait in `frontends/src/lib.rs`. Two adapter modules (`anthropic_messages/`, `openai_chat/`) implement the trait. Each adapter splits decode / unary-encode / stream-encode / mapping / wire-types into separate files. A test-only `MockProvider` lives in `protocol-tests` crate and replays canonical events from JSONL.

**Tech Stack:** `axum` (responses + SSE), `bytes`, `futures` (`Stream`, `StreamExt`), `serde`, `serde_json` (`RawValue`), `tokio` (only for tests / time-based heartbeats), and the pre-existing `agent-shim-core`.

---

## File Structure

`crates/frontends/`:
- Create: `crates/frontends/Cargo.toml`
- Create: `crates/frontends/src/lib.rs` — `FrontendProtocol` trait, registry, `FrontendError`
- Create: `crates/frontends/src/sse.rs` — small SSE writer helper
- Create: `crates/frontends/src/anthropic_messages/mod.rs`
- Create: `crates/frontends/src/anthropic_messages/wire.rs` — Anthropic-shaped serde types
- Create: `crates/frontends/src/anthropic_messages/decode.rs`
- Create: `crates/frontends/src/anthropic_messages/encode_unary.rs`
- Create: `crates/frontends/src/anthropic_messages/encode_stream.rs`
- Create: `crates/frontends/src/anthropic_messages/mapping.rs` — stop reason maps, role maps
- Create: `crates/frontends/src/openai_chat/mod.rs`
- Create: `crates/frontends/src/openai_chat/wire.rs`
- Create: `crates/frontends/src/openai_chat/decode.rs`
- Create: `crates/frontends/src/openai_chat/encode_unary.rs`
- Create: `crates/frontends/src/openai_chat/encode_stream.rs`
- Create: `crates/frontends/src/openai_chat/mapping.rs`

`crates/protocol-tests/`:
- Create: `crates/protocol-tests/Cargo.toml`
- Create: `crates/protocol-tests/src/lib.rs` — `MockProvider`, fixture helpers
- Create: `crates/protocol-tests/tests/anthropic_text_stream.rs`
- Create: `crates/protocol-tests/tests/anthropic_tool_call_stream.rs`
- Create: `crates/protocol-tests/tests/openai_text_stream.rs`
- Create: `crates/protocol-tests/tests/openai_tool_call_stream.rs`
- Create: `crates/protocol-tests/tests/anthropic_unary.rs`
- Create: `crates/protocol-tests/tests/openai_unary.rs`
- Create: `crates/protocol-tests/tests/cross_anthropic_to_openai.rs`
- Create: `crates/protocol-tests/tests/cross_openai_to_anthropic.rs`
- Create: `crates/protocol-tests/fixtures/canonical/text_stream.jsonl`
- Create: `crates/protocol-tests/fixtures/canonical/tool_call_stream.jsonl`
- Create: `crates/protocol-tests/fixtures/anthropic/text_stream.sse`
- Create: `crates/protocol-tests/fixtures/anthropic/tool_call_stream.sse`
- Create: `crates/protocol-tests/fixtures/openai/text_stream.sse`
- Create: `crates/protocol-tests/fixtures/openai/tool_call_stream.sse`

Workspace:
- Modify: root `Cargo.toml` `members`

---

## Task 1: Register `frontends` and `protocol-tests` crates

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Update workspace `members`**

```toml
members = [
  "crates/core",
  "crates/config",
  "crates/observability",
  "crates/gateway",
  "crates/frontends",
  "crates/protocol-tests",
]
```

- [ ] **Step 2: Add workspace deps**

Append to `[workspace.dependencies]`:

```toml
futures = "0.3"
futures-util = "0.3"
http-body-util = "0.1"
pretty_assertions = "1"
```

- [ ] **Step 3: Verify**

Run: `cargo metadata --no-deps > /dev/null`
Expected: succeeds (the crates don't exist yet, so this will fail until Task 2; treat as a checkpoint after Task 2 instead).

- [ ] **Step 4: Commit (after Task 2)**

(Combine into Task 2's commit.)

---

## Task 2: `frontends` crate skeleton + `FrontendProtocol` trait

**Files:**
- Create: `crates/frontends/Cargo.toml`
- Create: `crates/frontends/src/lib.rs`
- Create: `crates/frontends/src/sse.rs`

- [ ] **Step 1: `Cargo.toml`**

```toml
[package]
name = "agent-shim-frontends"
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
name = "agent_shim_frontends"
path = "src/lib.rs"

[dependencies]
agent-shim-core = { path = "../core" }
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
bytes.workspace = true
futures = { workspace = true }
futures-util = { workspace = true }
async-trait.workspace = true
http = "1"
axum = { workspace = true }
tokio = { workspace = true, features = ["time"] }

[dev-dependencies]
pretty_assertions.workspace = true
tokio = { workspace = true, features = ["macros", "rt"] }
```

- [ ] **Step 2: `lib.rs`**

```rust
#![forbid(unsafe_code)]

pub mod anthropic_messages;
pub mod openai_chat;
pub mod sse;

use thiserror::Error;

use agent_shim_core::{
    request::CanonicalRequest,
    response::CanonicalResponse,
    stream::CanonicalStream,
    target::FrontendKind,
};

#[derive(Debug, Error)]
pub enum FrontendError {
    #[error("invalid request body: {0}")]
    InvalidBody(String),
    #[error("unsupported feature: {0}")]
    Unsupported(String),
    #[error("encoding failure: {0}")]
    Encode(String),
    #[error("decoding failure: {0}")]
    Decode(String),
}

/// HTTP-shaped response a frontend builds. Body is either a finished `Bytes`
/// blob (unary) or a `Stream<Item = Bytes>` (SSE).
pub enum FrontendResponse {
    Unary { content_type: String, body: bytes::Bytes },
    Stream { content_type: String, stream: futures_util::stream::BoxStream<'static, Result<bytes::Bytes, FrontendError>> },
}

#[async_trait::async_trait]
pub trait FrontendProtocol: Send + Sync {
    fn kind(&self) -> FrontendKind;
    fn decode_request(&self, body: &[u8]) -> Result<CanonicalRequest, FrontendError>;
    fn encode_unary(&self, response: CanonicalResponse) -> Result<FrontendResponse, FrontendError>;
    /// Builder for SSE responses. The frontend owns the stream contract
    /// (event names, terminator, heartbeat shape). Returned stream yields
    /// raw `Bytes` already framed as SSE.
    fn encode_stream(&self, stream: CanonicalStream) -> FrontendResponse;
}
```

- [ ] **Step 3: `sse.rs`**

```rust
use bytes::{BufMut, Bytes, BytesMut};

/// Build a single SSE event with `event:` + `data:` + terminating blank line.
pub fn event(event_name: &str, data_json: &str) -> Bytes {
    let mut buf = BytesMut::with_capacity(event_name.len() + data_json.len() + 16);
    buf.put_slice(b"event: ");
    buf.put_slice(event_name.as_bytes());
    buf.put_slice(b"\ndata: ");
    buf.put_slice(data_json.as_bytes());
    buf.put_slice(b"\n\n");
    buf.freeze()
}

/// Build an `event:`-less SSE event (OpenAI-style — only `data:`).
pub fn data_only(data_json: &str) -> Bytes {
    let mut buf = BytesMut::with_capacity(data_json.len() + 8);
    buf.put_slice(b"data: ");
    buf.put_slice(data_json.as_bytes());
    buf.put_slice(b"\n\n");
    buf.freeze()
}

/// SSE comment line for keep-alive (legal per spec; ignored by clients).
pub fn comment(text: &str) -> Bytes {
    let mut buf = BytesMut::with_capacity(text.len() + 4);
    buf.put_slice(b": ");
    buf.put_slice(text.as_bytes());
    buf.put_slice(b"\n\n");
    buf.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_format() {
        let b = event("message_start", r#"{"a":1}"#);
        assert_eq!(b, "event: message_start\ndata: {\"a\":1}\n\n");
    }

    #[test]
    fn data_only_format() {
        let b = data_only(r#"{"x":2}"#);
        assert_eq!(b, "data: {\"x\":2}\n\n");
    }

    #[test]
    fn comment_format() {
        let b = comment("ping");
        assert_eq!(b, ": ping\n\n");
    }
}
```

- [ ] **Step 4: Stub the two adapter modules so it compiles**

`crates/frontends/src/anthropic_messages/mod.rs`:
```rust
pub mod decode;
pub mod encode_stream;
pub mod encode_unary;
pub mod mapping;
pub mod wire;
```

`crates/frontends/src/openai_chat/mod.rs`: identical 5-line listing.

Each of `decode.rs`, `encode_stream.rs`, `encode_unary.rs`, `mapping.rs`, `wire.rs` for both adapters: leave empty (just `// implemented in later tasks`).

- [ ] **Step 5: Build**

Run: `cargo build -p agent-shim-frontends`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/frontends
git commit -m "feat(frontends): add FrontendProtocol trait, SSE helpers, module skeleton"
```

---

## Task 3: Anthropic wire types

**Files:**
- Modify: `crates/frontends/src/anthropic_messages/wire.rs`

- [ ] **Step 1: Type definitions**

```rust
//! Serde-shaped types for the Anthropic `/v1/messages` request and SSE event
//! schema. Fields are spelled exactly as Anthropic documents them.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<InboundMessage>,
    #[serde(default)]
    pub system: Option<SystemField>,
    #[serde(default)]
    pub tools: Vec<InboundTool>,
    #[serde(default)]
    pub tool_choice: Option<InboundToolChoice>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SystemField {
    Text(String),
    Blocks(Vec<InboundContentBlock>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundMessage {
    pub role: String, // "user" | "assistant"
    pub content: InboundMessageContent,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum InboundMessageContent {
    Text(String),
    Blocks(Vec<InboundContentBlock>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboundContentBlock {
    Text { text: String, #[serde(default)] cache_control: Option<Value> },
    Image { source: Value, #[serde(default)] cache_control: Option<Value> },
    ToolUse { id: String, name: String, input: Value },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        is_error: Option<bool>,
        content: Value,
    },
    Thinking { thinking: String, #[serde(default)] signature: Option<String> },
    RedactedThinking { data: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(default)]
    pub cache_control: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboundToolChoice {
    Auto,
    Any,
    Tool { name: String },
}

// ---------- outbound (encoding) ----------

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum OutboundEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: OutboundMessage },
    #[serde(rename = "content_block_start")]
    ContentBlockStart { index: u32, content_block: OutboundContentBlockStart },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: u32, delta: OutboundDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: u32 },
    #[serde(rename = "message_delta")]
    MessageDelta { delta: OutboundMessageDelta, usage: OutboundUsage },
    #[serde(rename = "message_stop")]
    MessageStop {},
    #[serde(rename = "ping")]
    Ping {},
    #[serde(rename = "error")]
    Error { error: OutboundErrorBody },
}

#[derive(Debug, Clone, Serialize)]
pub struct OutboundMessage {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str, // always "message"
    pub role: &'static str, // "assistant"
    pub model: String,
    pub content: Vec<Value>, // empty at start; populated on unary
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: OutboundUsage,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutboundContentBlockStart {
    Text { text: String },
    ToolUse { id: String, name: String, input: Value },
    Thinking { thinking: String },
    RedactedThinking { data: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum OutboundDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OutboundMessageDelta {
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OutboundUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutboundErrorBody {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub message: String,
}
```

- [ ] **Step 2: Build**

Run: `cargo build -p agent-shim-frontends`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/frontends/src/anthropic_messages/wire.rs
git commit -m "feat(frontends/anthropic): wire-shape serde types for messages API"
```

---

## Task 4: Anthropic mapping helpers (stop reason, role)

**Files:**
- Modify: `crates/frontends/src/anthropic_messages/mapping.rs`

- [ ] **Step 1: Implementation + tests**

```rust
use agent_shim_core::message::MessageRole;
use agent_shim_core::usage::StopReason;

pub fn role_to_canonical(role: &str) -> Option<MessageRole> {
    match role {
        "user" => Some(MessageRole::User),
        "assistant" => Some(MessageRole::Assistant),
        _ => None,
    }
}

pub fn stop_reason_from_canonical(s: &StopReason) -> &'static str {
    match s {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence => "stop_sequence",
        StopReason::ToolUse => "tool_use",
        StopReason::ContentFilter => "end_turn",   // Anthropic has no exact equivalent
        StopReason::Refusal => "end_turn",
        StopReason::Error => "end_turn",
        StopReason::Unknown { .. } => "end_turn",
    }
}

pub fn stop_reason_to_canonical(s: &str) -> StopReason {
    match s {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        other => StopReason::Unknown { value: other.into() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_round_trip() {
        assert_eq!(role_to_canonical("user"), Some(MessageRole::User));
        assert_eq!(role_to_canonical("assistant"), Some(MessageRole::Assistant));
        assert_eq!(role_to_canonical("tool"), None);
    }

    #[test]
    fn stop_reason_round_trip() {
        for s in ["end_turn", "max_tokens", "stop_sequence", "tool_use"] {
            let canonical = stop_reason_to_canonical(s);
            assert_eq!(stop_reason_from_canonical(&canonical), s);
        }
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p agent-shim-frontends mapping`
Expected: 2 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/frontends/src/anthropic_messages/mapping.rs
git commit -m "feat(frontends/anthropic): role and stop-reason mapping with round-trip test"
```

---

## Task 5: Anthropic decoder

**Files:**
- Modify: `crates/frontends/src/anthropic_messages/decode.rs`

- [ ] **Step 1: Implementation**

```rust
use agent_shim_core::{
    content::{ContentBlock, ImageBlock, ReasoningBlock, RedactedReasoningBlock, TextBlock},
    extensions::ExtensionMap,
    ids::{RequestId, ToolCallId},
    media::BinarySource,
    message::{Message, SystemInstruction, SystemSource},
    request::{CanonicalRequest, GenerationOptions},
    target::{FrontendInfo, FrontendKind, FrontendModel},
    tool::{ToolCallArguments, ToolCallBlock, ToolChoice, ToolDefinition, ToolResultBlock},
};
use serde_json::json;

use super::mapping::role_to_canonical;
use super::wire::{
    InboundContentBlock, InboundMessageContent, InboundTool, InboundToolChoice, MessagesRequest,
    SystemField,
};
use crate::FrontendError;

pub fn decode(body: &[u8]) -> Result<CanonicalRequest, FrontendError> {
    let req: MessagesRequest = serde_json::from_slice(body)
        .map_err(|e| FrontendError::InvalidBody(e.to_string()))?;

    let system = match req.system {
        None => Vec::new(),
        Some(SystemField::Text(t)) => vec![SystemInstruction {
            source: SystemSource::AnthropicSystem,
            content: vec![ContentBlock::text(t)],
        }],
        Some(SystemField::Blocks(blocks)) => vec![SystemInstruction {
            source: SystemSource::AnthropicSystem,
            content: blocks.into_iter().map(content_block).collect::<Result<_, _>>()?,
        }],
    };

    let messages = req
        .messages
        .into_iter()
        .map(|m| {
            let role = role_to_canonical(&m.role)
                .ok_or_else(|| FrontendError::InvalidBody(format!("bad role: {}", m.role)))?;
            let content = match m.content {
                InboundMessageContent::Text(t) => vec![ContentBlock::text(t)],
                InboundMessageContent::Blocks(b) => b.into_iter().map(content_block).collect::<Result<_, _>>()?,
            };
            Ok::<_, FrontendError>(Message { role, content, name: None, extensions: Default::default() })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let tools = req
        .tools
        .into_iter()
        .map(tool_def)
        .collect::<Result<Vec<_>, _>>()?;

    let tool_choice = match req.tool_choice {
        None => ToolChoice::Auto,
        Some(InboundToolChoice::Auto) => ToolChoice::Auto,
        Some(InboundToolChoice::Any) => ToolChoice::Required,
        Some(InboundToolChoice::Tool { name }) => ToolChoice::Specific { name },
    };

    Ok(CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::AnthropicMessages,
            api_path: "/v1/messages".into(),
        },
        model: FrontendModel::from(req.model),
        system,
        messages,
        tools,
        tool_choice,
        generation: GenerationOptions {
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            top_p: req.top_p,
            top_k: req.top_k,
            stop_sequences: req.stop_sequences,
            ..Default::default()
        },
        response_format: None,
        stream: req.stream,
        metadata: Default::default(),
        extensions: Default::default(),
    })
}

fn tool_def(t: InboundTool) -> Result<ToolDefinition, FrontendError> {
    let mut ext = ExtensionMap::default();
    if let Some(cc) = t.cache_control {
        ext.insert("anthropic.cache_control", cc);
    }
    Ok(ToolDefinition {
        name: t.name,
        description: t.description,
        input_schema: t.input_schema,
        extensions: ext,
    })
}

fn content_block(b: InboundContentBlock) -> Result<ContentBlock, FrontendError> {
    Ok(match b {
        InboundContentBlock::Text { text, cache_control } => {
            let mut ext = ExtensionMap::default();
            if let Some(cc) = cache_control { ext.insert("anthropic.cache_control", cc); }
            ContentBlock::Text(TextBlock { text, extensions: ext })
        }
        InboundContentBlock::Image { source, cache_control } => {
            let mut ext = ExtensionMap::default();
            if let Some(cc) = cache_control { ext.insert("anthropic.cache_control", cc); }
            ContentBlock::Image(ImageBlock { source: parse_image_source(source)?, extensions: ext })
        }
        InboundContentBlock::ToolUse { id, name, input } => {
            ContentBlock::ToolCall(ToolCallBlock {
                id: ToolCallId::from_provider(id),
                name,
                arguments: ToolCallArguments::Complete(input),
                extensions: Default::default(),
            })
        }
        InboundContentBlock::ToolResult { tool_use_id, is_error, content } => {
            ContentBlock::ToolResult(ToolResultBlock {
                tool_call_id: ToolCallId::from_provider(tool_use_id),
                content,
                is_error: is_error.unwrap_or(false),
                extensions: Default::default(),
            })
        }
        InboundContentBlock::Thinking { thinking, .. } => ContentBlock::Reasoning(ReasoningBlock {
            text: thinking,
            extensions: Default::default(),
        }),
        InboundContentBlock::RedactedThinking { data } => {
            ContentBlock::RedactedReasoning(RedactedReasoningBlock { data, extensions: Default::default() })
        }
    })
}

fn parse_image_source(v: serde_json::Value) -> Result<BinarySource, FrontendError> {
    // Anthropic shapes:  { "type":"base64", "media_type":"image/png", "data":"..." }
    //                or  { "type":"url", "url":"..." }
    let obj = v.as_object().ok_or_else(|| FrontendError::InvalidBody("image source not an object".into()))?;
    match obj.get("type").and_then(|x| x.as_str()) {
        Some("base64") => {
            let mime = obj.get("media_type").and_then(|x| x.as_str()).unwrap_or("application/octet-stream").to_string();
            let data = obj.get("data").and_then(|x| x.as_str()).unwrap_or_default().to_string();
            Ok(BinarySource::Base64 { mime, data })
        }
        Some("url") => {
            let url = obj.get("url").and_then(|x| x.as_str()).unwrap_or_default().to_string();
            Ok(BinarySource::Url { url })
        }
        _ => {
            let _ = json!(null);
            Err(FrontendError::InvalidBody("unknown image source type".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_minimal_text_request() {
        let body = br#"{"model":"claude-3-5-sonnet","max_tokens":100,"messages":[{"role":"user","content":"Hi"}]}"#;
        let req = decode(body).unwrap();
        assert_eq!(req.model.0, "claude-3-5-sonnet");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.generation.max_tokens, Some(100));
    }

    #[test]
    fn decode_blocks_with_tool_use_and_tool_result() {
        let body = br#"{
          "model":"claude-3-5-sonnet",
          "messages":[
            {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"search","input":{"q":"x"}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":[{"type":"text","text":"found"}]}]}
          ]
        }"#;
        let req = decode(body).unwrap();
        assert_eq!(req.messages.len(), 2);
    }

    #[test]
    fn decode_system_string_becomes_one_instruction() {
        let body = br#"{"model":"x","messages":[],"system":"be helpful"}"#;
        let req = decode(body).unwrap();
        assert_eq!(req.system.len(), 1);
    }

    #[test]
    fn rejects_bad_role() {
        let body = br#"{"model":"x","messages":[{"role":"bot","content":"hi"}]}"#;
        assert!(decode(body).is_err());
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p agent-shim-frontends decode`
Expected: 4 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/frontends/src/anthropic_messages/decode.rs
git commit -m "feat(frontends/anthropic): decode_request with cache_control extension preservation"
```

---

## Task 6: Anthropic SSE encoder (streaming)

**Files:**
- Modify: `crates/frontends/src/anthropic_messages/encode_stream.rs`

- [ ] **Step 1: Implementation**

```rust
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::stream::{self, Stream, StreamExt};
use futures_util::stream::BoxStream;

use agent_shim_core::stream::{CanonicalStream, ContentBlockKind, StreamEvent};

use super::mapping::stop_reason_from_canonical;
use super::wire::*;
use crate::sse;
use crate::FrontendError;

pub fn encode(
    canonical: CanonicalStream,
    keepalive: Option<Duration>,
) -> BoxStream<'static, Result<Bytes, FrontendError>> {
    let state = Arc::new(parking_lot::Mutex::new(EncoderState::default()));
    let s = canonical.flat_map(move |evt| {
        let chunks = handle_event(state.clone(), evt);
        stream::iter(chunks)
    });

    if let Some(period) = keepalive {
        keepalive_merged(s.boxed(), period)
    } else {
        s.boxed()
    }
}

#[derive(Default)]
struct EncoderState {
    response_id: Option<String>,
    model: String,
    last_stop_reason: Option<String>,
    last_stop_sequence: Option<String>,
    final_usage: Option<OutboundUsage>,
}

fn handle_event(
    state: Arc<parking_lot::Mutex<EncoderState>>,
    evt: Result<StreamEvent, agent_shim_core::error::StreamError>,
) -> Vec<Result<Bytes, FrontendError>> {
    let evt = match evt {
        Ok(e) => e,
        Err(e) => {
            return vec![Ok(sse::event(
                "error",
                &serde_json::to_string(&OutboundEvent::Error {
                    error: OutboundErrorBody { kind: "api_error", message: e.to_string() },
                })
                .unwrap_or_default(),
            ))]
        }
    };

    let mut out = Vec::new();
    let mut s = state.lock();
    match evt {
        StreamEvent::ResponseStart { id, model, .. } => {
            s.response_id = Some(id.0.clone());
            s.model = model.clone();
        }
        StreamEvent::MessageStart { .. } => {
            let id = s.response_id.clone().unwrap_or_else(|| "msg_0".into());
            let msg = OutboundMessage {
                id,
                kind: "message",
                role: "assistant",
                model: s.model.clone(),
                content: vec![],
                stop_reason: None,
                stop_sequence: None,
                usage: OutboundUsage::default(),
            };
            push(&mut out, "message_start", OutboundEvent::MessageStart { message: msg });
        }
        StreamEvent::ContentBlockStart { index, kind } => {
            let block = match kind {
                ContentBlockKind::Text => OutboundContentBlockStart::Text { text: String::new() },
                ContentBlockKind::ToolCall => OutboundContentBlockStart::ToolUse {
                    id: String::new(),
                    name: String::new(),
                    input: serde_json::json!({}),
                },
                ContentBlockKind::Reasoning => OutboundContentBlockStart::Thinking { thinking: String::new() },
                ContentBlockKind::RedactedReasoning => OutboundContentBlockStart::RedactedThinking { data: String::new() },
                ContentBlockKind::Image | ContentBlockKind::Audio => {
                    OutboundContentBlockStart::Text { text: String::new() }
                }
            };
            push(&mut out, "content_block_start", OutboundEvent::ContentBlockStart { index, content_block: block });
        }
        StreamEvent::TextDelta { index, text } => {
            push(&mut out, "content_block_delta", OutboundEvent::ContentBlockDelta {
                index,
                delta: OutboundDelta::TextDelta { text },
            });
        }
        StreamEvent::ReasoningDelta { index, text } => {
            push(&mut out, "content_block_delta", OutboundEvent::ContentBlockDelta {
                index,
                delta: OutboundDelta::ThinkingDelta { thinking: text },
            });
        }
        StreamEvent::ToolCallStart { index, id, name } => {
            push(&mut out, "content_block_start", OutboundEvent::ContentBlockStart {
                index,
                content_block: OutboundContentBlockStart::ToolUse {
                    id: id.0,
                    name,
                    input: serde_json::json!({}),
                },
            });
        }
        StreamEvent::ToolCallArgumentsDelta { index, json_fragment } => {
            push(&mut out, "content_block_delta", OutboundEvent::ContentBlockDelta {
                index,
                delta: OutboundDelta::InputJsonDelta { partial_json: json_fragment },
            });
        }
        StreamEvent::ToolCallStop { index } | StreamEvent::ContentBlockStop { index } => {
            push(&mut out, "content_block_stop", OutboundEvent::ContentBlockStop { index });
        }
        StreamEvent::UsageDelta { usage } => {
            s.final_usage = Some(OutboundUsage {
                input_tokens: usage.input_tokens.unwrap_or(0),
                output_tokens: usage.output_tokens.unwrap_or(0),
                cache_creation_input_tokens: usage.cache_creation_input_tokens,
                cache_read_input_tokens: usage.cache_read_input_tokens,
            });
        }
        StreamEvent::MessageStop { stop_reason, stop_sequence } => {
            s.last_stop_reason = Some(stop_reason_from_canonical(&stop_reason).to_string());
            s.last_stop_sequence = stop_sequence;
        }
        StreamEvent::ResponseStop { usage } => {
            if let Some(u) = usage {
                s.final_usage = Some(OutboundUsage {
                    input_tokens: u.input_tokens.unwrap_or(0),
                    output_tokens: u.output_tokens.unwrap_or(0),
                    cache_creation_input_tokens: u.cache_creation_input_tokens,
                    cache_read_input_tokens: u.cache_read_input_tokens,
                });
            }
            push(&mut out, "message_delta", OutboundEvent::MessageDelta {
                delta: OutboundMessageDelta {
                    stop_reason: s.last_stop_reason.clone(),
                    stop_sequence: s.last_stop_sequence.clone(),
                },
                usage: s.final_usage.clone().unwrap_or_default(),
            });
            push(&mut out, "message_stop", OutboundEvent::MessageStop {});
        }
        StreamEvent::Error { message } => {
            push(&mut out, "error", OutboundEvent::Error {
                error: OutboundErrorBody { kind: "api_error", message },
            });
        }
        StreamEvent::RawProviderEvent(_) => {}
    }
    out
}

fn push(out: &mut Vec<Result<Bytes, FrontendError>>, name: &str, ev: OutboundEvent) {
    match serde_json::to_string(&ev) {
        Ok(s) => out.push(Ok(sse::event(name, &s))),
        Err(e) => out.push(Err(FrontendError::Encode(e.to_string()))),
    }
}

fn keepalive_merged(
    base: BoxStream<'static, Result<Bytes, FrontendError>>,
    period: Duration,
) -> BoxStream<'static, Result<Bytes, FrontendError>> {
    use tokio_stream::wrappers::IntervalStream;
    let interval = IntervalStream::new(tokio::time::interval(period))
        .map(|_| Ok(sse::event("ping", "{}")));
    futures::stream::select(base, interval).boxed()
}
```

- [ ] **Step 2: Add deps**

In `crates/frontends/Cargo.toml`:

```toml
parking_lot = "0.12"
tokio-stream = { version = "0.1", features = ["time"] }
```

- [ ] **Step 3: Build**

Run: `cargo build -p agent-shim-frontends`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/frontends
git commit -m "feat(frontends/anthropic): SSE stream encoder with tool-call deltas and ping keepalive"
```

---

## Task 7: Anthropic unary encoder + module impl

**Files:**
- Modify: `crates/frontends/src/anthropic_messages/encode_unary.rs`
- Modify: `crates/frontends/src/anthropic_messages/mod.rs`

- [ ] **Step 1: `encode_unary.rs`**

```rust
use bytes::Bytes;
use serde_json::{json, Value};

use agent_shim_core::content::ContentBlock;
use agent_shim_core::response::CanonicalResponse;

use super::mapping::stop_reason_from_canonical;
use crate::FrontendError;

pub fn encode(resp: CanonicalResponse) -> Result<Bytes, FrontendError> {
    let content: Vec<Value> = resp
        .content
        .into_iter()
        .map(content_block_to_wire)
        .collect();
    let body = json!({
        "id": resp.id.0,
        "type": "message",
        "role": "assistant",
        "model": resp.model,
        "content": content,
        "stop_reason": stop_reason_from_canonical(&resp.stop_reason),
        "stop_sequence": resp.stop_sequence,
        "usage": {
            "input_tokens": resp.usage.as_ref().and_then(|u| u.input_tokens).unwrap_or(0),
            "output_tokens": resp.usage.as_ref().and_then(|u| u.output_tokens).unwrap_or(0),
        }
    });
    serde_json::to_vec(&body)
        .map(Bytes::from)
        .map_err(|e| FrontendError::Encode(e.to_string()))
}

fn content_block_to_wire(b: ContentBlock) -> Value {
    match b {
        ContentBlock::Text(t) => json!({ "type": "text", "text": t.text }),
        ContentBlock::ToolCall(tc) => {
            let input = match tc.arguments {
                agent_shim_core::tool::ToolCallArguments::Complete(v) => v,
                agent_shim_core::tool::ToolCallArguments::Streaming(r) => {
                    serde_json::from_str(r.get()).unwrap_or(Value::Null)
                }
            };
            json!({ "type": "tool_use", "id": tc.id.0, "name": tc.name, "input": input })
        }
        ContentBlock::Reasoning(r) => json!({ "type": "thinking", "thinking": r.text }),
        ContentBlock::RedactedReasoning(r) => json!({ "type": "redacted_thinking", "data": r.data }),
        // v0.1 doesn't emit these on the response side
        ContentBlock::Image(_) | ContentBlock::Audio(_) | ContentBlock::File(_) | ContentBlock::ToolResult(_) | ContentBlock::Unsupported(_) => Value::Null,
    }
}
```

- [ ] **Step 2: `mod.rs` — implement `FrontendProtocol`**

```rust
pub mod decode;
pub mod encode_stream;
pub mod encode_unary;
pub mod mapping;
pub mod wire;

use std::time::Duration;

use agent_shim_core::request::CanonicalRequest;
use agent_shim_core::response::CanonicalResponse;
use agent_shim_core::stream::CanonicalStream;
use agent_shim_core::target::FrontendKind;

use crate::{FrontendError, FrontendProtocol, FrontendResponse};

pub struct AnthropicMessages {
    pub keepalive: Option<Duration>,
}

impl Default for AnthropicMessages {
    fn default() -> Self { Self { keepalive: Some(Duration::from_secs(15)) } }
}

#[async_trait::async_trait]
impl FrontendProtocol for AnthropicMessages {
    fn kind(&self) -> FrontendKind { FrontendKind::AnthropicMessages }

    fn decode_request(&self, body: &[u8]) -> Result<CanonicalRequest, FrontendError> {
        decode::decode(body)
    }

    fn encode_unary(&self, response: CanonicalResponse) -> Result<FrontendResponse, FrontendError> {
        let body = encode_unary::encode(response)?;
        Ok(FrontendResponse::Unary { content_type: "application/json".into(), body })
    }

    fn encode_stream(&self, stream: CanonicalStream) -> FrontendResponse {
        let s = encode_stream::encode(stream, self.keepalive);
        FrontendResponse::Stream { content_type: "text/event-stream".into(), stream: s }
    }
}
```

- [ ] **Step 3: Build, commit**

Run: `cargo build -p agent-shim-frontends`
Expected: clean.

```bash
git add crates/frontends/src/anthropic_messages
git commit -m "feat(frontends/anthropic): unary encoder and FrontendProtocol impl"
```

---

## Task 8: OpenAI wire types

**Files:**
- Modify: `crates/frontends/src/openai_chat/wire.rs`

- [ ] **Step 1: Implementation**

```rust
//! Serde shapes for OpenAI `/v1/chat/completions` request and SSE chunk schema.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionsRequest {
    pub model: String,
    pub messages: Vec<InboundMessage>,
    #[serde(default)]
    pub tools: Vec<InboundTool>,
    #[serde(default)]
    pub tool_choice: Option<InboundToolChoice>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub max_completion_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub presence_penalty: Option<f32>,
    #[serde(default)]
    pub frequency_penalty: Option<f32>,
    #[serde(default)]
    pub stop: Option<StopField>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub response_format: Option<Value>,
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub user: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StopField {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundMessage {
    pub role: String, // "system" | "developer" | "user" | "assistant" | "tool"
    #[serde(default)]
    pub content: Option<InboundMessageContent>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<InboundToolCall>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum InboundMessageContent {
    Text(String),
    Parts(Vec<InboundContentPart>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboundContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrlObj },
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImageUrlObj { pub url: String }

#[derive(Debug, Clone, Deserialize)]
pub struct InboundToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String, // "function"
    pub function: InboundToolCallFn,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundToolCallFn {
    pub name: String,
    pub arguments: String, // raw JSON string
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundTool {
    #[serde(rename = "type")]
    pub kind: String, // "function"
    pub function: InboundToolDef,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundToolDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub parameters: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum InboundToolChoice {
    Mode(String), // "auto" | "none" | "required"
    Specific(SpecificToolChoice),
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpecificToolChoice {
    #[serde(rename = "type")]
    pub kind: String, // "function"
    pub function: SpecificToolChoiceFn,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpecificToolChoiceFn { pub name: String }

// ---------- outbound ----------

#[derive(Debug, Clone, Serialize)]
pub struct ChunkOut {
    pub id: String,
    pub object: &'static str, // "chat.completion.chunk"
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChoiceOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageOut>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChoiceOut {
    pub index: u32,
    pub delta: DeltaOut,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DeltaOut {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallDeltaOut>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallDeltaOut {
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    pub function: ToolCallDeltaFn,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ToolCallDeltaFn {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageOut {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

// Unary completion shape
#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionOut {
    pub id: String,
    pub object: &'static str, // "chat.completion"
    pub created: u64,
    pub model: String,
    pub choices: Vec<UnaryChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageOut>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnaryChoice {
    pub index: u32,
    pub message: UnaryMessage,
    pub finish_reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnaryMessage {
    pub role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<UnaryToolCall>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnaryToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str, // "function"
    pub function: UnaryFn,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnaryFn {
    pub name: String,
    pub arguments: String,
}
```

- [ ] **Step 2: Build, commit**

Run: `cargo build -p agent-shim-frontends`

```bash
git add crates/frontends/src/openai_chat/wire.rs
git commit -m "feat(frontends/openai): wire-shape serde types for chat.completions API"
```

---

## Task 9: OpenAI mapping helpers

**Files:**
- Modify: `crates/frontends/src/openai_chat/mapping.rs`

- [ ] **Step 1: Implementation + tests**

```rust
use agent_shim_core::message::{MessageRole, SystemSource};
use agent_shim_core::usage::StopReason;

pub fn role_to_canonical(role: &str) -> Option<RoleClass> {
    match role {
        "user" => Some(RoleClass::Message(MessageRole::User)),
        "assistant" => Some(RoleClass::Message(MessageRole::Assistant)),
        "tool" => Some(RoleClass::Message(MessageRole::Tool)),
        "system" => Some(RoleClass::System(SystemSource::OpenAiSystem)),
        "developer" => Some(RoleClass::System(SystemSource::OpenAiDeveloper)),
        _ => None,
    }
}

#[derive(Debug, PartialEq)]
pub enum RoleClass {
    Message(MessageRole),
    System(SystemSource),
}

pub fn finish_reason_from_canonical(s: &StopReason) -> &'static str {
    match s {
        StopReason::EndTurn => "stop",
        StopReason::MaxTokens => "length",
        StopReason::StopSequence => "stop",
        StopReason::ToolUse => "tool_calls",
        StopReason::ContentFilter => "content_filter",
        StopReason::Refusal => "stop",
        StopReason::Error => "stop",
        StopReason::Unknown { .. } => "stop",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_classification() {
        assert_eq!(role_to_canonical("user"), Some(RoleClass::Message(MessageRole::User)));
        assert_eq!(role_to_canonical("system"), Some(RoleClass::System(SystemSource::OpenAiSystem)));
        assert_eq!(role_to_canonical("developer"), Some(RoleClass::System(SystemSource::OpenAiDeveloper)));
        assert_eq!(role_to_canonical("foo"), None);
    }

    #[test]
    fn finish_reason_mapping() {
        assert_eq!(finish_reason_from_canonical(&StopReason::EndTurn), "stop");
        assert_eq!(finish_reason_from_canonical(&StopReason::ToolUse), "tool_calls");
        assert_eq!(finish_reason_from_canonical(&StopReason::MaxTokens), "length");
    }
}
```

- [ ] **Step 2: Run tests, commit**

Run: `cargo test -p agent-shim-frontends openai_chat::mapping`
Expected: 2 passed.

```bash
git add crates/frontends/src/openai_chat/mapping.rs
git commit -m "feat(frontends/openai): role and finish-reason mapping"
```

---

## Task 10: OpenAI decoder

**Files:**
- Modify: `crates/frontends/src/openai_chat/decode.rs`

- [ ] **Step 1: Implementation**

```rust
use agent_shim_core::{
    content::{ContentBlock, ImageBlock, TextBlock},
    extensions::ExtensionMap,
    ids::{RequestId, ToolCallId},
    media::BinarySource,
    message::{Message, SystemInstruction},
    request::{CanonicalRequest, GenerationOptions, ResponseFormat},
    target::{FrontendInfo, FrontendKind, FrontendModel},
    tool::{ToolCallArguments, ToolCallBlock, ToolChoice, ToolDefinition, ToolResultBlock},
};

use super::mapping::{role_to_canonical, RoleClass};
use super::wire::{
    ChatCompletionsRequest, InboundContentPart, InboundMessage, InboundMessageContent,
    InboundToolChoice, StopField,
};
use crate::FrontendError;

pub fn decode(body: &[u8]) -> Result<CanonicalRequest, FrontendError> {
    let req: ChatCompletionsRequest = serde_json::from_slice(body)
        .map_err(|e| FrontendError::InvalidBody(e.to_string()))?;

    let mut system: Vec<SystemInstruction> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();

    for m in req.messages {
        match role_to_canonical(&m.role).ok_or_else(|| FrontendError::InvalidBody(format!("bad role: {}", m.role)))? {
            RoleClass::System(source) => {
                let content = msg_content_to_blocks(m.content)?;
                system.push(SystemInstruction { source, content });
            }
            RoleClass::Message(role) => {
                messages.push(decode_message(role, m)?);
            }
        }
    }

    let tools = req.tools.into_iter().map(|t| ToolDefinition {
        name: t.function.name,
        description: t.function.description,
        input_schema: t.function.parameters,
        extensions: ExtensionMap::default(),
    }).collect();

    let tool_choice = match req.tool_choice {
        None => ToolChoice::Auto,
        Some(InboundToolChoice::Mode(m)) => match m.as_str() {
            "auto" => ToolChoice::Auto,
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            other => return Err(FrontendError::InvalidBody(format!("bad tool_choice: {other}"))),
        },
        Some(InboundToolChoice::Specific(s)) => ToolChoice::Specific { name: s.function.name },
    };

    let response_format = req.response_format.and_then(|v| {
        let kind = v.get("type").and_then(|x| x.as_str())?;
        Some(match kind {
            "json_object" => ResponseFormat::JsonObject,
            "json_schema" => {
                let js = v.get("json_schema")?;
                ResponseFormat::JsonSchema {
                    name: js.get("name").and_then(|x| x.as_str()).unwrap_or("schema").into(),
                    schema: js.get("schema").cloned().unwrap_or(serde_json::json!({})),
                    strict: js.get("strict").and_then(|x| x.as_bool()).unwrap_or(false),
                }
            }
            _ => ResponseFormat::Text,
        })
    });

    let stop_sequences = match req.stop {
        None => Vec::new(),
        Some(StopField::One(s)) => vec![s],
        Some(StopField::Many(v)) => v,
    };

    Ok(CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::OpenAiChat,
            api_path: "/v1/chat/completions".into(),
        },
        model: FrontendModel::from(req.model),
        system,
        messages,
        tools,
        tool_choice,
        generation: GenerationOptions {
            max_tokens: req.max_completion_tokens.or(req.max_tokens),
            temperature: req.temperature,
            top_p: req.top_p,
            presence_penalty: req.presence_penalty,
            frequency_penalty: req.frequency_penalty,
            stop_sequences,
            seed: req.seed,
            ..Default::default()
        },
        response_format,
        stream: req.stream,
        metadata: Default::default(),
        extensions: Default::default(),
    })
}

fn decode_message(role: agent_shim_core::message::MessageRole, m: InboundMessage) -> Result<Message, FrontendError> {
    let mut content = msg_content_to_blocks(m.content)?;
    for tc in m.tool_calls {
        let raw = tc.function.arguments;
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw));
        content.push(ContentBlock::ToolCall(ToolCallBlock {
            id: ToolCallId::from_provider(tc.id),
            name: tc.function.name,
            arguments: ToolCallArguments::Complete(value),
            extensions: ExtensionMap::default(),
        }));
    }
    if let Some(tc_id) = m.tool_call_id {
        // role is "tool" — entire content becomes a ToolResult.
        let result_content = content.drain(..).fold(serde_json::Value::Array(vec![]), |mut acc, b| {
            if let serde_json::Value::Array(ref mut a) = acc {
                a.push(serde_json::to_value(b).unwrap_or(serde_json::Value::Null));
            }
            acc
        });
        return Ok(Message {
            role,
            content: vec![ContentBlock::ToolResult(ToolResultBlock {
                tool_call_id: ToolCallId::from_provider(tc_id),
                content: result_content,
                is_error: false,
                extensions: ExtensionMap::default(),
            })],
            name: m.name,
            extensions: ExtensionMap::default(),
        });
    }
    Ok(Message { role, content, name: m.name, extensions: ExtensionMap::default() })
}

fn msg_content_to_blocks(c: Option<InboundMessageContent>) -> Result<Vec<ContentBlock>, FrontendError> {
    Ok(match c {
        None => Vec::new(),
        Some(InboundMessageContent::Text(t)) => vec![ContentBlock::Text(TextBlock { text: t, extensions: Default::default() })],
        Some(InboundMessageContent::Parts(parts)) => parts.into_iter().map(|p| match p {
            InboundContentPart::Text { text } => ContentBlock::Text(TextBlock { text, extensions: Default::default() }),
            InboundContentPart::ImageUrl { image_url } => ContentBlock::Image(ImageBlock {
                source: BinarySource::Url { url: image_url.url },
                extensions: Default::default(),
            }),
        }).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_minimal() {
        let body = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#;
        let req = decode(body).unwrap();
        assert_eq!(req.model.0, "gpt-4o");
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn decode_with_system_and_developer() {
        let body = br#"{"model":"x","messages":[
            {"role":"system","content":"sys"},
            {"role":"developer","content":"dev"},
            {"role":"user","content":"hi"}
        ]}"#;
        let req = decode(body).unwrap();
        assert_eq!(req.system.len(), 2);
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn decode_tool_choice_modes() {
        for (json_choice, want) in [("\"auto\"", ToolChoice::Auto), ("\"none\"", ToolChoice::None), ("\"required\"", ToolChoice::Required)] {
            let body = format!(r#"{{"model":"x","messages":[],"tool_choice":{}}}"#, json_choice);
            let req = decode(body.as_bytes()).unwrap();
            assert_eq!(req.tool_choice, want);
        }
    }
}
```

- [ ] **Step 2: Run tests, commit**

Run: `cargo test -p agent-shim-frontends openai_chat::decode`
Expected: 3 passed.

```bash
git add crates/frontends/src/openai_chat/decode.rs
git commit -m "feat(frontends/openai): decode_request including system/developer split and tool_choice"
```

---

## Task 11: OpenAI SSE encoder + unary encoder + impl

**Files:**
- Modify: `crates/frontends/src/openai_chat/encode_stream.rs`
- Modify: `crates/frontends/src/openai_chat/encode_unary.rs`
- Modify: `crates/frontends/src/openai_chat/mod.rs`

- [ ] **Step 1: `encode_stream.rs`**

```rust
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::stream::{self, Stream, StreamExt};
use futures_util::stream::BoxStream;
use parking_lot::Mutex;

use agent_shim_core::stream::{CanonicalStream, StreamEvent};

use super::mapping::finish_reason_from_canonical;
use super::wire::*;
use crate::sse;
use crate::FrontendError;

pub fn encode(
    canonical: CanonicalStream,
    keepalive: Option<Duration>,
) -> BoxStream<'static, Result<Bytes, FrontendError>> {
    let state = Arc::new(Mutex::new(EncoderState::default()));
    let s = canonical
        .flat_map({
            let state = state.clone();
            move |evt| stream::iter(handle(state.clone(), evt))
        })
        .chain(stream::once(async { Ok(sse::data_only("[DONE]")) }));

    if let Some(period) = keepalive {
        keepalive_merged(s.boxed(), period)
    } else {
        s.boxed()
    }
}

#[derive(Default)]
struct EncoderState {
    response_id: String,
    model: String,
    created: u64,
    role_emitted: bool,
}

fn handle(
    state: Arc<Mutex<EncoderState>>,
    evt: Result<StreamEvent, agent_shim_core::error::StreamError>,
) -> Vec<Result<Bytes, FrontendError>> {
    let evt = match evt {
        Ok(e) => e,
        Err(e) => return vec![Ok(emit_error(&state.lock(), e.to_string()))],
    };
    let mut s = state.lock();
    match evt {
        StreamEvent::ResponseStart { id, model, .. } => {
            s.response_id = id.0;
            s.model = model;
            s.created = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            vec![]
        }
        StreamEvent::MessageStart { .. } => {
            // Emit a chunk that carries `role: "assistant"` once.
            let chunk = ChunkOut {
                id: s.response_id.clone(),
                object: "chat.completion.chunk",
                created: s.created,
                model: s.model.clone(),
                choices: vec![ChoiceOut {
                    index: 0,
                    delta: DeltaOut { role: Some("assistant"), ..Default::default() },
                    finish_reason: None,
                }],
                usage: None,
            };
            s.role_emitted = true;
            vec![push(chunk)]
        }
        StreamEvent::TextDelta { text, .. } => {
            let chunk = ChunkOut {
                id: s.response_id.clone(),
                object: "chat.completion.chunk",
                created: s.created,
                model: s.model.clone(),
                choices: vec![ChoiceOut {
                    index: 0,
                    delta: DeltaOut { content: Some(text), ..Default::default() },
                    finish_reason: None,
                }],
                usage: None,
            };
            vec![push(chunk)]
        }
        StreamEvent::ToolCallStart { index, id, name } => {
            let chunk = ChunkOut {
                id: s.response_id.clone(),
                object: "chat.completion.chunk",
                created: s.created,
                model: s.model.clone(),
                choices: vec![ChoiceOut {
                    index: 0,
                    delta: DeltaOut {
                        tool_calls: vec![ToolCallDeltaOut {
                            index,
                            id: Some(id.0),
                            kind: Some("function"),
                            function: ToolCallDeltaFn {
                                name: Some(name),
                                arguments: Some(String::new()),
                            },
                        }],
                        ..Default::default()
                    },
                    finish_reason: None,
                }],
                usage: None,
            };
            vec![push(chunk)]
        }
        StreamEvent::ToolCallArgumentsDelta { index, json_fragment } => {
            let chunk = ChunkOut {
                id: s.response_id.clone(),
                object: "chat.completion.chunk",
                created: s.created,
                model: s.model.clone(),
                choices: vec![ChoiceOut {
                    index: 0,
                    delta: DeltaOut {
                        tool_calls: vec![ToolCallDeltaOut {
                            index,
                            id: None,
                            kind: None,
                            function: ToolCallDeltaFn { name: None, arguments: Some(json_fragment) },
                        }],
                        ..Default::default()
                    },
                    finish_reason: None,
                }],
                usage: None,
            };
            vec![push(chunk)]
        }
        StreamEvent::MessageStop { stop_reason, .. } => {
            let chunk = ChunkOut {
                id: s.response_id.clone(),
                object: "chat.completion.chunk",
                created: s.created,
                model: s.model.clone(),
                choices: vec![ChoiceOut {
                    index: 0,
                    delta: DeltaOut::default(),
                    finish_reason: Some(finish_reason_from_canonical(&stop_reason).into()),
                }],
                usage: None,
            };
            vec![push(chunk)]
        }
        StreamEvent::ResponseStop { usage } => {
            if let Some(u) = usage {
                let chunk = ChunkOut {
                    id: s.response_id.clone(),
                    object: "chat.completion.chunk",
                    created: s.created,
                    model: s.model.clone(),
                    choices: vec![],
                    usage: Some(UsageOut {
                        prompt_tokens: u.input_tokens.unwrap_or(0),
                        completion_tokens: u.output_tokens.unwrap_or(0),
                        total_tokens: u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0),
                    }),
                };
                return vec![push(chunk)];
            }
            vec![]
        }
        StreamEvent::Error { message } => vec![Ok(emit_error(&s, message))],
        // ignored: ContentBlockStart/Stop, ToolCallStop, ReasoningDelta, UsageDelta (rolled into ResponseStop),
        // RawProviderEvent.
        _ => vec![],
    }
}

fn push(chunk: ChunkOut) -> Result<Bytes, FrontendError> {
    let s = serde_json::to_string(&chunk).map_err(|e| FrontendError::Encode(e.to_string()))?;
    Ok(sse::data_only(&s))
}

fn emit_error(s: &EncoderState, message: String) -> Bytes {
    let body = serde_json::json!({
        "error": { "message": message, "type": "api_error" },
        "id": s.response_id,
        "model": s.model,
    });
    sse::data_only(&body.to_string())
}

fn keepalive_merged(
    base: BoxStream<'static, Result<Bytes, FrontendError>>,
    period: Duration,
) -> BoxStream<'static, Result<Bytes, FrontendError>> {
    use tokio_stream::wrappers::IntervalStream;
    let interval = IntervalStream::new(tokio::time::interval(period))
        .map(|_| Ok(sse::comment("keepalive")));
    futures::stream::select(base, interval).boxed()
}
```

- [ ] **Step 2: `encode_unary.rs`**

```rust
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;

use agent_shim_core::content::ContentBlock;
use agent_shim_core::response::CanonicalResponse;

use super::mapping::finish_reason_from_canonical;
use super::wire::*;
use crate::FrontendError;

pub fn encode(resp: CanonicalResponse) -> Result<Bytes, FrontendError> {
    let mut content_text = String::new();
    let mut tool_calls = Vec::new();
    for block in resp.content {
        match block {
            ContentBlock::Text(t) => content_text.push_str(&t.text),
            ContentBlock::ToolCall(tc) => {
                let args = match tc.arguments {
                    agent_shim_core::tool::ToolCallArguments::Complete(v) => v.to_string(),
                    agent_shim_core::tool::ToolCallArguments::Streaming(r) => r.get().to_string(),
                };
                tool_calls.push(UnaryToolCall {
                    id: tc.id.0,
                    kind: "function",
                    function: UnaryFn { name: tc.name, arguments: args },
                });
            }
            _ => {}
        }
    }
    let out = ChatCompletionOut {
        id: resp.id.0,
        object: "chat.completion",
        created: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        model: resp.model,
        choices: vec![UnaryChoice {
            index: 0,
            message: UnaryMessage {
                role: "assistant",
                content: if content_text.is_empty() { None } else { Some(content_text) },
                tool_calls,
            },
            finish_reason: finish_reason_from_canonical(&resp.stop_reason).into(),
        }],
        usage: resp.usage.map(|u| UsageOut {
            prompt_tokens: u.input_tokens.unwrap_or(0),
            completion_tokens: u.output_tokens.unwrap_or(0),
            total_tokens: u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0),
        }),
    };
    serde_json::to_vec(&out).map(Bytes::from).map_err(|e| FrontendError::Encode(e.to_string()))
}
```

- [ ] **Step 3: `mod.rs`**

```rust
pub mod decode;
pub mod encode_stream;
pub mod encode_unary;
pub mod mapping;
pub mod wire;

use std::time::Duration;

use agent_shim_core::request::CanonicalRequest;
use agent_shim_core::response::CanonicalResponse;
use agent_shim_core::stream::CanonicalStream;
use agent_shim_core::target::FrontendKind;

use crate::{FrontendError, FrontendProtocol, FrontendResponse};

pub struct OpenAiChat {
    pub keepalive: Option<Duration>,
}

impl Default for OpenAiChat {
    fn default() -> Self { Self { keepalive: Some(Duration::from_secs(15)) } }
}

#[async_trait::async_trait]
impl FrontendProtocol for OpenAiChat {
    fn kind(&self) -> FrontendKind { FrontendKind::OpenAiChat }

    fn decode_request(&self, body: &[u8]) -> Result<CanonicalRequest, FrontendError> {
        decode::decode(body)
    }

    fn encode_unary(&self, response: CanonicalResponse) -> Result<FrontendResponse, FrontendError> {
        let body = encode_unary::encode(response)?;
        Ok(FrontendResponse::Unary { content_type: "application/json".into(), body })
    }

    fn encode_stream(&self, stream: CanonicalStream) -> FrontendResponse {
        let s = encode_stream::encode(stream, self.keepalive);
        FrontendResponse::Stream { content_type: "text/event-stream".into(), stream: s }
    }
}
```

- [ ] **Step 4: Build, commit**

Run: `cargo build -p agent-shim-frontends`
Expected: clean.

```bash
git add crates/frontends/src/openai_chat
git commit -m "feat(frontends/openai): SSE + unary encoders, FrontendProtocol impl, [DONE] terminator"
```

---

## Task 12: `protocol-tests` crate skeleton + `MockProvider`

**Files:**
- Create: `crates/protocol-tests/Cargo.toml`
- Create: `crates/protocol-tests/src/lib.rs`

- [ ] **Step 1: `Cargo.toml`**

```toml
[package]
name = "agent-shim-protocol-tests"
version.workspace = true
edition.workspace = true
license.workspace = true
publish = false

[lib]
name = "agent_shim_protocol_tests"
path = "src/lib.rs"

[dependencies]
agent-shim-core = { path = "../core" }
agent-shim-frontends = { path = "../frontends" }
serde.workspace = true
serde_json.workspace = true
futures = { workspace = true }
futures-util = { workspace = true }
bytes.workspace = true
tokio = { workspace = true, features = ["macros", "rt", "fs"] }

[dev-dependencies]
pretty_assertions.workspace = true
```

- [ ] **Step 2: `src/lib.rs`**

```rust
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use futures::stream::{self, StreamExt};
use futures_util::stream::BoxStream;

use agent_shim_core::error::StreamError;
use agent_shim_core::stream::{CanonicalStream, StreamEvent};

/// Build a `CanonicalStream` from a JSONL fixture (one `StreamEvent` per line).
pub fn replay_jsonl<P: AsRef<Path>>(path: P, per_event_delay: Option<Duration>) -> CanonicalStream {
    let bytes = std::fs::read(path.as_ref())
        .unwrap_or_else(|e| panic!("read {}: {e}", path.as_ref().display()));
    let text = String::from_utf8(bytes).expect("fixture is utf-8");
    let events: Vec<Result<StreamEvent, StreamError>> = text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with("//"))
        .map(|l| serde_json::from_str::<StreamEvent>(l).unwrap_or_else(|e| panic!("bad fixture line `{l}`: {e}")))
        .map(Ok)
        .collect();

    if let Some(d) = per_event_delay {
        Box::pin(stream::iter(events).then(move |e| async move {
            tokio::time::sleep(d).await;
            e
        }))
    } else {
        Box::pin(stream::iter(events))
    }
}

/// Collect the full body of a frontend Stream response into a single `Bytes`.
pub async fn collect_sse(s: BoxStream<'static, Result<Bytes, agent_shim_frontends::FrontendError>>) -> Bytes {
    let chunks: Vec<Bytes> = s.map(|r| r.expect("encode error")).collect().await;
    let mut all = bytes::BytesMut::new();
    for c in chunks {
        all.extend_from_slice(&c);
    }
    all.freeze()
}

/// Read a fixture file as `Bytes`.
pub fn read_fixture<P: AsRef<Path>>(p: P) -> Bytes {
    Bytes::from(std::fs::read(p.as_ref()).unwrap_or_else(|e| panic!("read {}: {e}", p.as_ref().display())))
}

/// Resolve a path relative to this crate.
pub fn fixture(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(name)
}
```

- [ ] **Step 3: Build, commit**

Run: `cargo build -p agent-shim-protocol-tests`
Expected: clean.

```bash
git add crates/protocol-tests
git commit -m "test(protocol-tests): MockProvider replay + fixture helpers"
```

---

## Task 13: Canonical fixtures + Anthropic text stream golden test

**Files:**
- Create: `crates/protocol-tests/fixtures/canonical/text_stream.jsonl`
- Create: `crates/protocol-tests/fixtures/anthropic/text_stream.sse`
- Create: `crates/protocol-tests/tests/anthropic_text_stream.rs`

- [ ] **Step 1: Canonical fixture**

`crates/protocol-tests/fixtures/canonical/text_stream.jsonl`:

```jsonl
{"type":"response_start","id":"resp_test1","model":"claude-3-5-sonnet","created_at_unix":1700000000}
{"type":"message_start","role":"assistant"}
{"type":"content_block_start","index":0,"kind":"text"}
{"type":"text_delta","index":0,"text":"Hello"}
{"type":"text_delta","index":0,"text":", world"}
{"type":"content_block_stop","index":0}
{"type":"message_stop","stop_reason":{"kind":"end_turn"},"stop_sequence":null}
{"type":"response_stop","usage":{"input_tokens":12,"output_tokens":3,"estimated":false}}
```

- [ ] **Step 2: Expected Anthropic SSE fixture**

`crates/protocol-tests/fixtures/anthropic/text_stream.sse`:

```
event: message_start
data: {"type":"message_start","message":{"id":"resp_test1","type":"message","role":"assistant","model":"claude-3-5-sonnet","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":", world"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":12,"output_tokens":3}}

event: message_stop
data: {"type":"message_stop"}

```

(Note: file ends with a trailing blank line — the final `\n\n` of the last event.)

- [ ] **Step 3: Test**

`crates/protocol-tests/tests/anthropic_text_stream.rs`:

```rust
use agent_shim_frontends::anthropic_messages::AnthropicMessages;
use agent_shim_frontends::{FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{collect_sse, fixture, read_fixture, replay_jsonl};
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "current_thread")]
async fn anthropic_text_stream_matches_golden() {
    let canonical = replay_jsonl(fixture("canonical/text_stream.jsonl"), None);
    let frontend = AnthropicMessages { keepalive: None };
    let response = frontend.encode_stream(canonical);
    let bytes = match response {
        FrontendResponse::Stream { stream, .. } => collect_sse(stream).await,
        _ => panic!("expected Stream"),
    };
    let expected = read_fixture(fixture("anthropic/text_stream.sse"));
    assert_eq!(
        std::str::from_utf8(&bytes).unwrap(),
        std::str::from_utf8(&expected).unwrap()
    );
}
```

- [ ] **Step 4: Run and refine**

Run: `cargo test -p agent-shim-protocol-tests --test anthropic_text_stream`

If the byte diff fails, the test prints a colored diff via `pretty_assertions`. Adjust either the fixture (preferred — capture intent) or the encoder (if it's wrong) until it passes. **Do not weaken the test.**

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/protocol-tests
git commit -m "test(protocol-tests): Anthropic text-stream golden SSE roundtrip"
```

---

## Task 14: Anthropic tool-call golden test

**Files:**
- Create: `crates/protocol-tests/fixtures/canonical/tool_call_stream.jsonl`
- Create: `crates/protocol-tests/fixtures/anthropic/tool_call_stream.sse`
- Create: `crates/protocol-tests/tests/anthropic_tool_call_stream.rs`

- [ ] **Step 1: Canonical fixture**

```jsonl
{"type":"response_start","id":"resp_t2","model":"claude-3-5-sonnet","created_at_unix":1700000001}
{"type":"message_start","role":"assistant"}
{"type":"tool_call_start","index":0,"id":"call_abc","name":"get_weather"}
{"type":"tool_call_arguments_delta","index":0,"json_fragment":"{\"city\":"}
{"type":"tool_call_arguments_delta","index":0,"json_fragment":"\"Tokyo\"}"}
{"type":"content_block_stop","index":0}
{"type":"message_stop","stop_reason":{"kind":"tool_use"},"stop_sequence":null}
{"type":"response_stop","usage":{"input_tokens":20,"output_tokens":7,"estimated":false}}
```

- [ ] **Step 2: Expected SSE**

```
event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_abc","name":"get_weather","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"Tokyo\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":20,"output_tokens":7}}

event: message_stop
data: {"type":"message_stop"}

```

(Plus the leading `message_start` event — copy structure from Task 13's fixture, replacing id/model/etc.)

Build the full expected file by prefixing the above with the same `message_start` block from Task 13 with `id=resp_t2`. **Important:** Get the exact bytes right when first running the test; let the diff drive corrections.

- [ ] **Step 3: Test**

```rust
use agent_shim_frontends::anthropic_messages::AnthropicMessages;
use agent_shim_frontends::{FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{collect_sse, fixture, read_fixture, replay_jsonl};
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "current_thread")]
async fn anthropic_tool_call_stream_matches_golden() {
    let canonical = replay_jsonl(fixture("canonical/tool_call_stream.jsonl"), None);
    let frontend = AnthropicMessages { keepalive: None };
    let response = frontend.encode_stream(canonical);
    let bytes = match response {
        FrontendResponse::Stream { stream, .. } => collect_sse(stream).await,
        _ => panic!("expected Stream"),
    };
    let expected = read_fixture(fixture("anthropic/tool_call_stream.sse"));
    assert_eq!(
        std::str::from_utf8(&bytes).unwrap(),
        std::str::from_utf8(&expected).unwrap()
    );
}
```

- [ ] **Step 4: Run, refine, commit**

Run: `cargo test -p agent-shim-protocol-tests --test anthropic_tool_call_stream`
Expected: PASS (after fixture + encoder converge).

```bash
git add crates/protocol-tests
git commit -m "test(protocol-tests): Anthropic tool-call streaming golden SSE"
```

---

## Task 15: OpenAI text-stream golden test

**Files:**
- Create: `crates/protocol-tests/fixtures/openai/text_stream.sse`
- Create: `crates/protocol-tests/tests/openai_text_stream.rs`

- [ ] **Step 1: Expected SSE (re-uses canonical/text_stream.jsonl from Task 13)**

```
data: {"id":"resp_test1","object":"chat.completion.chunk","created":1700000000,"model":"claude-3-5-sonnet","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}

data: {"id":"resp_test1","object":"chat.completion.chunk","created":1700000000,"model":"claude-3-5-sonnet","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}

data: {"id":"resp_test1","object":"chat.completion.chunk","created":1700000000,"model":"claude-3-5-sonnet","choices":[{"index":0,"delta":{"content":", world"},"finish_reason":null}]}

data: {"id":"resp_test1","object":"chat.completion.chunk","created":1700000000,"model":"claude-3-5-sonnet","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: {"id":"resp_test1","object":"chat.completion.chunk","created":1700000000,"model":"claude-3-5-sonnet","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}

data: [DONE]

```

**`created` field issue:** the encoder uses `SystemTime::now()` so the timestamp varies. To make the test deterministic, the `OpenAiChat` adapter needs an injectable clock. Add it now:

In `crates/frontends/src/openai_chat/mod.rs`:

```rust
pub struct OpenAiChat {
    pub keepalive: Option<Duration>,
    /// Optional fixed `created` timestamp (Unix seconds). When `None`, uses `SystemTime::now()`.
    pub clock_override: Option<u64>,
}

impl Default for OpenAiChat {
    fn default() -> Self { Self { keepalive: Some(Duration::from_secs(15)), clock_override: None } }
}
```

Update `encode_stream::encode` signature to accept `clock_override: Option<u64>`, store it on `EncoderState.created`, and use `1700000000` as the fixture timestamp.

In `mod.rs::encode_stream`, pass `self.clock_override` through.

- [ ] **Step 2: Test**

```rust
use std::time::Duration;
use agent_shim_frontends::openai_chat::OpenAiChat;
use agent_shim_frontends::{FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{collect_sse, fixture, read_fixture, replay_jsonl};
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "current_thread")]
async fn openai_text_stream_matches_golden() {
    let canonical = replay_jsonl(fixture("canonical/text_stream.jsonl"), None);
    let frontend = OpenAiChat { keepalive: None, clock_override: Some(1_700_000_000) };
    let response = frontend.encode_stream(canonical);
    let bytes = match response {
        FrontendResponse::Stream { stream, .. } => collect_sse(stream).await,
        _ => panic!("expected Stream"),
    };
    let expected = read_fixture(fixture("openai/text_stream.sse"));
    assert_eq!(
        std::str::from_utf8(&bytes).unwrap(),
        std::str::from_utf8(&expected).unwrap()
    );
    let _ = Duration::from_secs(0);
}
```

- [ ] **Step 3: Run, refine, commit**

Run: `cargo test -p agent-shim-protocol-tests --test openai_text_stream`
Expected: PASS.

```bash
git add crates/frontends/src/openai_chat crates/protocol-tests
git commit -m "test(protocol-tests): OpenAI text-stream golden SSE with deterministic clock"
```

---

## Task 16: OpenAI tool-call golden test

**Files:**
- Create: `crates/protocol-tests/fixtures/openai/tool_call_stream.sse`
- Create: `crates/protocol-tests/tests/openai_tool_call_stream.rs`

- [ ] **Step 1: Expected SSE (uses tool_call_stream.jsonl from Task 14)**

```
data: {"id":"resp_t2","object":"chat.completion.chunk","created":1700000001,"model":"claude-3-5-sonnet","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}

data: {"id":"resp_t2","object":"chat.completion.chunk","created":1700000001,"model":"claude-3-5-sonnet","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}

data: {"id":"resp_t2","object":"chat.completion.chunk","created":1700000001,"model":"claude-3-5-sonnet","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]},"finish_reason":null}]}

data: {"id":"resp_t2","object":"chat.completion.chunk","created":1700000001,"model":"claude-3-5-sonnet","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Tokyo\"}"}}]},"finish_reason":null}]}

data: {"id":"resp_t2","object":"chat.completion.chunk","created":1700000001,"model":"claude-3-5-sonnet","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

data: {"id":"resp_t2","object":"chat.completion.chunk","created":1700000001,"model":"claude-3-5-sonnet","choices":[],"usage":{"prompt_tokens":20,"completion_tokens":7,"total_tokens":27}}

data: [DONE]

```

- [ ] **Step 2: Test**

```rust
use agent_shim_frontends::openai_chat::OpenAiChat;
use agent_shim_frontends::{FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{collect_sse, fixture, read_fixture, replay_jsonl};
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "current_thread")]
async fn openai_tool_call_stream_matches_golden() {
    let canonical = replay_jsonl(fixture("canonical/tool_call_stream.jsonl"), None);
    let frontend = OpenAiChat { keepalive: None, clock_override: Some(1_700_000_001) };
    let response = frontend.encode_stream(canonical);
    let bytes = match response {
        FrontendResponse::Stream { stream, .. } => collect_sse(stream).await,
        _ => panic!("expected Stream"),
    };
    let expected = read_fixture(fixture("openai/tool_call_stream.sse"));
    assert_eq!(
        std::str::from_utf8(&bytes).unwrap(),
        std::str::from_utf8(&expected).unwrap()
    );
}
```

- [ ] **Step 3: Run, refine, commit**

Run: `cargo test -p agent-shim-protocol-tests --test openai_tool_call_stream`
Expected: PASS.

```bash
git add crates/protocol-tests
git commit -m "test(protocol-tests): OpenAI tool-call streaming golden SSE"
```

---

## Task 17: Unary golden tests (both frontends)

**Files:**
- Create: `crates/protocol-tests/tests/anthropic_unary.rs`
- Create: `crates/protocol-tests/tests/openai_unary.rs`

- [ ] **Step 1: Anthropic unary**

```rust
use agent_shim_core::{
    content::{ContentBlock, TextBlock},
    ids::ResponseId,
    response::CanonicalResponse,
    usage::{StopReason, Usage},
};
use agent_shim_frontends::anthropic_messages::AnthropicMessages;
use agent_shim_frontends::{FrontendProtocol, FrontendResponse};
use pretty_assertions::assert_eq;

#[test]
fn anthropic_unary_text_response_shape() {
    let resp = CanonicalResponse {
        id: ResponseId("resp_u1".into()),
        model: "claude-3-5-sonnet".into(),
        content: vec![ContentBlock::Text(TextBlock { text: "hi".into(), extensions: Default::default() })],
        stop_reason: StopReason::EndTurn,
        stop_sequence: None,
        usage: Some(Usage { input_tokens: Some(5), output_tokens: Some(2), ..Default::default() }),
    };
    let frontend = AnthropicMessages::default();
    let body = match frontend.encode_unary(resp).unwrap() {
        FrontendResponse::Unary { body, .. } => body,
        _ => panic!(),
    };
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["id"], "resp_u1");
    assert_eq!(v["type"], "message");
    assert_eq!(v["role"], "assistant");
    assert_eq!(v["content"][0]["type"], "text");
    assert_eq!(v["content"][0]["text"], "hi");
    assert_eq!(v["stop_reason"], "end_turn");
    assert_eq!(v["usage"]["input_tokens"], 5);
}
```

- [ ] **Step 2: OpenAI unary**

```rust
use agent_shim_core::{
    content::{ContentBlock, TextBlock},
    ids::ResponseId,
    response::CanonicalResponse,
    usage::{StopReason, Usage},
};
use agent_shim_frontends::openai_chat::OpenAiChat;
use agent_shim_frontends::{FrontendProtocol, FrontendResponse};

#[test]
fn openai_unary_text_response_shape() {
    let resp = CanonicalResponse {
        id: ResponseId("chatcmpl_u1".into()),
        model: "gpt-4o".into(),
        content: vec![ContentBlock::Text(TextBlock { text: "hi".into(), extensions: Default::default() })],
        stop_reason: StopReason::EndTurn,
        stop_sequence: None,
        usage: Some(Usage { input_tokens: Some(5), output_tokens: Some(2), ..Default::default() }),
    };
    let frontend = OpenAiChat::default();
    let body = match frontend.encode_unary(resp).unwrap() {
        FrontendResponse::Unary { body, .. } => body,
        _ => panic!(),
    };
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["id"], "chatcmpl_u1");
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["choices"][0]["message"]["role"], "assistant");
    assert_eq!(v["choices"][0]["message"]["content"], "hi");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(v["usage"]["prompt_tokens"], 5);
    assert_eq!(v["usage"]["total_tokens"], 7);
}
```

- [ ] **Step 3: Run + commit**

Run: `cargo test -p agent-shim-protocol-tests --test anthropic_unary --test openai_unary`
Expected: 2 passed.

```bash
git add crates/protocol-tests
git commit -m "test(protocol-tests): unary response shape tests for both frontends"
```

---

## Task 18: Cross-protocol semantic tests

**Files:**
- Create: `crates/protocol-tests/tests/cross_anthropic_to_openai.rs`
- Create: `crates/protocol-tests/tests/cross_openai_to_anthropic.rs`

- [ ] **Step 1: Anthropic-in → OpenAI-out**

These assert *semantic* equivalence (event types and data fields), not byte equality, because the wire format intentionally differs.

```rust
use agent_shim_frontends::openai_chat::OpenAiChat;
use agent_shim_frontends::{FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{collect_sse, fixture, replay_jsonl};

#[tokio::test(flavor = "current_thread")]
async fn anthropic_request_canonical_streams_through_openai_encoder() {
    // Request: an Anthropic-shaped messages request decoded to canonical
    let req_body = br#"{"model":"claude-3-5-sonnet","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}"#;
    let frontend_in = agent_shim_frontends::anthropic_messages::AnthropicMessages::default();
    let canonical_req = frontend_in.decode_request(req_body).unwrap();
    assert_eq!(canonical_req.messages.len(), 1);
    assert_eq!(canonical_req.model.0, "claude-3-5-sonnet");

    // Stream: replay canonical events, encode as OpenAI SSE
    let canonical = replay_jsonl(fixture("canonical/text_stream.jsonl"), None);
    let frontend_out = OpenAiChat { keepalive: None, clock_override: Some(1_700_000_000) };
    let bytes = match frontend_out.encode_stream(canonical) {
        FrontendResponse::Stream { stream, .. } => collect_sse(stream).await,
        _ => panic!(),
    };
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("\"role\":\"assistant\""));
    assert!(text.contains("\"content\":\"Hello\""));
    assert!(text.contains("\"finish_reason\":\"stop\""));
    assert!(text.ends_with("data: [DONE]\n\n"));
}
```

- [ ] **Step 2: OpenAI-in → Anthropic-out**

```rust
use agent_shim_frontends::anthropic_messages::AnthropicMessages;
use agent_shim_frontends::{FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{collect_sse, fixture, replay_jsonl};

#[tokio::test(flavor = "current_thread")]
async fn openai_request_canonical_streams_through_anthropic_encoder() {
    let req_body = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"stream":true}"#;
    let frontend_in = agent_shim_frontends::openai_chat::OpenAiChat::default();
    let canonical_req = frontend_in.decode_request(req_body).unwrap();
    assert_eq!(canonical_req.model.0, "gpt-4o");
    assert!(canonical_req.stream);

    let canonical = replay_jsonl(fixture("canonical/tool_call_stream.jsonl"), None);
    let frontend_out = AnthropicMessages { keepalive: None };
    let bytes = match frontend_out.encode_stream(canonical) {
        FrontendResponse::Stream { stream, .. } => collect_sse(stream).await,
        _ => panic!(),
    };
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("event: content_block_start"));
    assert!(text.contains("\"type\":\"tool_use\""));
    assert!(text.contains("\"name\":\"get_weather\""));
    assert!(text.contains("\"type\":\"input_json_delta\""));
    assert!(text.contains("\"stop_reason\":\"tool_use\""));
}
```

- [ ] **Step 3: Run + commit**

Run: `cargo test -p agent-shim-protocol-tests --test cross_anthropic_to_openai --test cross_openai_to_anthropic`
Expected: 2 passed.

```bash
git add crates/protocol-tests
git commit -m "test(protocol-tests): cross-protocol semantic tests for both directions"
```

---

## Self-Review Notes

- Spec §3 `FrontendProtocol` trait shape implemented (decode/encode_unary/encode_stream). ✓ — note: `decode_request` takes raw `&[u8]` instead of full `HttpRequest`; HTTP wrapping happens in `gateway` (Plan 04). Trade-off: lets adapters be tested without an HTTP runtime.
- Spec §5 stream pipeline (canonical → frontend SSE) implemented for both frontends with golden tests. ✓
- Spec §5 `[DONE]` terminator on OpenAI: present (Task 11). ✓
- Spec §5 heartbeats configurable: `Anthropic` `ping`, `OpenAI` `: comment` — both behind `Option<Duration>` field. ✓
- Spec §7 hard problems addressed:
  - #1 tool-call streaming deltas: `json_fragment: String` round-tripped on both sides. ✓
  - #2 system/developer placement: `SystemSource` discriminator preserved on decode. ✓
  - #3 reasoning/thinking blocks: decoded for Anthropic, no special OpenAI handling (OpenAI doesn't expose them). ✓
  - #5 stop reason mapping: both directions, with explicit table + tests. ✓
  - #6 SSE wire-shape parity: golden tests in place. ✓
  - #13 Anthropic `cache_control`: written to `extensions` on decode (text/image/tool blocks); provider passthrough belongs in Plan 04. ✓
- Spec §10 Layer 1 (unit) and Layer 2 (golden) coverage in place. Layer 3 (live API e2e) deferred to feature flag in a later plan.
- Self-review: searched for "TODO/TBD/placeholder" — none. Type names consistent (`OpenAiChat`, `AnthropicMessages`). `clock_override` added in Task 15 only after needed; back-references explicit.
