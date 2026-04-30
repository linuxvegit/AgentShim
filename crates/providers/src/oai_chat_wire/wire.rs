//! Wire types for outbound OpenAI chat.completions requests.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ChatBody {
    pub model: String,
    pub messages: Vec<MsgOut>,
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
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub stop: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormatOut>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tools: Vec<ToolOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoiceOut>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// GPT-5 / Copilot reasoning effort: `minimal`, `low`, `medium`, `high`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MsgOut {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallOut>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ToolCallOut {
    pub id: String,
    pub r#type: String,
    pub function: FunctionCallOut,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct FunctionCallOut {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ToolOut {
    pub r#type: String,
    pub function: FunctionDefOut,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct FunctionDefOut {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum ToolChoiceOut {
    String(String),
    Object {
        r#type: String,
        function: ToolChoiceFunction,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ToolChoiceFunction {
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ResponseFormatOut {
    Text,
    JsonObject,
    JsonSchema { json_schema: JsonSchemaOut },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct JsonSchemaOut {
    pub name: String,
    pub schema: Value,
    pub strict: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StreamOptions {
    pub include_usage: bool,
}
