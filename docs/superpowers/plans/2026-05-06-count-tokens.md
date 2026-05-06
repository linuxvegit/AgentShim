# `/v1/messages/count_tokens` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a working Anthropic-shaped `POST /v1/messages/count_tokens` endpoint to AgentShim so Claude Desktop's Agent SDK preflight stops 404'ing and the client can issue real `/v1/messages` calls.

**Architecture:** Local-only token approximation. The endpoint reuses the existing `anthropic_messages` decoder to turn the body into a `CanonicalRequest`, then walks it with a `tiktoken-rs` (cl100k_base) tokenizer plus per-piece structural overhead. No upstream call. No new provider trait method. No router involvement. Returns `{ "input_tokens": N }`.

**Tech Stack:** Rust 2021, axum 0.7, serde, `tiktoken-rs` 0.5 (new), `parking_lot::OnceCell` (already in workspace), `reqwest` (test-only, already in dev-deps).

**Spec:** `docs/superpowers/specs/2026-05-06-count-tokens-design.md`

---

## File map

**New files:**
- `crates/frontends/src/anthropic_messages/count_tokens_wire.rs` — wire struct that mirrors `MessagesRequest` but with optional `max_tokens`. Provides `into_messages_request()` for re-using `decode::decode`.
- `crates/frontends/src/anthropic_messages/count_tokens.rs` — the `count(req: &CanonicalRequest) -> u32` function plus the cl100k_base tokenizer cell, the structural-overhead constants, and unit tests.
- `crates/gateway/src/handlers/anthropic_count_tokens.rs` — thin axum handler.
- `crates/gateway/tests/count_tokens_smoke.rs` — integration tests against an ephemeral port.

**Modified files:**
- `Cargo.toml` (workspace) — add `tiktoken-rs = "0.5"` to `[workspace.dependencies]`.
- `crates/frontends/Cargo.toml` — depend on `tiktoken-rs.workspace = true`.
- `crates/frontends/src/anthropic_messages/mod.rs` — `pub mod count_tokens;` and `pub mod count_tokens_wire;`.
- `crates/gateway/src/handlers/mod.rs` — `pub mod anthropic_count_tokens;`.
- `crates/gateway/src/server.rs` — one new `.route(...)` line.

**Untouched:** `core`, `providers`, `router`, `config`, `observability`.

---

## Task 1: Add `tiktoken-rs` to the workspace

**Why first:** Every later task needs this dependency. Get it compiling cleanly before writing code that uses it.

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/frontends/Cargo.toml`

- [ ] **Step 1: Add `tiktoken-rs` to workspace dependencies**

Edit `Cargo.toml` (the workspace one at the repo root). Add this line in the `[workspace.dependencies]` section, alphabetically after `thiserror`:

```toml
tiktoken-rs = "0.5"
```

The full surrounding context after the edit:

```toml
serde_json = { version = "1", features = ["raw_value", "preserve_order"] }
thiserror = "1"
tiktoken-rs = "0.5"
bytes = "1"
```

(Position doesn't matter functionally; alphabetical-ish is fine.)

- [ ] **Step 2: Wire it into the `frontends` crate**

Edit `crates/frontends/Cargo.toml`. Add this line in the `[dependencies]` section after `parking_lot.workspace = true`:

```toml
tiktoken-rs.workspace = true
```

- [ ] **Step 3: Verify the dependency resolves**

Run: `cargo build -p agent-shim-frontends`
Expected: success, no compile errors. (You're not using the dep yet, so a `unused_imports` warning would be the only signal of trouble — there shouldn't be one because nothing imports it yet.)

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock crates/frontends/Cargo.toml
git commit -m "deps(frontends): add tiktoken-rs for local token counting"
```

---

## Task 2: Create the `count_tokens_wire` module

The existing `MessagesRequest` requires `max_tokens`, but the count_tokens API doesn't. Rather than make `max_tokens` optional everywhere (touches many tests), introduce a small mirror struct that decodes the optional shape and emits a `MessagesRequest` with `max_tokens = 0` injected.

**Files:**
- Create: `crates/frontends/src/anthropic_messages/count_tokens_wire.rs`
- Modify: `crates/frontends/src/anthropic_messages/mod.rs`

- [ ] **Step 1: Write the failing test**

Append to a new file `crates/frontends/src/anthropic_messages/count_tokens_wire.rs` — for now write only the `#[cfg(test)]` block so we can see it fail. Create the file with this content:

```rust
//! Wire types for `/v1/messages/count_tokens`.
//!
//! Mirrors `MessagesRequest` from `wire.rs` but makes `max_tokens` optional
//! (the count_tokens API does not require it). `into_messages_request` injects
//! `max_tokens: 0` so the existing `decode::decode` path is reused unchanged.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_without_max_tokens() {
        let body = br#"{
            "model": "claude-opus-4-7",
            "messages": [{"role":"user","content":"hi"}]
        }"#;
        let req: CountTokensRequest = serde_json::from_slice(body).unwrap();
        assert_eq!(req.model, "claude-opus-4-7");
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn decodes_with_max_tokens_present() {
        let body = br#"{
            "model": "m",
            "max_tokens": 1024,
            "messages": [{"role":"user","content":"hi"}]
        }"#;
        let req: CountTokensRequest = serde_json::from_slice(body).unwrap();
        assert_eq!(req.max_tokens, Some(1024));
    }

    #[test]
    fn into_messages_request_injects_zero_when_absent() {
        let body = br#"{
            "model": "m",
            "messages": [{"role":"user","content":"hi"}]
        }"#;
        let req: CountTokensRequest = serde_json::from_slice(body).unwrap();
        let msg = req.into_messages_request();
        assert_eq!(msg.max_tokens, 0);
    }

    #[test]
    fn into_messages_request_preserves_existing_max_tokens() {
        let body = br#"{
            "model": "m",
            "max_tokens": 512,
            "messages": [{"role":"user","content":"hi"}]
        }"#;
        let req: CountTokensRequest = serde_json::from_slice(body).unwrap();
        let msg = req.into_messages_request();
        assert_eq!(msg.max_tokens, 512);
    }
}
```

