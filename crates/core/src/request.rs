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
    /// Reasoning / thinking control. Translated to provider-native form by
    /// each backend (e.g. OpenAI/Copilot `reasoning_effort`, Anthropic
    /// `thinking.budget_tokens`). `None` means the agent didn't ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningOptions>,
}

/// Frontend-agnostic reasoning controls.
///
/// Models accept a qualitative effort level (`minimal`/`low`/`medium`/`high`)
/// or — on Anthropic — an explicit token budget. We carry both so each backend
/// can pick the form it understands.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReasoningOptions {
    /// Qualitative effort (OpenAI / Copilot / GPT-5 / o-series style).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    /// Explicit reasoning token budget (Anthropic-style).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

/// Qualitative reasoning-effort levels accepted by Copilot / OpenAI / GPT-5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Xhigh => "xhigh",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "minimal" | "none" => Some(ReasoningEffort::Minimal),
            "low" => Some(ReasoningEffort::Low),
            "medium" | "default" => Some(ReasoningEffort::Medium),
            "high" => Some(ReasoningEffort::High),
            "xhigh" | "x-high" | "extra_high" | "max" => Some(ReasoningEffort::Xhigh),
            _ => None,
        }
    }
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
    JsonSchema {
        name: String,
        schema: serde_json::Value,
        strict: bool,
    },
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
