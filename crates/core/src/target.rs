use serde::{Deserialize, Serialize};

/// The model name as sent by the frontend client (e.g. "claude-3-5-sonnet-20241022").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FrontendModel(pub String);

impl FrontendModel {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<S: Into<String>> From<S> for FrontendModel {
    fn from(s: S) -> Self {
        Self(s.into())
    }
}

/// Which frontend API dialect the client is speaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendKind {
    AnthropicMessages,
    OpenAiChat,
}

/// Information about the frontend that sent the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendInfo {
    pub kind: FrontendKind,
    pub requested_model: FrontendModel,
}

/// Identifies the backend that should handle the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendTarget {
    /// Name of the backend provider (e.g. "anthropic", "openai", "copilot").
    pub provider: String,
    /// The model to use on the backend, after any model-mapping has been applied.
    pub model: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontend_model_round_trips() {
        let m = FrontendModel::from("claude-3-5-sonnet-20241022");
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, "\"claude-3-5-sonnet-20241022\"");
    }

    #[test]
    fn frontend_kind_serializes_snake_case() {
        let j = serde_json::to_string(&FrontendKind::OpenAiChat).unwrap();
        assert_eq!(j, "\"open_ai_chat\"");
    }
}