Edit `crates/frontends/src/anthropic_messages/mod.rs` and add this line after the existing `pub mod` declarations:

```rust
pub mod count_tokens_wire;
```

The full top of the file after the edit should read:

```rust
pub mod count_tokens_wire;
pub mod decode;
pub mod encode_stream;
pub mod encode_unary;
pub mod mapping;
pub mod wire;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p agent-shim-frontends count_tokens_wire`
Expected: FAIL — compile error, `cannot find type CountTokensRequest in this scope`.

- [ ] **Step 3: Implement `CountTokensRequest` and `into_messages_request`**

Replace the entire content of `crates/frontends/src/anthropic_messages/count_tokens_wire.rs` with:

```rust
//! Wire types for `/v1/messages/count_tokens`.
//!
//! Mirrors `MessagesRequest` from `wire.rs` but makes `max_tokens` optional
//! (the count_tokens API does not require it). `into_messages_request` injects
//! `max_tokens: 0` so the existing `decode::decode` path is reused unchanged.

use serde::Deserialize;
use serde_json::Value;

use super::wire::{
    InboundMessage, InboundTool, InboundToolChoice, MessagesRequest, SystemField, ThinkingConfig,
};

#[derive(Debug, Clone, Deserialize)]
pub struct CountTokensRequest {
    pub model: String,
    pub messages: Vec<InboundMessage>,
    #[serde(default)]
    pub system: Option<SystemField>,
    #[serde(default)]
    pub tools: Option<Vec<InboundTool>>,
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
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
}

impl CountTokensRequest {
    /// Convert to `MessagesRequest` for re-use of the existing decoder.
    /// `max_tokens` is filled with 0 when absent — count_tokens never reads it.
    pub fn into_messages_request(self) -> MessagesRequest {
        MessagesRequest {
            model: self.model,
            messages: self.messages,
            system: self.system,
            tools: self.tools,
            tool_choice: self.tool_choice,
            max_tokens: self.max_tokens.unwrap_or(0),
            temperature: self.temperature,
            top_p: self.top_p,
            top_k: self.top_k,
            stop_sequences: self.stop_sequences,
            stream: None,
            metadata: self.metadata,
            thinking: self.thinking,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_without_max_tokens() {
        let body = br#"{
            "model": "claude-opus-4-7",
            "messages": [{"role":"user","content":"hi"}]
        }"#;
        let req: CountTokensRequest = serde_json::from_slice(body).unwrap();
        assert_eq!(req.model, "claude-opus-4-7");
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn decodes_with_max_tokens_present() {
        let body = br#"{
            "model": "m",
            "max_tokens": 1024,
            "messages": [{"role":"user","content":"hi"}]
        }"#;
        let req: CountTokensRequest = serde_json::from_slice(body).unwrap();
        assert_eq!(req.max_tokens, Some(1024));
    }

    #[test]
    fn into_messages_request_injects_zero_when_absent() {
        let body = br#"{
            "model": "m",
            "messages": [{"role":"user","content":"hi"}]
        }"#;
        let req: CountTokensRequest = serde_json::from_slice(body).unwrap();
        let msg = req.into_messages_request();
        assert_eq!(msg.max_tokens, 0);
    }

    #[test]
    fn into_messages_request_preserves_existing_max_tokens() {
        let body = br#"{
            "model": "m",
            "max_tokens": 512,
            "messages": [{"role":"user","content":"hi"}]
        }"#;
        let req: CountTokensRequest = serde_json::from_slice(body).unwrap();
        let msg = req.into_messages_request();
        assert_eq!(msg.max_tokens, 512);
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p agent-shim-frontends count_tokens_wire`
Expected: 4 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/frontends/src/anthropic_messages/count_tokens_wire.rs \
         crates/frontends/src/anthropic_messages/mod.rs
git commit -m "feat(frontends): add count_tokens_wire mirror of MessagesRequest"
```

---

## Task 3: The token counter — base text and per-message overhead

We'll build `count_tokens.rs` incrementally, one block type at a time, each behind a passing test.

**Files:**
- Create: `crates/frontends/src/anthropic_messages/count_tokens.rs`
- Modify: `crates/frontends/src/anthropic_messages/mod.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/frontends/src/anthropic_messages/count_tokens.rs` with this content:

```rust
//! Local token-count approximation for `/v1/messages/count_tokens`.
//!
//! Pure function over a `CanonicalRequest`. Uses `tiktoken-rs` cl100k_base
//! plus per-piece structural overhead constants. See
//! `docs/superpowers/specs/2026-05-06-count-tokens-design.md` for the
//! algorithm and the rationale behind each constant.

