use serde::{Deserialize, Serialize};

use crate::extensions::ExtensionMap;
use crate::ids::RequestId;
use crate::message::{Message, SystemInstruction};
use crate::target::{FrontendInfo, FrontendModel};
use crate::tool::{ToolChoice, ToolDefinition};

/// Knobs that control the generation process.
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

/// Desired output format constraints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Free-form text (default).
    Text,
    /// The model must respond with valid JSON (no schema enforcement).
    JsonObject,
    /// The model must follow the supplied JSON schema.
    JsonSchema { name: String, schema: serde_json::Value, strict: bool },
}

/// Request-level metadata carried through the gateway for logging/tracing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub forwarded_headers: ExtensionMap,
}

/// The fully-normalised, frontend-agnostic request handed to a backend.
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
