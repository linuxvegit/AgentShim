use serde::{Deserialize, Serialize};

/// Describes what a backend provider supports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderCapabilities {
    pub supports_streaming: bool,
    pub supports_tool_use: bool,
    pub supports_vision: bool,
    pub supports_audio: bool,
    pub supports_file_upload: bool,
    pub supports_system_prompt: bool,
    pub supports_reasoning: bool,
    pub supports_json_mode: bool,
    pub supports_parallel_tool_calls: bool,
    pub available_models: Option<Vec<String>>,
}

impl ProviderCapabilities {
    /// Returns true if the given model identifier is in the available_models list,
    /// or if no list is present (unknown / open-ended provider).
    pub fn supports_model(&self, model: &str) -> bool {
        match &self.available_models {
            None => true,
            Some(models) => models.iter().any(|m| m == model),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_model_returns_true_when_no_list() {
        let caps = ProviderCapabilities::default();
        assert!(caps.supports_model("anything"));
    }

    #[test]
    fn supports_model_checks_list() {
        let caps = ProviderCapabilities {
            available_models: Some(vec!["gpt-4o".into(), "gpt-4o-mini".into()]),
            ..Default::default()
        };
        assert!(caps.supports_model("gpt-4o"));
        assert!(!caps.supports_model("claude-3-5-sonnet"));
    }
}