use agent_shim_core::{
    content::ContentBlock,
    message::Message,
    request::CanonicalRequest,
    tool::{ToolCallArguments, ToolChoice, ToolDefinition},
};
use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;

// Per-piece structural overhead. See spec §"Per-piece structural overhead".
const PER_MESSAGE: u32 = 4;
const PER_SYSTEM: u32 = 4;
const PER_TOOL_USE: u32 = 8;
const PER_TOOL_RESULT: u32 = 6;
const PER_REASONING: u32 = 4;
const PER_TOOL_DEF: u32 = 10;
const PER_TOOL_CHOICE: u32 = 6;
const PER_IMAGE: u32 = 200;

/// Count the approximate input tokens of a canonical request.
pub fn count(req: &CanonicalRequest) -> u32 {
    let mut total: u32 = 0;
    for sys in &req.system {
        total = total.saturating_add(PER_SYSTEM);
        for block in &sys.content {
            total = total.saturating_add(count_block(block));
        }
    }
    for msg in &req.messages {
        total = total.saturating_add(count_message(msg));
    }
    for tool in &req.tools {
        total = total.saturating_add(count_tool(tool));
    }
    total = total.saturating_add(count_tool_choice(&req.tool_choice));
    total
}

fn count_message(msg: &Message) -> u32 {
    let mut t = PER_MESSAGE;
    for block in &msg.content {
        t = t.saturating_add(count_block(block));
    }
    t
}

fn count_block(block: &ContentBlock) -> u32 {
    match block {
        ContentBlock::Text(b) => count_text(&b.text),
        _ => 0, // implemented in later tasks
    }
}

fn count_tool(_t: &ToolDefinition) -> u32 {
    0 // implemented in a later task
}

fn count_tool_choice(_c: &ToolChoice) -> u32 {
    0 // implemented in a later task
}

/// Count the cl100k_base tokens in a string. Uses `encode_ordinary` so
/// `<|...|>` literals in user content do not blow up.
fn count_text(s: &str) -> u32 {
    static CELL: OnceLock<CoreBPE> = OnceLock::new();
    let bpe = CELL
        .get_or_init(|| tiktoken_rs::cl100k_base().expect("cl100k_base tokenizer must initialize"));
    bpe.encode_ordinary(s).len() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        content::TextBlock,
        extensions::ExtensionMap,
        ids::RequestId,
        message::{Message, MessageRole},
        request::{CanonicalRequest, GenerationOptions, RequestMetadata},
        target::{FrontendInfo, FrontendKind, FrontendModel},
    };

    fn empty_request() -> CanonicalRequest {
        let model = FrontendModel("m".into());
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: model.clone(),
            },
            model,
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            generation: GenerationOptions::default(),
            response_format: None,
            stream: false,
            metadata: RequestMetadata::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: Default::default(),
            extensions: ExtensionMap::new(),
        }
    }

    fn user_text(s: &str) -> Message {
        Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text(TextBlock {
                text: s.into(),
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        }
    }

    #[test]
    fn empty_request_counts_zero() {
        let req = empty_request();
        assert_eq!(count(&req), 0);
    }

    #[test]
    fn single_user_text_message_equals_tokenizer_plus_per_message() {
        let mut req = empty_request();
        req.messages.push(user_text("hello world"));
        let expected = count_text("hello world") + PER_MESSAGE;
        assert_eq!(count(&req), expected);
    }

    #[test]
    fn three_messages_sum_to_three_per_message_overheads_plus_text() {
        let mut req = empty_request();
        req.messages.push(user_text("a"));
        req.messages.push(user_text("b"));
        req.messages.push(user_text("c"));
        let expected = count_text("a") + count_text("b") + count_text("c") + 3 * PER_MESSAGE;
        assert_eq!(count(&req), expected);
    }

    #[test]
    fn deterministic_same_input_same_output() {
        let mut req = empty_request();
        req.messages.push(user_text("the quick brown fox"));
        assert_eq!(count(&req), count(&req));
    }
}
```

Edit `crates/frontends/src/anthropic_messages/mod.rs` to add the module declaration after `count_tokens_wire`:

```rust
pub mod count_tokens;
pub mod count_tokens_wire;
pub mod decode;
pub mod encode_stream;
pub mod encode_unary;
pub mod mapping;
pub mod wire;
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo nextest run -p agent-shim-frontends count_tokens::`
Expected: 4 tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/frontends/src/anthropic_messages/count_tokens.rs \
         crates/frontends/src/anthropic_messages/mod.rs
git commit -m "feat(frontends): count_tokens base — text blocks + per-message overhead"
```

---

## Task 4: System instructions

**Files:**
- Modify: `crates/frontends/src/anthropic_messages/count_tokens.rs`

- [ ] **Step 1: Write the failing test**

Add this test inside the existing `mod tests` block in `count_tokens.rs`:

```rust
    #[test]
    fn system_instruction_adds_per_system_overhead_plus_content() {
        use agent_shim_core::message::{SystemInstruction, SystemSource};
        let mut req = empty_request();
        req.system.push(SystemInstruction {
            source: SystemSource::AnthropicSystem,
            content: vec![ContentBlock::Text(TextBlock {
                text: "you are helpful".into(),
                extensions: ExtensionMap::new(),
            })],
        });
        let expected = count_text("you are helpful") + PER_SYSTEM;
        assert_eq!(count(&req), expected);
    }
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo nextest run -p agent-shim-frontends count_tokens::tests::system_instruction`
Expected: PASS. (System counting was already implemented in Task 3 — this test ratifies it.)

