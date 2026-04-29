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
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
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
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputContent {
    OutputText {
        text: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        annotations: Vec<Value>,
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
