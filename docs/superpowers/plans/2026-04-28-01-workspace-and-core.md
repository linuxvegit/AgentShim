# Plan 01 — Workspace Bootstrap + `core` Crate

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the Cargo workspace and build the leaf `core` crate (canonical data model, IDs, error types, capabilities, stream events) with unit + property test coverage.

**Architecture:** Pure-Rust workspace, zero I/O in `core`. Spec Section 4 (Canonical Data Model) is the contract. Every other crate depends on `core`. Property tests guarantee encode/decode round-trips will be possible later.

**Tech Stack:** Rust (stable), `serde`, `serde_json` (with `RawValue`), `bytes`, `thiserror`, `uuid`, `proptest`, `cargo nextest`.

---

## File Structure

Workspace root:
- Create: `Cargo.toml` (workspace manifest)
- Create: `rust-toolchain.toml`
- Create: `rustfmt.toml`
- Create: `clippy.toml`
- Create: `deny.toml`
- Create: `.gitignore`
- Create: `LICENSE` (MIT)
- Create: `README.md` (one-paragraph stub)

`crates/core/`:
- Create: `crates/core/Cargo.toml`
- Create: `crates/core/src/lib.rs` — re-exports
- Create: `crates/core/src/ids.rs` — `RequestId`, `ResponseId`, `ToolCallId`
- Create: `crates/core/src/error.rs` — `CoreError`, `StreamError`
- Create: `crates/core/src/extensions.rs` — `ExtensionMap` newtype
- Create: `crates/core/src/capabilities.rs` — `ProviderCapabilities`
- Create: `crates/core/src/target.rs` — `BackendTarget`, `FrontendInfo`, `FrontendModel`
- Create: `crates/core/src/media.rs` — `BinarySource`
- Create: `crates/core/src/tool.rs` — `ToolDefinition`, `ToolChoice`, `ToolCallBlock`, `ToolCallArguments`, `ToolResultBlock`
- Create: `crates/core/src/content.rs` — `ContentBlock`, `TextBlock`, `ImageBlock`, etc.
- Create: `crates/core/src/message.rs` — `Message`, `MessageRole`, `SystemInstruction`, `SystemSource`
- Create: `crates/core/src/usage.rs` — `Usage`, `StopReason`
- Create: `crates/core/src/stream.rs` — `StreamEvent`, `ContentBlockKind`, `RawProviderEvent`, `CanonicalStream` type alias
- Create: `crates/core/src/request.rs` — `CanonicalRequest`, `GenerationOptions`, `ResponseFormat`, `RequestMetadata`
- Create: `crates/core/src/response.rs` — `CanonicalResponse` (collected stream)
- Create: `crates/core/tests/proptest_roundtrip.rs` — proptest suite (serde JSON round-trip)

---

## Task 1: Workspace skeleton

**Files:**
- Create: `Cargo.toml`
- Create: `rust-toolchain.toml`
- Create: `rustfmt.toml`
- Create: `clippy.toml`
- Create: `deny.toml`
- Create: `.gitignore`
- Create: `LICENSE`
- Create: `README.md`

- [ ] **Step 1: Create workspace `Cargo.toml`**

```toml
[workspace]
resolver = "2"
members = ["crates/core"]

[workspace.package]
version = "0.1.0"
edition = "2021"
rust-version = "1.80"
license = "MIT"
repository = "https://github.com/anthropics/agent-shim"

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = { version = "1", features = ["raw_value", "preserve_order"] }
thiserror = "1"
bytes = "1"
uuid = { version = "1", features = ["v4", "serde"] }
proptest = "1"
```

- [ ] **Step 2: Pin toolchain**

`rust-toolchain.toml`:

```toml
[toolchain]
channel = "1.82.0"
components = ["rustfmt", "clippy", "llvm-tools-preview"]
```

- [ ] **Step 3: Add formatter / linter / deny configs**

`rustfmt.toml`:

```toml
edition = "2021"
max_width = 100
imports_granularity = "Crate"
group_imports = "StdExternalCrate"
```

`clippy.toml`:

```toml
avoid-breaking-exported-api = false
```

`deny.toml`:

```toml
[advisories]
yanked = "deny"
unmaintained = "warn"

[licenses]
allow = ["MIT", "Apache-2.0", "BSD-3-Clause", "BSD-2-Clause", "ISC", "Unicode-DFS-2016", "Zlib"]
confidence-threshold = 0.9

[bans]
multiple-versions = "warn"

[sources]
unknown-registry = "deny"
unknown-git = "deny"
```

- [ ] **Step 4: `.gitignore`, `LICENSE`, `README`**

`.gitignore`:

```
/target
**/*.rs.bk
.idea/
.vscode/
*.iml
.DS_Store
```

`LICENSE` — standard MIT text with copyright `2026 AgentShim contributors`.

`README.md`:

```markdown
# AgentShim

A Rust gateway that lets any AI coding agent talk to any LLM backend. See `docs/Requirements.md` and `docs/superpowers/specs/`.
```

- [ ] **Step 5: Verify workspace parses**

Run: `cargo metadata --no-deps --format-version 1 > /dev/null`
Expected: exits 0 (workspace is empty of members but parses).

- [ ] **Step 6: Commit**

```bash
git init
git add .
git commit -m "chore: bootstrap workspace skeleton"
```

---

## Task 2: `core` crate skeleton + `ids` module

**Files:**
- Create: `crates/core/Cargo.toml`
- Create: `crates/core/src/lib.rs`
- Create: `crates/core/src/ids.rs`

- [ ] **Step 1: Write the failing test**

`crates/core/src/ids.rs`:

```rust
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub String);

impl RequestId {
    pub fn new() -> Self { Self(format!("req_{}", Uuid::new_v4().simple())) }
}

impl Default for RequestId { fn default() -> Self { Self::new() } }

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResponseId(pub String);

impl ResponseId {
    pub fn new() -> Self { Self(format!("resp_{}", Uuid::new_v4().simple())) }
}

impl Default for ResponseId { fn default() -> Self { Self::new() } }

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolCallId(pub String);

impl ToolCallId {
    pub fn new() -> Self { Self(format!("call_{}", Uuid::new_v4().simple())) }
    pub fn from_provider(s: impl Into<String>) -> Self { Self(s.into()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_has_prefix_and_is_unique() {
        let a = RequestId::new();
        let b = RequestId::new();
        assert!(a.0.starts_with("req_"));
        assert_ne!(a, b);
    }

    #[test]
    fn tool_call_id_round_trips_as_string() {
        let id = ToolCallId::from_provider("call_abc");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"call_abc\"");
        let back: ToolCallId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }
}
```

- [ ] **Step 2: Add `core/Cargo.toml`**

```toml
[package]
name = "agent-shim-core"
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
name = "agent_shim_core"
path = "src/lib.rs"

[dependencies]
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
bytes.workspace = true
uuid.workspace = true

[dev-dependencies]
proptest.workspace = true
```

Update root `Cargo.toml` `members = ["crates/core"]` (already present).

- [ ] **Step 3: Minimal `lib.rs`**

`crates/core/src/lib.rs`:

```rust
#![forbid(unsafe_code)]

pub mod ids;

pub use ids::{RequestId, ResponseId, ToolCallId};
```

- [ ] **Step 4: Run tests — should pass**

Run: `cargo test -p agent-shim-core ids`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/core Cargo.toml
git commit -m "feat(core): add ID newtypes with serde transparent serialization"
```

---

## Task 3: `extensions` module

**Files:**
- Create: `crates/core/src/extensions.rs`
- Modify: `crates/core/src/lib.rs`

- [ ] **Step 1: Write the failing test**

`crates/core/src/extensions.rs`:

```rust
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExtensionMap(pub BTreeMap<String, serde_json::Value>);