- [ ] **Step 3: Commit**

```bash
git add crates/frontends/src/anthropic_messages/count_tokens.rs
git commit -m "test(frontends): cover system instruction counting"
```

---

## Task 5: Tool-use and tool-result blocks

**Files:**
- Modify: `crates/frontends/src/anthropic_messages/count_tokens.rs`

- [ ] **Step 1: Write the failing test**

Add inside `mod tests`:

```rust
    #[test]
    fn tool_use_block_counts_name_arguments_plus_overhead() {
        use agent_shim_core::{ids::ToolCallId, tool::ToolCallBlock};
        let mut req = empty_request();
        let args = serde_json::json!({"q": "rust"});
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolCall(ToolCallBlock {
                id: ToolCallId::from_provider("call_1".to_string()),
                name: "search".into(),
                arguments: ToolCallArguments::Complete { value: args.clone() },
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let serialized = serde_json::to_string(&args).unwrap();
        let expected = count_text("search") + count_text(&serialized) + PER_TOOL_USE + PER_MESSAGE;
        assert_eq!(count(&req), expected);
    }

    #[test]
    fn tool_result_block_counts_serialized_content_plus_overhead() {
        use agent_shim_core::{ids::ToolCallId, tool::ToolResultBlock};
        let mut req = empty_request();
        let content = serde_json::json!("the result text");
        req.messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult(ToolResultBlock {
                tool_call_id: ToolCallId::from_provider("call_1".to_string()),
                content: content.clone(),
                is_error: false,
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let serialized = serde_json::to_string(&content).unwrap();
        let expected = count_text(&serialized) + PER_TOOL_RESULT + PER_MESSAGE;
        assert_eq!(count(&req), expected);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p agent-shim-frontends count_tokens::tests::tool_use_block`
Expected: FAIL — current `count_block` returns 0 for ToolCall.

- [ ] **Step 3: Implement tool_use and tool_result counting**

In `count_tokens.rs`, replace the existing `count_block` function with:

```rust
fn count_block(block: &ContentBlock) -> u32 {
    match block {
        ContentBlock::Text(b) => count_text(&b.text),
        ContentBlock::ToolCall(b) => {
            let args_text = match &b.arguments {
                ToolCallArguments::Complete { value } => {
                    serde_json::to_string(value).unwrap_or_default()
                }
                ToolCallArguments::Streaming { data } => data.clone(),
            };
            count_text(&b.name)
                .saturating_add(count_text(&args_text))
                .saturating_add(PER_TOOL_USE)
        }
        ContentBlock::ToolResult(b) => {
            let content_text = serde_json::to_string(&b.content).unwrap_or_default();
            count_text(&content_text).saturating_add(PER_TOOL_RESULT)
        }
        _ => 0, // remaining variants implemented in later tasks
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p agent-shim-frontends count_tokens::tests::tool_use_block count_tokens::tests::tool_result_block`
Expected: both pass.

- [ ] **Step 5: Commit**

```bash
git add crates/frontends/src/anthropic_messages/count_tokens.rs
git commit -m "feat(frontends): count tool_use and tool_result blocks"
```

---

## Task 6: Reasoning, redacted reasoning, and image (unsupported) blocks

**Files:**
- Modify: `crates/frontends/src/anthropic_messages/count_tokens.rs`

- [ ] **Step 1: Write the failing tests**

Add inside `mod tests`:

```rust
    #[test]
    fn reasoning_block_counts_text_plus_overhead() {
        use agent_shim_core::content::ReasoningBlock;
        let mut req = empty_request();
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Reasoning(ReasoningBlock {
                text: "thinking aloud".into(),
                extensions: ExtensionMap::new(), // signature lives here, must NOT be counted
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let expected = count_text("thinking aloud") + PER_REASONING + PER_MESSAGE;
        assert_eq!(count(&req), expected);
    }

    #[test]
    fn redacted_reasoning_counts_blob_length_quarter_plus_overhead() {
        use agent_shim_core::content::RedactedReasoningBlock;
        let mut req = empty_request();
        let blob = "x".repeat(40);
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::RedactedReasoning(RedactedReasoningBlock {
                data: blob.clone(),
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let expected = (blob.len() as u32 / 4) + PER_REASONING + PER_MESSAGE;
        assert_eq!(count(&req), expected);
    }

    #[test]
    fn image_unsupported_block_uses_flat_overhead_only() {
        use agent_shim_core::content::UnsupportedBlock;
        let mut req = empty_request();
        req.messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Unsupported(UnsupportedBlock {
                origin: "anthropic_messages".into(),
                raw: serde_json::json!({"type":"image","source":{"type":"base64","data":"aGVsbG8="}}),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let expected = PER_IMAGE + PER_MESSAGE;
        assert_eq!(count(&req), expected);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p agent-shim-frontends count_tokens::tests::reasoning_block count_tokens::tests::redacted_reasoning count_tokens::tests::image_unsupported`
Expected: all three fail.

- [ ] **Step 3: Add the variants to `count_block`**

Replace `count_block` again — fully expanded form, no `_ => 0` remaining:

```rust
fn count_block(block: &ContentBlock) -> u32 {
    match block {
        ContentBlock::Text(b) => count_text(&b.text),
        ContentBlock::ToolCall(b) => {
            let args_text = match &b.arguments {
                ToolCallArguments::Complete { value } => {
                    serde_json::to_string(value).unwrap_or_default()
                }
                ToolCallArguments::Streaming { data } => data.clone(),
            };
            count_text(&b.name)
                .saturating_add(count_text(&args_text))
                .saturating_add(PER_TOOL_USE)
        }
        ContentBlock::ToolResult(b) => {
            let content_text = serde_json::to_string(&b.content).unwrap_or_default();
            count_text(&content_text).saturating_add(PER_TOOL_RESULT)
        }
        ContentBlock::Reasoning(b) => count_text(&b.text).saturating_add(PER_REASONING),
        ContentBlock::RedactedReasoning(b) => {
            let approx = (b.data.len() as u32) / 4;
            approx.saturating_add(PER_REASONING)
        }
        // Anthropic images decode into Unsupported(...) per
        // anthropic_messages::decode::inbound_block_to_canonical. Use the flat
        // image overhead — we don't have dimensions, and tokenizing the base64
        // blob would over-count by orders of magnitude.
        ContentBlock::Unsupported(_) => PER_IMAGE,
        // Native image/audio/file blocks aren't produced by the Anthropic
        // decoder today, but in case some path produces them, treat them as
        // image-equivalent rather than zero.
        ContentBlock::Image(_) | ContentBlock::Audio(_) | ContentBlock::File(_) => PER_IMAGE,
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p agent-shim-frontends count_tokens::`
Expected: all count_tokens tests pass (8 so far).

- [ ] **Step 5: Commit**

```bash
git add crates/frontends/src/anthropic_messages/count_tokens.rs
git commit -m "feat(frontends): count reasoning, redacted, and image blocks"
```

---

## Task 7: Tool definitions and tool_choice

**Files:**
- Modify: `crates/frontends/src/anthropic_messages/count_tokens.rs`

- [ ] **Step 1: Write the failing tests**

Add inside `mod tests`:

```rust
    #[test]
    fn tool_definition_counts_name_description_schema_plus_overhead() {
        let mut req = empty_request();
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "q": { "type": "string" } },
            "required": ["q"]
        });
        req.tools.push(ToolDefinition {
            name: "search".into(),
            description: Some("find things".into()),
            input_schema: schema.clone(),
            extensions: ExtensionMap::new(),
        });
        let serialized = serde_json::to_string(&schema).unwrap();
        let expected = count_text("search")
            + count_text("find things")
            + count_text(&serialized)
            + PER_TOOL_DEF;
        assert_eq!(count(&req), expected);
    }

    #[test]
    fn tool_definition_with_no_description_does_not_panic() {
        let mut req = empty_request();
        req.tools.push(ToolDefinition {
            name: "ping".into(),
            description: None,
            input_schema: serde_json::json!({}),
            extensions: ExtensionMap::new(),
        });
        // We just care it doesn't panic and includes the tool overhead.
        let n = count(&req);
        assert!(n >= PER_TOOL_DEF);
    }

    #[test]
    fn tool_choice_auto_adds_zero() {
        let mut req = empty_request();
        req.tool_choice = ToolChoice::Auto;
        assert_eq!(count(&req), 0);
    }

    #[test]
    fn tool_choice_required_adds_overhead() {
        let mut req = empty_request();
        req.tool_choice = ToolChoice::Required;
        assert_eq!(count(&req), PER_TOOL_CHOICE);
    }

    #[test]
    fn tool_choice_specific_adds_overhead_plus_tool_name() {
        let mut req = empty_request();
        req.tool_choice = ToolChoice::Specific {
            name: "search".into(),
        };
        let expected = count_text("search") + PER_TOOL_CHOICE;
        assert_eq!(count(&req), expected);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p agent-shim-frontends count_tokens::tests::tool_definition count_tokens::tests::tool_choice`
Expected: all five fail or return 0.

- [ ] **Step 3: Implement `count_tool` and `count_tool_choice`**

Replace the placeholder bodies of `count_tool` and `count_tool_choice` in `count_tokens.rs` with:

```rust
fn count_tool(t: &ToolDefinition) -> u32 {
    let desc = t.description.as_deref().unwrap_or("");
    let schema = serde_json::to_string(&t.input_schema).unwrap_or_default();
    count_text(&t.name)
        .saturating_add(count_text(desc))
        .saturating_add(count_text(&schema))
        .saturating_add(PER_TOOL_DEF)
}

fn count_tool_choice(c: &ToolChoice) -> u32 {
    match c {
        ToolChoice::Auto => 0,
        ToolChoice::None | ToolChoice::Required => PER_TOOL_CHOICE,
        ToolChoice::Specific { name } => count_text(name).saturating_add(PER_TOOL_CHOICE),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p agent-shim-frontends count_tokens::`
Expected: all count_tokens tests pass (13 total).

- [ ] **Step 5: Commit**

```bash
git add crates/frontends/src/anthropic_messages/count_tokens.rs
git commit -m "feat(frontends): count tool definitions and tool_choice"
```

---

## Task 8: Lint and final unit-test pass on the frontends crate

Catch any clippy warnings the new module introduced before moving to the gateway layer.

**Files:** none changed in this task.

- [ ] **Step 1: Run formatter**

Run: `cargo fmt --all`
Expected: no diff produced (file already formatted) or a small whitespace fix. If a fix happened, stage and amend it into a commit at the end of this task.

