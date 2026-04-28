use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::ids::ToolCallId;

/// A tool/function definition exposed to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// The tool name the model will use when calling it.
    pub name: String,
    /// Human-readable description of what the tool does.
    pub description: Option<String>,
    /// JSON Schema object describing the input parameters.
    pub input_schema: serde_json::Value,
}

/// How the model should select tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Let the model decide.
    Auto,
    /// The model must not call any tool.
    None,
    /// The model must call at least one tool.
    Required,
    /// The model must call this specific tool.
    Specific { name: String },
}

/// Arguments to a tool call — either a completed JSON value or a streaming raw fragment.
///
/// The `Complete` variant stores the JSON as a plain `String` so that serde's externally-tagged
/// enum layout works without restriction. Use [`ToolCallArguments::complete`] to validate and
/// construct it, and [`ToolCallArguments::as_raw_str`] to access it as a raw JSON slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallArguments {
    /// Fully accumulated arguments as a raw JSON string (preserves key order).
    Complete { json: String },
    /// Partial JSON accumulated so far during streaming.
    Streaming { partial_json: String },
}

impl ToolCallArguments {
    /// Return the raw JSON string if this is the `Complete` variant.
    pub fn as_raw_str(&self) -> Option<&str> {
        match self {
            ToolCallArguments::Complete { json } => Some(json.as_str()),
            _ => None,
        }
    }

    /// Construct a `Complete` variant from a string, validating that it is valid JSON.
    pub fn complete(json: impl Into<String>) -> Result<Self, serde_json::Error> {
        let s = json.into();
        let _: Box<RawValue> = RawValue::from_string(s.clone())?;
        Ok(ToolCallArguments::Complete { json: s })
    }
}

/// A single tool call emitted by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallBlock {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: ToolCallArguments,
}

/// The result of executing a tool, returned to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultBlock {
    pub tool_call_id: ToolCallId,
    pub content: String,
    /// Whether the tool execution produced an error.
    pub is_error: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition_round_trips() {
        let def = ToolDefinition {
            name: "get_weather".into(),
            description: Some("Returns current weather.".into()),
            input_schema: serde_json::json!({ "type": "object", "properties": {} }),
        };
        let json = serde_json::to_string(&def).unwrap();
        let back: ToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(back, def);
    }

    #[test]
    fn tool_choice_specific_round_trips() {
        let choice = ToolChoice::Specific {
            name: "get_weather".into(),
        };
        let json = serde_json::to_string(&choice).unwrap();
        let back: ToolChoice = serde_json::from_str(&json).unwrap();
        assert_eq!(back, choice);
    }

    #[test]
    fn tool_call_arguments_complete_preserves_raw_json() {
        let raw = r#"{"location":"Paris","unit":"celsius"}"#;
        let args = ToolCallArguments::complete(raw).unwrap();
        let serialized = serde_json::to_string(&args).unwrap();
        let back: ToolCallArguments = serde_json::from_str(&serialized).unwrap();
        assert_eq!(back.as_raw_str(), Some(raw));
    }
}
