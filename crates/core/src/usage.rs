use serde::{Deserialize, Serialize};

/// Token usage for a single request/response pair.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
    pub reasoning_tokens: Option<u32>,
    #[serde(default)]
    pub estimated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_raw: Option<serde_json::Value>,
}

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model reached a natural end of turn.
    EndTurn,
    /// The model hit the max_tokens limit.
    MaxTokens,
    /// The model called one or more tools.
    ToolUse,
    /// The model was stopped by a stop sequence.
    StopSequence,
    /// The content was filtered.
    ContentFilter,
    /// An unrecognised stop reason from the provider.
    Other(String),
}

impl StopReason {
    /// Normalise a provider-specific stop reason string into the canonical enum.
    pub fn from_provider_string(s: &str) -> Self {
        match s {
            "end_turn" | "stop" => StopReason::EndTurn,
            "max_tokens" | "length" => StopReason::MaxTokens,
            "tool_use" | "tool_calls" => StopReason::ToolUse,
            "stop_sequence" => StopReason::StopSequence,
            "content_filter" => StopReason::ContentFilter,
            other => StopReason::Other(other.to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_normalisation() {
        assert_eq!(
            StopReason::from_provider_string("stop"),
            StopReason::EndTurn
        );
        assert_eq!(
            StopReason::from_provider_string("length"),
            StopReason::MaxTokens
        );
        assert_eq!(
            StopReason::from_provider_string("tool_calls"),
            StopReason::ToolUse
        );
        assert_eq!(
            StopReason::from_provider_string("unknown_xyz"),
            StopReason::Other("unknown_xyz".into())
        );
    }

    #[test]
    fn stop_reason_round_trips() {
        let r = StopReason::EndTurn;
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, "\"end_turn\"");
        let back: StopReason = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn usage_optional_fields() {
        let u = Usage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            ..Default::default()
        };
        let json = serde_json::to_string(&u).unwrap();
        let back: Usage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, u);
    }
}