- [ ] **Step 2: Run clippy on frontends**

Run: `cargo clippy -p agent-shim-frontends --all-targets -- -D warnings`
Expected: no warnings. If clippy flags anything (unused imports, needless borrow, etc.), fix in `count_tokens.rs` / `count_tokens_wire.rs` until clean.

- [ ] **Step 3: Run the full frontends test suite**

Run: `cargo nextest run -p agent-shim-frontends`
Expected: all existing + 13 new count_tokens tests pass.

- [ ] **Step 4: Commit any lint/format fixes**

If steps 1 or 2 produced changes:

```bash
git add -u
git commit -m "chore(frontends): fmt + clippy for count_tokens module"
```

If no changes, skip the commit.

---

## Task 9: Gateway handler

Wire the count function up to an axum POST handler. Mirrors the `anthropic_messages` handler shape but never touches the router or providers.

**Files:**
- Create: `crates/gateway/src/handlers/anthropic_count_tokens.rs`
- Modify: `crates/gateway/src/handlers/mod.rs`
- Modify: `crates/gateway/src/server.rs`

- [ ] **Step 1: Create the handler**

Create `crates/gateway/src/handlers/anthropic_count_tokens.rs`:

```rust
//! `/v1/messages/count_tokens` — local-only token approximation.
//!
//! Decodes an Anthropic-shaped count_tokens body, runs the canonical request
//! through `count_tokens::count`, and returns `{"input_tokens": N}`. Never
//! contacts an upstream provider. See
//! `docs/superpowers/specs/2026-05-06-count-tokens-design.md`.

use axum::{response::IntoResponse, response::Response, Json};
use bytes::Bytes;
use serde::Serialize;

use agent_shim_frontends::anthropic_messages::{
    count_tokens, count_tokens_wire::CountTokensRequest, decode,
};
use agent_shim_frontends::FrontendError;

use super::HandlerError;

#[derive(Debug, Serialize)]
struct CountTokensResponse {
    input_tokens: u32,
}

pub async fn handle(body: Bytes) -> Result<Response, HandlerError> {
    let started = std::time::Instant::now();
    let body_bytes = body.len();

    // Validate the count_tokens-specific shape and pull the model alias for logging.
    let ct_req: CountTokensRequest = serde_json::from_slice(&body)
        .map_err(|e| HandlerError::Frontend(FrontendError::InvalidBody(e.to_string())))?;
    let model_alias = ct_req.model.clone();

    // Re-frame as a MessagesRequest-shaped body by patching `max_tokens=0` if
    // missing, then run the standard decoder. This shares all of decode's
    // validation (role checks, block shape, tool_choice forms) with /v1/messages.
    let mut value: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| HandlerError::Frontend(FrontendError::InvalidBody(e.to_string())))?;
    if let serde_json::Value::Object(map) = &mut value {
        map.entry("max_tokens")
            .or_insert(serde_json::Value::Number(0.into()));
    }
    let normalized = serde_json::to_vec(&value)
        .map_err(|e| HandlerError::Frontend(FrontendError::InvalidBody(e.to_string())))?;

    let canonical = decode::decode(&normalized).map_err(HandlerError::Frontend)?;
    let n = count_tokens::count(&canonical);

    tracing::info!(
        "→ /v1/messages/count_tokens | model: {} | tokens: {} | bodyBytes: {} | {:.3}s",
        model_alias,
        n,
        body_bytes,
        started.elapsed().as_secs_f64()
    );

    Ok(Json(CountTokensResponse { input_tokens: n }).into_response())
}
```

> Why two `serde_json::from_slice` calls? The first proves the count_tokens-specific shape parses (cheap defensive check + gets the model name for the log). The second produces a JSON `Value` we can mutate in place to inject `max_tokens: 0` before calling the existing decoder. The double-parse is trivial cost (under 1ms for typical bodies) and keeps decoder reuse free of branching. The unused `into_messages_request()` method on `CountTokensRequest` (Task 2) stays — it's covered by wire tests and useful for future callers.

- [ ] **Step 2: Register the handler module**

Edit `crates/gateway/src/handlers/mod.rs`. Add the new module right after the existing `pub mod` declarations:

```rust
pub mod anthropic_count_tokens;
pub mod anthropic_messages;
pub mod openai_chat;
pub mod openai_responses;
```

- [ ] **Step 3: Mount the route**

Edit `crates/gateway/src/server.rs`. In `build_router`, add one line after the `/v1/messages` route:

```rust
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(healthz))
        .route("/healthz", get(healthz))
        .route("/v1/messages", post(handlers::anthropic_messages::handle))
        .route(
            "/v1/messages/count_tokens",
            post(handlers::anthropic_count_tokens::handle),
        )
        .route("/v1/chat/completions", post(handlers::openai_chat::handle))
        .route("/v1/responses", post(handlers::openai_responses::handle))
        .layer(TraceLayer::new_for_http())
        .layer(RequestIdLayer)
        .with_state(state)
}
```

- [ ] **Step 4: Build and lint**

Run in parallel:
- `cargo build -p agent-shim`
- `cargo clippy -p agent-shim --all-targets -- -D warnings`

Expected: both clean.

- [ ] **Step 5: Commit**

```bash
git add crates/gateway/src/handlers/anthropic_count_tokens.rs \
         crates/gateway/src/handlers/mod.rs \
         crates/gateway/src/server.rs
git commit -m "feat(gateway): mount POST /v1/messages/count_tokens"
```

