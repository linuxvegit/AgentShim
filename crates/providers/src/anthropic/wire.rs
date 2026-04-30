//! Anthropic Messages wire types — provider-side mirror of the frontend's
//! `anthropic_messages::wire`, with Serialize/Deserialize swapped because the
//! direction is reversed.
//!
//! Per CLAUDE.md, frontends and providers must never import each other; the
//! shared bridge is `agent_shim_core::mapping::anthropic_wire`. The DTOs live
//! duplicated in each crate so the serde shapes can evolve independently.
//!
//! Naming convention:
//! * `Outgoing*` — types that the provider serializes (request body sent to
//!   api.anthropic.com).
//! * `Incoming*` — types that the provider deserializes (SSE events and the
//!   non-streaming response body).

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Outgoing (request) wire types ───────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub(crate) struct OutgoingRequest {
    pub(crate) model: String,
    pub(crate) messages: Vec<OutgoingMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) system: Option<OutgoingSystem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tools: Option<Vec<OutgoingTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_choice: Option<OutgoingToolChoice>,
    pub(crate) max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stop_sequences: Option<Vec<String>>,
    pub(crate) stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thinking: Option<OutgoingThinking>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum OutgoingSystem {
    /// Plain string form (preferred when there's only one system instruction
    /// with a single text block).
    Text(String),
    /// Block-array form (used when the canonical request carries cache_control
    /// or other extension data on a system block).
    Blocks(Vec<OutgoingContentBlock>),
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct OutgoingThinking {
    #[serde(rename = "type")]
    pub(crate) ty: &'static str,
    pub(crate) budget_tokens: u32,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct OutgoingMessage {
    pub(crate) role: &'static str,
    pub(crate) content: Vec<OutgoingContentBlock>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum OutgoingContentBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<Value>,
    },
    Image {
        source: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<Value>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<Value>,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<OutgoingToolResultContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<Value>,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum OutgoingToolResultContent {
    Text(String),
    Blocks(Vec<Value>),
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct OutgoingTool {
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    pub(crate) input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cache_control: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum OutgoingToolChoice {
    /// Anthropic's default — `tool_choice` is omitted from the request body
    /// when the canonical request has [`ToolChoice::Auto`][tc-auto], so this
    /// variant is currently unused but kept for symmetry with the wire spec.
    ///
    /// [tc-auto]: agent_shim_core::tool::ToolChoice::Auto
    Auto,
    Any,
    Tool {
        name: String,
    },
    None,
}

// ── Incoming (response / SSE) wire types ────────────────────────────────────

/// A complete (non-streaming) Anthropic messages response.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct IncomingMessagesResponse {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) content: Vec<IncomingContentBlock>,
    #[serde(default)]
    pub(crate) stop_reason: Option<String>,
    #[serde(default)]
    pub(crate) stop_sequence: Option<String>,
    #[serde(default)]
    pub(crate) usage: Option<IncomingUsage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum IncomingContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct IncomingUsage {
    #[serde(default)]
    pub(crate) input_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) output_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_read_input_tokens: Option<u32>,
}

// ── SSE event wrappers (Incoming) ────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum IncomingEvent {
    MessageStart {
        message: IncomingMessageStart,
    },
    ContentBlockStart {
        index: u32,
        content_block: IncomingContentBlockStart,
    },
    ContentBlockDelta {
        index: u32,
        delta: IncomingContentBlockDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: IncomingMessageDelta,
        #[serde(default)]
        usage: Option<IncomingUsage>,
    },
    MessageStop,
    Ping,
    Error {
        error: IncomingErrorPayload,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct IncomingMessageStart {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) usage: Option<IncomingUsage>,
}

/// Streaming-shaped content block start. `tool_use.input` is a string (empty
/// at start, then filled in via `input_json_delta` events) — distinct from
/// the unary path's `IncomingContentBlock::ToolUse` where `input` is a Value.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum IncomingContentBlockStart {
    Text {
        #[serde(default)]
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    RedactedThinking {
        #[serde(default)]
        data: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)]
#[allow(dead_code)]
pub(crate) enum IncomingContentBlockDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct IncomingMessageDelta {
    #[serde(default)]
    pub(crate) stop_reason: Option<String>,
    #[serde(default)]
    pub(crate) stop_sequence: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct IncomingErrorPayload {
    #[serde(default, rename = "type")]
    pub(crate) ty: String,
    #[serde(default)]
    pub(crate) message: String,
}
