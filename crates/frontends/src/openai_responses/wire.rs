use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Inbound (request) wire types ─────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: InputField,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub tools: Option<Vec<InboundTool>>,
    #[serde(default)]
    pub tool_choice: Option<InboundToolChoice>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub metadata: Option<Value>,
    // Fields we accept but ignore (stateless gateway)
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub store: Option<bool>,
    #[serde(default)]
    pub truncation: Option<String>,
    #[serde(default)]
    pub include: Option<Vec<String>>,
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReasoningConfig {
    #[serde(default)]
    pub effort: Option<String>,
}

/// The `input` field accepts a plain string, a message array, or a typed item
/// array.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum InputField {
    Text(String),
    Messages(Vec<InputMessage>),
    Items(Vec<InputItem>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct InputMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<InputMessageContent>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum InputMessageContent {
    Text(String),
    Parts(Vec<InputContentPart>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputContentPart {
    InputText { text: String },
    InputImage { image_url: String },
    /// Assistant-side text part. OpenAI Responses uses `output_text` for
    /// content emitted by the model; codex 0.5+ feeds prior assistant
    /// messages back into `input` with their original `output_text`
    /// parts intact. Decoded into a canonical text block on the canonical
    /// path; passthrough preserves the original bytes.
    OutputText { text: String },
    /// Forward-compatibility catch-all for content part types we don't
    /// enumerate (future OpenAI Responses additions). Without this,
    /// `Items(Vec<InputItem>)` decoding cascades up the untagged
    /// `InputField` enum and 400's the request before admission.
    #[serde(other)]
    Other,
}

/// Typed input items for multi-turn conversations.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputItem {
    Message {
        role: String,
        #[serde(default)]
        content: Option<InputMessageContent>,
    },
    FunctionCall {
        #[serde(default)]
        id: Option<String>,
        call_id: String,
        name: String,
        /// `arguments` is typed as `Value` because OpenAI Responses clients
        /// (codex 0.5+) sometimes emit it as a JSON object directly rather
        /// than the spec-canonical JSON-encoded string. Both shapes are
        /// accepted here; the decoder normalizes downstream.
        arguments: Value,
    },
    FunctionCallOutput {
        call_id: String,
        /// Same rationale as `arguments` above -- accept both String and
        /// structured shapes so tool outputs containing JSON don't get
        /// double-encoded by the client (which would then fail strict
        /// String deserialization).
        output: Value,
    },
    /// Reasoning item from a previous turn (multi-turn input). Mirrors the
    /// real OpenAI Responses shape: `summary` and `content` are part-arrays,
    /// not flattened strings. Decoded into `ContentBlock::Reasoning` on the
    /// preceding assistant message in T2.
    Reasoning {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        summary: Option<Vec<ReasoningSummaryPart>>,
        #[serde(default)]
        content: Option<Vec<ReasoningContentPart>>,
        #[serde(default)]
        status: Option<String>,
    },
    /// Forward-compatibility catch-all for input item types we don't
    /// enumerate (e.g. `mcp_call`, `mcp_list_tools`, `web_search_call`,
    /// `local_shell_call`, `image_generation_call`, `namespace`, future
    /// OpenAI Responses additions).
    ///
    /// Without this variant, a single unknown `type` in the input array
    /// would fail the entire `InputItem` deserialization, which then
    /// cascades up the untagged `InputField` enum and 400's the request
    /// before admission. With it, the byte-passthrough path forwards the
    /// original body verbatim to an OpenAI-Responses-native upstream
    /// (which is the authority on whether the type is valid), while the
    /// canonical chain walk drops these items because they have no
    /// canonical representation.
    #[serde(other)]
    Other,
}

/// Inbound part inside a `reasoning` item's `summary` array.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningSummaryPart {
    SummaryText { text: String },
    /// Forward-compatibility catch-all (same rationale as `InputItem::Other`).
    /// Reasoning summaries occasionally carry vendor-specific part types
    /// (`encrypted_content`, etc.) that we drop on the canonical path
    /// while passthrough preserves the original bytes.
    #[serde(other)]
    Other,
}

/// Inbound part inside a `reasoning` item's `content` array.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningContentPart {
    ReasoningText { text: String },
    /// Forward-compatibility catch-all (same rationale as `InputItem::Other`).
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundTool {
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<Value>,
    // For function-type tools, the definition may be nested
    #[serde(default)]
    pub function: Option<InboundToolFunction>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundToolFunction {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum InboundToolChoice {
    Mode(String),
    Specific {
        #[serde(rename = "type")]
        ty: String,
        name: String,
    },
}

// ── Outbound wire types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ResponseObject {
    pub id: String,
    pub object: &'static str,
    pub status: &'static str,
    pub model: String,
    pub created_at: u64,
    pub output: Vec<OutputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageOut>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    Message {
        id: String,
        role: &'static str,
        status: &'static str,
        content: Vec<OutputContent>,
    },
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
        arguments: String,
        status: &'static str,
    },
    Reasoning {
        id: String,
        status: &'static str,
        content: Vec<OutputContent>,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputContent {
    OutputText {
        text: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        annotations: Vec<Value>,
    },
    Reasoning {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageOut {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
}

// ── Streaming event payloads ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct OutputItemAdded {
    pub output_index: u32,
    pub item: OutputItem,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContentPartAdded {
    pub item_id: String,
    pub output_index: u32,
    pub content_index: u32,
    pub part: OutputContent,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextDeltaPayload {
    pub item_id: String,
    pub output_index: u32,
    pub content_index: u32,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextDonePayload {
    pub item_id: String,
    pub output_index: u32,
    pub content_index: u32,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContentPartDone {
    pub item_id: String,
    pub output_index: u32,
    pub content_index: u32,
    pub part: OutputContent,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutputItemDone {
    pub output_index: u32,
    pub item: OutputItem,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionCallArgsDelta {
    pub item_id: String,
    pub output_index: u32,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionCallArgsDone {
    pub item_id: String,
    pub output_index: u32,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReasoningDeltaPayload {
    pub item_id: String,
    pub output_index: u32,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReasoningDonePayload {
    pub item_id: String,
    pub output_index: u32,
    pub text: String,
}
