use serde::{Deserialize, Serialize};

use crate::extensions::ExtensionMap;
use crate::ids::RequestId;
use crate::message::{Message, SystemInstruction};
use crate::target::{BackendTarget, FrontendInfo};
use crate::tool::{ToolChoice, ToolDefinition};

/// Desired output format constraints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Free-form text (default).
    Text,
    /// The model must respond with valid JSON (no schema enforcement).
    JsonObject,
    /// The model must follow the supplied JSON schema.
    JsonSchema { schema: serde_json::Value },
}

/// Knobs that control the generation process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GenerationOptions {
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub stop_sequences: Option<Vec<String>>,
    pub response_format: Option<ResponseFormat>,
    /// Budget in tokens for extended thinking / reasoning.
    pub thinking_budget_tokens: Option<u32>,
}

/// Request-level metadata carried through the gateway for logging/tracing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RequestMetadata {
    /// Correlation ID supplied by the caller.
    pub request_id: Option<RequestId>,
    /// Arbitrary tags the caller wants preserved in logs.
    pub tags: Option<std::collections::HashMap<String, String>>,
}

/// The fully-normalised, frontend-agnostic request handed to a backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalRequest {
    pub frontend: FrontendInfo,
    pub target: BackendTarget,
    pub system: Vec<SystemInstruction>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub tool_choice: Option<ToolChoice>,
    pub options: GenerationOptions,
    pub metadata: RequestMetadata,
    pub extensions: ExtensionMap,
}