---

## Task 10: Integration smoke tests

Real router on an ephemeral port, real HTTP via `reqwest`. Mirrors the existing `healthz.rs` test pattern.

**Files:**
- Create: `crates/gateway/tests/count_tokens_smoke.rs`

- [ ] **Step 1: Write the failing happy-path test**

Create `crates/gateway/tests/count_tokens_smoke.rs`:

```rust
//! Integration tests for POST /v1/messages/count_tokens.
//!
//! Validates the HTTP contract end-to-end: response shape, status codes,
//! and behavior on the captured Claude Desktop preflight body.

use agent_shim_config::GatewayConfig;
use agent_shim_gateway::{server::run_on_listener, state::AppState};
use std::collections::BTreeMap;
use tokio::net::TcpListener;

fn minimal_config() -> GatewayConfig {
    use agent_shim_config::schema::{LoggingConfig, ServerConfig};
    GatewayConfig {
        server: ServerConfig::default(),
        logging: LoggingConfig::default(),
        upstreams: BTreeMap::new(),
        routes: vec![],
        copilot: None,
    }
}

/// Spawn the server on an ephemeral port and return (base_url, shutdown_tx).
async fn spawn_server() -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::new(minimal_config()).await;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        run_on_listener(listener, state, async {
            let _ = rx.await;
        })
        .await
        .unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (format!("http://{}", addr), tx)
}

#[tokio::test]
async fn happy_path_returns_input_tokens() {
    let (base, shutdown) = spawn_server().await;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "claude-opus-4-7",
        "messages": [{"role":"user","content":"hello"}]
    });
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", base))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    let n = json.get("input_tokens").and_then(|v| v.as_u64()).unwrap();
    assert!(n > 0, "input_tokens should be positive, got {}", n);
    let _ = shutdown.send(());
}
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo nextest run -p agent-shim --test count_tokens_smoke happy_path`
Expected: pass.

