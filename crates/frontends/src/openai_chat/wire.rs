use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Inbound (request) wire types ─────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionsRequest {
    pub model: String,
    pub messages: Vec<InboundMessage>,
    #[serde(default)]
    pub tools: Option<Vec<InboundTool>>,
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
    pub stream: Option<bool>,
    #[serde(default)]
    pub response_format: Option<InboundResponseFormat>,
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub user: Option<String>,
}

/// `stop` can be either a single string or an array of strings.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StopField {
    One(String),
    Many(Vec<String>),
}

impl StopField {
    pub fn into_vec(self) -> Vec<String> {
        match self {
            StopField::One(s) => vec![s],
            StopField::Many(v) => v,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<InboundMessageContent>,
    #[serde(default)]
    pub name: Option<String>,
    /// Present on `assistant` messages that called tools.
    #[serde(default)]
    pub tool_calls: Vec<InboundToolCall>,
    /// Present on `tool` role messages (tool results).
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
    ImageUrl { image_url: ImageUrlPayload },
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImageUrlPayload {
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub function: InboundToolCallFunction,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundTool {
    #[serde(rename = "type")]
    pub ty: String,
    pub function: InboundToolFunction,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundToolFunction {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<Value>,
}

/// `tool_choice` is either a string mode ("none", "auto", "required") or a
/// specific function object.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum InboundToolChoice {
    Mode(String),
    Specific {
        #[serde(rename = "type")]
        ty: String,
        function: InboundToolChoiceFunction,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundToolChoiceFunction {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundResponseFormat {
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub json_schema: Option<InboundJsonSchema>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundJsonSchema {
    pub name: String,
    pub schema: Value,
    #[serde(default)]
    pub strict: Option<bool>,
}

// ── Outbound streaming wire types ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ChunkOut {
    pub id: String,
    pub object: &'static str,
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

#[derive(Debug, Clone, Serialize, Default)]
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
    pub ty: Option<&'static str>,
    pub function: ToolCallFunctionDeltaOut,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ToolCallFunctionDeltaOut {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

// ── Outbound unary wire types ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionOut {
    pub id: String,
    pub object: &'static str,
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
    pub ty: &'static str,
    pub function: UnaryToolCallFunction,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnaryToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageOut {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}