impl ExtensionMap {
    pub fn new() -> Self { Self::default() }
    pub fn is_empty(&self) -> bool { self.0.is_empty() }
    pub fn insert(&mut self, key: impl Into<String>, value: serde_json::Value) {
        self.0.insert(key.into(), value);
    }
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> { self.0.get(key) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_as_plain_object() {
        let mut ext = ExtensionMap::new();
        ext.insert("anthropic.cache_control", json!({ "type": "ephemeral" }));
        let s = serde_json::to_string(&ext).unwrap();
        assert_eq!(s, r#"{"anthropic.cache_control":{"type":"ephemeral"}}"#);
        let back: ExtensionMap = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ext);
    }

    #[test]
    fn empty_serializes_as_empty_object() {
        let ext = ExtensionMap::new();
        assert_eq!(serde_json::to_string(&ext).unwrap(), "{}");
    }
}
```

- [ ] **Step 2: Re-export from `lib.rs`**

Append to `crates/core/src/lib.rs`:

```rust
pub mod extensions;
pub use extensions::ExtensionMap;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p agent-shim-core extensions`
Expected: 2 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/core
git commit -m "feat(core): add ExtensionMap newtype"
```

---

## Task 4: `error` module

**Files:**
- Create: `crates/core/src/error.rs`
- Modify: `crates/core/src/lib.rs`

- [ ] **Step 1: Write the failing test + implementation**

`crates/core/src/error.rs`:

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid model alias: {0}")]
    InvalidModel(String),
    #[error("capability mismatch: {0}")]
    CapabilityMismatch(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

#[derive(Debug, Error)]
pub enum StreamError {
    #[error("upstream error: {0}")]
    Upstream(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("encode error: {0}")]
    Encode(String),
    #[error("client disconnected")]
    ClientDisconnected,
    #[error("timeout after {0:?}")]
    Timeout(std::time::Duration),
}

impl StreamError {
    pub fn is_retryable_pre_first_byte(&self) -> bool {
        matches!(self, StreamError::Upstream(_) | StreamError::Timeout(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_are_useful() {
        let e = CoreError::InvalidModel("foo".into());
        assert_eq!(e.to_string(), "invalid model alias: foo");
    }

    #[test]
    fn disconnected_is_not_retryable() {
        assert!(!StreamError::ClientDisconnected.is_retryable_pre_first_byte());
    }

    #[test]
    fn upstream_is_retryable_pre_first_byte() {
        assert!(StreamError::Upstream("503".into()).is_retryable_pre_first_byte());
    }
}
```

- [ ] **Step 2: Re-export**

Append to `lib.rs`:

```rust
pub mod error;
pub use error::{CoreError, StreamError};
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p agent-shim-core error`
Expected: 3 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/core
git commit -m "feat(core): add CoreError and StreamError"
```

---

## Task 5: `capabilities` module

**Files:**
- Create: `crates/core/src/capabilities.rs`
- Modify: `crates/core/src/lib.rs`

- [ ] **Step 1: Write the implementation + tests**

`crates/core/src/capabilities.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub streaming: bool,
    pub tool_calling: bool,
    pub vision: bool,
    pub audio: bool,
    pub reasoning: bool,
    pub json_mode: bool,
    pub json_schema: bool,
    pub system_prompts: bool,
    pub developer_prompts: bool,
    /// `None` means "discovered dynamically, unknown statically".
    pub available_models: Option<Vec<String>>,
}

impl ProviderCapabilities {
    pub fn supports_model(&self, model: &str) -> bool {
        match &self.available_models {
            None => true,
            Some(list) => list.iter().any(|m| m == model),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_models_supported_when_dynamic() {
        let caps = ProviderCapabilities { available_models: None, ..Default::default() };
        assert!(caps.supports_model("anything"));
    }

    #[test]
    fn static_list_rejects_unknown_models() {
        let caps = ProviderCapabilities {
            available_models: Some(vec!["gpt-4o".into()]),
            ..Default::default()
        };
        assert!(caps.supports_model("gpt-4o"));
        assert!(!caps.supports_model("gpt-5"));
    }
}
```

- [ ] **Step 2: Re-export, run tests, commit**

Append `pub mod capabilities; pub use capabilities::ProviderCapabilities;` to `lib.rs`.

Run: `cargo test -p agent-shim-core capabilities`
Expected: 2 passed.

```bash
git add crates/core
git commit -m "feat(core): add ProviderCapabilities"
```

---

## Task 6: `target` module — `BackendTarget`, `FrontendInfo`, `FrontendModel`

**Files:**
- Create: `crates/core/src/target.rs`
- Modify: `crates/core/src/lib.rs`

- [ ] **Step 1: Write the implementation + tests**

`crates/core/src/target.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FrontendModel(pub String);

impl<S: Into<String>> From<S> for FrontendModel {
    fn from(s: S) -> Self { Self(s.into()) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendKind {
    AnthropicMessages,
    OpenAiChat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendInfo {
    pub kind: FrontendKind,
    pub api_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendTarget {
    /// Provider name (e.g. "openai_compatible", "github_copilot").
    pub provider: String,
    /// Provider-side model name (after alias resolution).
    pub upstream_model: String,
    /// Optional named upstream config (e.g. "deepseek", "kimi").
    pub upstream: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontend_kind_serializes_snake_case() {
        let s = serde_json::to_string(&FrontendKind::AnthropicMessages).unwrap();
        assert_eq!(s, "\"anthropic_messages\"");
    }

    #[test]
    fn backend_target_round_trips() {
        let t = BackendTarget {
            provider: "openai_compatible".into(),
            upstream_model: "deepseek-chat".into(),
            upstream: Some("deepseek".into()),
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: BackendTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }
}
```

- [ ] **Step 2: Re-export, run tests, commit**

Append to `lib.rs`: `pub mod target; pub use target::{BackendTarget, FrontendInfo, FrontendKind, FrontendModel};`

Run: `cargo test -p agent-shim-core target`
Expected: 2 passed.

```bash
git add crates/core
git commit -m "feat(core): add FrontendInfo and BackendTarget types"
```

---

## Task 7: `media` module — `BinarySource`

**Files:**
- Create: `crates/core/src/media.rs`
- Modify: `crates/core/src/lib.rs`

- [ ] **Step 1: Implementation + tests**

`crates/core/src/media.rs`:

```rust
use bytes::Bytes;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BinarySource {
    Url { url: String },
    Base64 { mime: String, data: String },
    Bytes {
        mime: String,
        #[serde(with = "bytes_base64")]
        data: Bytes,
    },
    ProviderFileId { provider: String, id: String },
}

mod bytes_base64 {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use bytes::Bytes;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(bytes: &Bytes, ser: S) -> Result<S::Ok, S::Error> {
        STANDARD.encode(bytes).serialize(ser)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Bytes, D::Error> {
        let s = String::deserialize(de)?;
        STANDARD.decode(&s).map(Bytes::from).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_variant_round_trips() {
        let src = BinarySource::Url { url: "https://example/x.png".into() };
        let json = serde_json::to_string(&src).unwrap();
        assert!(json.contains("\"kind\":\"url\""));
        let back: BinarySource = serde_json::from_str(&json).unwrap();
        assert_eq!(back, src);
    }

    #[test]
    fn bytes_variant_base64_round_trips() {
        let src = BinarySource::Bytes { mime: "image/png".into(), data: Bytes::from_static(&[1, 2, 3, 4]) };
        let json = serde_json::to_string(&src).unwrap();
        let back: BinarySource = serde_json::from_str(&json).unwrap();
        assert_eq!(back, src);
    }
}
```

- [ ] **Step 2: Add `base64` dep**

In `crates/core/Cargo.toml` `[dependencies]` add: `base64 = "0.22"`

- [ ] **Step 3: Re-export, run tests, commit**

Append `pub mod media; pub use media::BinarySource;` to `lib.rs`.

Run: `cargo test -p agent-shim-core media`
Expected: 2 passed.

```bash
git add crates/core
git commit -m "feat(core): add BinarySource with base64 byte serialization"
```

---

## Task 8: `tool` module — tool definitions, calls, results

**Files:**
- Create: `crates/core/src/tool.rs`
- Modify: `crates/core/src/lib.rs`

- [ ] **Step 1: Write the implementation**

`crates/core/src/tool.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::extensions::ExtensionMap;
use crate::ids::ToolCallId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: Option<String>,
    /// JSON Schema for the tool's input.
    pub input_schema: serde_json::Value,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Specific { name: String },
}

impl Default for ToolChoice {
    fn default() -> Self { Self::Auto }
}

/// Argument representation. Streaming uses `RawValue` to preserve byte fidelity
/// (e.g. `6.0` vs `6`) until all fragments are joined.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolCallArguments {
    Complete(serde_json::Value),
    Streaming(Box<RawValue>),
}

impl PartialEq for ToolCallArguments {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Complete(a), Self::Complete(b)) => a == b,
            (Self::Streaming(a), Self::Streaming(b)) => a.get() == b.get(),
            _ => false,
        }
    }
}

impl ToolCallArguments {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Complete(v) => {
                // Cheap: only used in tests; production path uses Streaming.
                Box::leak(serde_json::to_string(v).unwrap().into_boxed_str())
            }
            Self::Streaming(r) => r.get(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallBlock {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: ToolCallArguments,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultBlock {
    pub tool_call_id: ToolCallId,
    /// Content can be text, images, etc. — declared as JSON to avoid a circular
    /// type with `ContentBlock`; concrete shape is `Vec<ContentBlock>` enforced
    /// at the request-builder layer.
    pub content: serde_json::Value,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, value::to_raw_value};

    #[test]
    fn tool_choice_default_is_auto() {
        assert_eq!(ToolChoice::default(), ToolChoice::Auto);
    }

    #[test]
    fn tool_choice_specific_round_trips() {
        let c = ToolChoice::Specific { name: "search".into() };
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(s, r#"{"kind":"specific","name":"search"}"#);
        let back: ToolChoice = serde_json::from_str(&s).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn streaming_args_preserve_raw_bytes() {
        let raw = to_raw_value(&json!({ "n": 6.0 })).unwrap();
        let args = ToolCallArguments::Streaming(raw);
        // The raw form keeps the trailing `.0`.
        assert!(args.as_str().contains("6.0"));
    }
}
```

- [ ] **Step 2: Re-export, run tests, commit**

Append `pub mod tool; pub use tool::*;` to `lib.rs`.

Run: `cargo test -p agent-shim-core tool`
Expected: 3 passed.

```bash
git add crates/core
git commit -m "feat(core): add tool definitions, calls, results with raw arg preservation"
```

---

## Task 9: `content` module

**Files:**
- Create: `crates/core/src/content.rs`
- Modify: `crates/core/src/lib.rs`

- [ ] **Step 1: Implementation**

`crates/core/src/content.rs`:

```rust
use serde::{Deserialize, Serialize};

use crate::extensions::ExtensionMap;
use crate::media::BinarySource;
use crate::tool::{ToolCallBlock, ToolResultBlock};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextBlock {
    pub text: String,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageBlock {
    pub source: BinarySource,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioBlock {
    pub source: BinarySource,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileBlock {
    pub source: BinarySource,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningBlock {
    pub text: String,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedactedReasoningBlock {
    pub data: String,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnsupportedBlock {
    /// Origin protocol (e.g. "anthropic", "openai").
    pub origin: String,
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text(TextBlock),
    Image(ImageBlock),
    Audio(AudioBlock),
    File(FileBlock),
    ToolCall(ToolCallBlock),
    ToolResult(ToolResultBlock),
    Reasoning(ReasoningBlock),
    RedactedReasoning(RedactedReasoningBlock),
    Unsupported(UnsupportedBlock),
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(TextBlock { text: s.into(), extensions: ExtensionMap::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_block_serializes_with_type_tag() {
        let b = ContentBlock::text("hi");
        let s = serde_json::to_string(&b).unwrap();
        assert!(s.contains("\"type\":\"text\""));
        assert!(s.contains("\"text\":\"hi\""));
    }

    #[test]
    fn round_trip_preserves_variant() {
        let cases = vec![
            ContentBlock::text("hello"),
            ContentBlock::Reasoning(ReasoningBlock { text: "thinking".into(), extensions: Default::default() }),
        ];
        for c in cases {
            let s = serde_json::to_string(&c).unwrap();
            let back: ContentBlock = serde_json::from_str(&s).unwrap();
            assert_eq!(back, c);
        }
    }
}
```

- [ ] **Step 2: Re-export, run tests, commit**

Append `pub mod content; pub use content::*;` to `lib.rs`.

Run: `cargo test -p agent-shim-core content`
Expected: 2 passed.

```bash
git add crates/core
git commit -m "feat(core): add ContentBlock enum and concrete block types"
```

---

## Task 10: `message` module — `Message`, `SystemInstruction`

**Files:**
- Create: `crates/core/src/message.rs`
- Modify: `crates/core/src/lib.rs`

- [ ] **Step 1: Implementation**

`crates/core/src/message.rs`:

```rust
use serde::{Deserialize, Serialize};

use crate::content::ContentBlock;
use crate::extensions::ExtensionMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemSource {
    AnthropicSystem,
    OpenAiSystem,
    OpenAiDeveloper,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemInstruction {
    pub source: SystemSource,
    pub content: Vec<ContentBlock>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentBlock;

    #[test]
    fn role_serializes_lowercase() {
        let s = serde_json::to_string(&MessageRole::Assistant).unwrap();
        assert_eq!(s, "\"assistant\"");
    }

    #[test]
    fn message_round_trips() {
        let m = Message {
            role: MessageRole::User,
            content: vec![ContentBlock::text("hi")],
            name: None,
            extensions: Default::default(),
        };
        let s = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&s).unwrap();
        assert_eq!(back, m);
    }
}
```

- [ ] **Step 2: Re-export, run tests, commit**

Append `pub mod message; pub use message::*;` to `lib.rs`.

Run: `cargo test -p agent-shim-core message`
Expected: 2 passed.

```bash
git add crates/core
git commit -m "feat(core): add Message and SystemInstruction"
```

---

## Task 11: `usage` module — `Usage`, `StopReason`

**Files:**
- Create: `crates/core/src/usage.rs`
- Modify: `crates/core/src/lib.rs`

- [ ] **Step 1: Implementation**

`crates/core/src/usage.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
    pub reasoning_tokens: Option<u32>,
    #[serde(default)]
    pub estimated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_raw: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    ContentFilter,
    Refusal,
    Error,
    Unknown { value: String },
}

impl StopReason {
    pub fn from_provider_string(s: &str) -> Self {
        match s {
            "stop" | "end_turn" => Self::EndTurn,
            "length" | "max_tokens" => Self::MaxTokens,
            "stop_sequence" => Self::StopSequence,
            "tool_calls" | "tool_use" => Self::ToolUse,
            "content_filter" => Self::ContentFilter,
            "refusal" => Self::Refusal,
            "error" => Self::Error,
            other => Self::Unknown { value: other.into() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_normalizes_known_values() {
        assert_eq!(StopReason::from_provider_string("stop"), StopReason::EndTurn);
        assert_eq!(StopReason::from_provider_string("tool_calls"), StopReason::ToolUse);
        assert_eq!(StopReason::from_provider_string("length"), StopReason::MaxTokens);
    }

    #[test]
    fn stop_reason_preserves_unknown() {
        assert_eq!(
            StopReason::from_provider_string("weird"),
            StopReason::Unknown { value: "weird".into() }
        );
    }

    #[test]
    fn usage_round_trips() {
        let u = Usage { input_tokens: Some(10), output_tokens: Some(5), ..Default::default() };
        let json = serde_json::to_string(&u).unwrap();
        let back: Usage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, u);
    }
}
```

- [ ] **Step 2: Re-export, run tests, commit**

Append `pub mod usage; pub use usage::*;` to `lib.rs`.

Run: `cargo test -p agent-shim-core usage`
Expected: 3 passed.

```bash
git add crates/core
git commit -m "feat(core): add Usage and StopReason with provider normalization"
```

---

## Task 12: `stream` module — `StreamEvent`, `CanonicalStream`

**Files:**
- Create: `crates/core/src/stream.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/core/Cargo.toml` (add `futures-core`)

- [ ] **Step 1: Add `futures-core` dep**

In `crates/core/Cargo.toml`:

```toml
futures-core = "0.3"
```

- [ ] **Step 2: Implementation**

`crates/core/src/stream.rs`:

```rust
use std::pin::Pin;

use futures_core::Stream;
use serde::{Deserialize, Serialize};

use crate::error::StreamError;
use crate::ids::{ResponseId, ToolCallId};
use crate::message::MessageRole;
use crate::usage::{StopReason, Usage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentBlockKind {
    Text,
    ToolCall,
    Reasoning,
    RedactedReasoning,
    Image,
    Audio,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawProviderEvent {
    pub provider: String,
    pub event_name: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    ResponseStart { id: ResponseId, model: String, created_at_unix: u64 },
    MessageStart { role: MessageRole },
    ContentBlockStart { index: u32, kind: ContentBlockKind },
    TextDelta { index: u32, text: String },
    ReasoningDelta { index: u32, text: String },
    ToolCallStart { index: u32, id: ToolCallId, name: String },
    ToolCallArgumentsDelta { index: u32, json_fragment: String },
    ToolCallStop { index: u32 },
    ContentBlockStop { index: u32 },
    UsageDelta { usage: Usage },
    MessageStop { stop_reason: StopReason, stop_sequence: Option<String> },
    ResponseStop { usage: Option<Usage> },
    Error { message: String },
    RawProviderEvent(RawProviderEvent),
}

pub type CanonicalStream =
    Pin<Box<dyn Stream<Item = Result<StreamEvent, StreamError>> + Send>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_delta_serializes_with_type_tag() {
        let e = StreamEvent::TextDelta { index: 0, text: "hi".into() };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"type\":\"text_delta\""));
    }

    #[test]
    fn round_trip_preserves_variant() {
        let e = StreamEvent::ToolCallArgumentsDelta { index: 1, json_fragment: r#"{"a":"#.into() };
        let s = serde_json::to_string(&e).unwrap();
        let back: StreamEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }
}
```

- [ ] **Step 3: Re-export, run tests, commit**

Append `pub mod stream; pub use stream::*;` to `lib.rs`.

Run: `cargo test -p agent-shim-core stream`
Expected: 2 passed.

```bash
git add crates/core
git commit -m "feat(core): add StreamEvent, ContentBlockKind, CanonicalStream alias"
```

---

## Task 13: `request` + `response` modules

**Files:**
- Create: `crates/core/src/request.rs`
- Create: `crates/core/src/response.rs`
- Modify: `crates/core/src/lib.rs`

- [ ] **Step 1: Implementation**

`crates/core/src/request.rs`:

```rust
use serde::{Deserialize, Serialize};

use crate::extensions::ExtensionMap;
use crate::ids::RequestId;
use crate::message::{Message, SystemInstruction};
use crate::target::{FrontendInfo, FrontendModel};
use crate::tool::{ToolChoice, ToolDefinition};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GenerationOptions {
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    pub seed: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    JsonObject,
    JsonSchema { name: String, schema: serde_json::Value, strict: bool },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub forwarded_headers: ExtensionMap,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalRequest {
    pub id: RequestId,
    pub frontend: FrontendInfo,
    pub model: FrontendModel,
    #[serde(default)]
    pub system: Vec<SystemInstruction>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub tool_choice: ToolChoice,
    #[serde(default)]
    pub generation: GenerationOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub metadata: RequestMetadata,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentBlock;
    use crate::message::{Message, MessageRole};
    use crate::target::{FrontendInfo, FrontendKind, FrontendModel};

    #[test]
    fn minimal_request_round_trips() {
        let req = CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo { kind: FrontendKind::AnthropicMessages, api_path: "/v1/messages".into() },
            model: FrontendModel::from("claude-3-5-sonnet"),
            system: vec![],
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::text("hi")],
                name: None,
                extensions: Default::default(),
            }],
            tools: vec![],
            tool_choice: Default::default(),
            generation: Default::default(),
            response_format: None,
            stream: true,
            metadata: Default::default(),
            extensions: Default::default(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: CanonicalRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }
}
```

`crates/core/src/response.rs`:

```rust
use serde::{Deserialize, Serialize};

use crate::content::ContentBlock;
use crate::ids::ResponseId;
use crate::usage::{StopReason, Usage};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalResponse {
    pub id: ResponseId,
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub stop_sequence: Option<String>,
    pub usage: Option<Usage>,
}
```

- [ ] **Step 2: Re-export, run tests, commit**

Append to `lib.rs`:

```rust
pub mod request;
pub mod response;
pub use request::{CanonicalRequest, GenerationOptions, RequestMetadata, ResponseFormat};
pub use response::CanonicalResponse;
```

Run: `cargo test -p agent-shim-core`
Expected: all previous tests + the new `request` test pass.

```bash
git add crates/core
git commit -m "feat(core): add CanonicalRequest and CanonicalResponse"
```

---

## Task 14: Property tests for full request round-trip

**Files:**
- Create: `crates/core/tests/proptest_roundtrip.rs`

- [ ] **Step 1: Write the property test**

`crates/core/tests/proptest_roundtrip.rs`:

```rust
use agent_shim_core::{
    content::{ContentBlock, TextBlock},
    extensions::ExtensionMap,
    ids::RequestId,
    message::{Message, MessageRole},
    request::{CanonicalRequest, GenerationOptions, RequestMetadata},
    target::{FrontendInfo, FrontendKind, FrontendModel},
};
use proptest::prelude::*;

fn arb_text_block() -> impl Strategy<Value = ContentBlock> {
    "[a-zA-Z0-9 ]{0,32}".prop_map(|t| ContentBlock::Text(TextBlock {
        text: t,
        extensions: ExtensionMap::default(),
    }))
}

fn arb_message() -> impl Strategy<Value = Message> {
    (
        prop_oneof![Just(MessageRole::User), Just(MessageRole::Assistant)],
        prop::collection::vec(arb_text_block(), 1..4),
    )
        .prop_map(|(role, content)| Message {
            role,
            content,
            name: None,
            extensions: ExtensionMap::default(),
        })
}

fn arb_request() -> impl Strategy<Value = CanonicalRequest> {
    (prop::collection::vec(arb_message(), 1..6), any::<bool>(), 0u32..4096u32).prop_map(
        |(messages, stream, max_tokens)| CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::OpenAiChat,
                api_path: "/v1/chat/completions".into(),
            },
            model: FrontendModel::from("gpt-4o"),
            system: vec![],
            messages,
            tools: vec![],
            tool_choice: Default::default(),
            generation: GenerationOptions { max_tokens: Some(max_tokens), ..Default::default() },
            response_format: None,
            stream,
            metadata: RequestMetadata::default(),
            extensions: ExtensionMap::default(),
        },
    )
}

proptest! {
    #[test]
    fn request_json_round_trip(req in arb_request()) {
        let json = serde_json::to_string(&req).unwrap();
        let back: CanonicalRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, req);
    }
}
```

- [ ] **Step 2: Run**

Run: `cargo test -p agent-shim-core --test proptest_roundtrip`
Expected: PASS (256 cases).

- [ ] **Step 3: Commit**

```bash
git add crates/core/tests
git commit -m "test(core): proptest round-trip for CanonicalRequest"
```

---

## Task 15: CI workflow

**Files:**
- Create: `.github/workflows/ci.yaml`

- [ ] **Step 1: CI YAML**

```yaml
name: CI

on:
  push: { branches: [main] }
  pull_request:

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all -- --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - uses: taiki-e/install-action@nextest
      - run: cargo nextest run --workspace
      - uses: EmbarkStudios/cargo-deny-action@v2
```

- [ ] **Step 2: Verify locally**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green.

- [ ] **Step 3: Commit**

```bash
git add .github
git commit -m "ci: add fmt/clippy/nextest/cargo-deny workflow"
```

---

## Self-Review Notes

- Spec §4 fields covered: `RequestId`, `ResponseId`, `ToolCallId`, `FrontendInfo`, `FrontendModel`, `SystemInstruction` + `SystemSource`, `Message` + `MessageRole`, all nine `ContentBlock` variants, `ToolDefinition`, `ToolChoice`, `ToolCallBlock`, `ToolCallArguments` (Complete + Streaming/RawValue), `ToolResultBlock`, `BinarySource` (4 variants), `StreamEvent` (all 14 variants), `Usage`, `StopReason`, `CanonicalRequest`, `GenerationOptions`, `ResponseFormat`, `RequestMetadata`, `ExtensionMap`, `ProviderCapabilities`, `BackendTarget`, `CanonicalStream` type alias, `StreamError`. ✓
- `RawValue` preservation tested in Task 8. ✓
- No I/O imports anywhere in `core`. ✓
- Property test covers JSON round-trip — encoders in later plans inherit confidence. ✓