(The test should pass on first run since Task 9 already mounted the route. If it fails, that's a sign Task 9 has a bug — fix before continuing.)

- [ ] **Step 3: Add the Claude Desktop replay regression guard**

Append to `crates/gateway/tests/count_tokens_smoke.rs`:

```rust
#[tokio::test]
async fn claude_desktop_replay_returns_200() {
    // Exact body captured in claudeDesktop.pcapng — preflight from
    // agent-sdk/0.2.121. Before the fix this 404'd and Claude Desktop
    // never proceeded.
    let captured = r#"{"model":"claude-opus-4-7","messages":[{"role":"user","content":"a331d0994e97:build-perf Agent for diagnosing and optimizing MSBuild build performance. Runs multi-step analysis: generates binlogs, analyzes timeline and bottlenecks, identifies expensive targets/tasks/analyzers, and suggests concrete optimizations. Invoke when builds are slow or when asked to optimize build times."}],"tools":[]}"#;

    let (base, shutdown) = spawn_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages/count_tokens?beta=true", base))
        .header("content-type", "application/json")
        .header("x-api-key", "agent shim") // exact header Claude Desktop sent, with the space
        .header("anthropic-version", "2023-06-01")
        .header(
            "anthropic-beta",
            "claude-code-20250219,interleaved-thinking-2025-05-14,token-counting-2024-11-01",
        )
        .body(captured.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "Claude Desktop preflight must succeed");
    let json: serde_json::Value = resp.json().await.unwrap();
    let n = json.get("input_tokens").and_then(|v| v.as_u64()).unwrap();
    assert!(n > 0);
    let _ = shutdown.send(());
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p agent-shim --test count_tokens_smoke claude_desktop_replay`
Expected: pass.

- [ ] **Step 5: Add error-path tests**

Append to `count_tokens_smoke.rs`:

```rust
#[tokio::test]
async fn malformed_body_returns_400() {
    let (base, shutdown) = spawn_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", base))
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert!(json.get("error").is_some());
    let _ = shutdown.send(());
}

#[tokio::test]
async fn unknown_role_returns_400() {
    let (base, shutdown) = spawn_server().await;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "m",
        "messages": [{"role":"system","content":"hi"}]  // 'system' is not a valid Anthropic role
    });
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", base))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let _ = shutdown.send(());
}

#[tokio::test]
async fn no_auth_headers_still_returns_200() {
    // The endpoint must not require x-api-key or Authorization.
    let (base, shutdown) = spawn_server().await;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "m",
        "messages": [{"role":"user","content":"hi"}]
    });
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", base))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = shutdown.send(());
}

#[tokio::test]
async fn beta_query_string_is_ignored() {
    let (base, shutdown) = spawn_server().await;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "m",
        "messages": [{"role":"user","content":"hi"}]
    });
    let with_beta = client
        .post(format!("{}/v1/messages/count_tokens?beta=true", base))
        .json(&body)
        .send()
        .await
        .unwrap();
    let without_beta = client
        .post(format!("{}/v1/messages/count_tokens", base))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(with_beta.status(), 200);
    assert_eq!(without_beta.status(), 200);
    let a: serde_json::Value = with_beta.json().await.unwrap();
    let b: serde_json::Value = without_beta.json().await.unwrap();
    assert_eq!(a, b, "?beta=true must not change the response");
    let _ = shutdown.send(());
}
```

- [ ] **Step 6: Run all integration tests**

Run: `cargo nextest run -p agent-shim --test count_tokens_smoke`
Expected: 6 tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/gateway/tests/count_tokens_smoke.rs
git commit -m "test(gateway): integration smoke tests for /v1/messages/count_tokens"
```

---

## Task 11: Workspace-wide green build

Final guard before manual verification.

- [ ] **Step 1: Format check**

Run: `cargo fmt --all -- --check`
Expected: no output (clean). If it fails, run `cargo fmt --all`, stage, and commit:

```bash
git add -u
git commit -m "chore: cargo fmt"
```

- [ ] **Step 2: Workspace clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 3: Workspace tests**

Run: `cargo nextest run --workspace`
Expected: all tests pass, including the 13 new unit tests in `count_tokens` and the 6 new integration tests.

- [ ] **Step 4: Cargo deny**

Run: `cargo deny check`
Expected: no advisories or license issues. (`tiktoken-rs` is MIT-licensed and stable; this should be clean.)

If `cargo deny` flags a license / advisory issue on `tiktoken-rs`'s transitive deps, document it in the PR description and consult before pinning a different version.

---

## Task 12: Manual verification

Replay the original failure mode to confirm the fix.

- [ ] **Step 1: Build the release binary**

Run: `cargo build --release -p agent-shim`
Expected: success.

- [ ] **Step 2: Start the server with the example config**

Run in a terminal that you'll keep open:

```bash
./target/release/agent-shim serve --config config/gateway.example.yaml
```

Expected stderr:
```
Listening on 127.0.0.1:8787
```

- [ ] **Step 3: Replay the captured preflight with curl**

In another terminal:

```bash
curl -i -X POST 'http://127.0.0.1:8787/v1/messages/count_tokens?beta=true' \
  -H 'Content-Type: application/json' \
  -H 'x-api-key: agent shim' \
  -H 'anthropic-version: 2023-06-01' \
  -H 'anthropic-beta: token-counting-2024-11-01' \
  -d '{"model":"claude-opus-4-7","messages":[{"role":"user","content":"hello"}],"tools":[]}'
```

Expected:
- `HTTP/1.1 200 OK`
- `content-type: application/json`
- Body shape `{"input_tokens": <small positive integer>}`
- A log line on the server stderr starting with `→ /v1/messages/count_tokens | model: claude-opus-4-7 | tokens: ... | bodyBytes: ...`

- [ ] **Step 4: Replay the original Claude Desktop body**

Save the body from `claudeDesktop.pcapng` to a temp file, then:

```bash
curl -i -X POST 'http://127.0.0.1:8787/v1/messages/count_tokens?beta=true' \
  -H 'Content-Type: application/json' \
  --data-binary @/tmp/captured.json
```

Expected: same shape, status 200.

- [ ] **Step 5: Live Claude Desktop test**

Configure Claude Desktop to point at `http://127.0.0.1:8787` (per your usual Claude Desktop → AgentShim setup). Start a packet capture on the loopback interface again.

Send any prompt. Stop the capture. Open it.

Expected:
- A 200 OK response to `POST /v1/messages/count_tokens?beta=true` (this used to be a missing 404).
- A subsequent `POST /v1/messages` request from Claude Desktop, which proceeds through the configured route to whatever upstream is wired up.

If that second `/v1/messages` call fails for *other* reasons (e.g. the model alias `claude-opus-4-7` not configured in routes, the empty `Authorization: Bearer` header confusing a strict upstream, etc.), those are downstream issues separate from this plan. File them as new bugs.

- [ ] **Step 6: Stop the server**

`Ctrl-C` in the server terminal.

---

## Spec coverage check

| Spec section | Implemented in |
|---|---|
| Local approximation everywhere | Tasks 3-7 (no upstream call anywhere) |
| Single tokenizer cl100k_base via OnceLock | Task 3 |
| Frontend-shaped placement | Tasks 2-3 (in `crates/frontends/...`) |
| Route `POST /v1/messages/count_tokens` | Task 9 step 4 |
| `?beta=true` ignored | Task 10 (`beta_query_string_is_ignored`) |
| `CountTokensRequest` with optional `max_tokens` | Task 2 |
| Decoded fields walked: model/system/messages/tools/tool_choice | Tasks 3-7 |
| Auth headers ignored | Task 9 (handler does not read headers) + Task 10 (`no_auth_headers_still_returns_200`) |
| Response shape `{"input_tokens": N}` | Task 9 step 2 |
| Error mapping reuses `HandlerError → IntoResponse` | Task 9 step 2 (uses `HandlerError::Frontend(FrontendError::InvalidBody)`) |
| All structural-overhead constants | Task 3 (declared) + Tasks 5-7 (used) |
| Per-block tokenization rules | Tasks 5-6 |
| Tool definition counting | Task 7 |
| `tool_choice` counting (Auto=0, others=overhead, Specific adds name) | Task 7 |
| Saturating u32 accumulator | Task 3 (all `count_*` functions use `saturating_add`) |
| Image block as flat +200 | Task 6 |
| Naming bridge (wire `thinking` → canonical `Reasoning`) | Task 6 (test uses canonical names; decoder mapping is unchanged) |
| Unit tests | Tasks 3-7 (13 tests inline) |
| Integration tests | Task 10 (6 tests) |
| Manual verification | Task 12 |
| CI runs under `cargo nextest run --workspace` | Task 11 step 3 |
