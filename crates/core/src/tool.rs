use serde::{Deserialize, Serialize};

use crate::extensions::ExtensionMap;
use crate::ids::ToolCallId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    Specific { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolCallArguments {
    Complete { value: serde_json::Value },
    Streaming { data: String },
}

impl PartialEq for ToolCallArguments {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Complete { value: a }, Self::Complete { value: b }) => a == b,
            (Self::Streaming { data: a }, Self::Streaming { data: b }) => a == b,
            _ => false,
        }
    }
}

impl Eq for ToolCallArguments {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallBlock {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: ToolCallArguments,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultBlock {
    pub tool_call_id: ToolCallId,
    pub content: serde_json::Value,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_choice_default_is_auto() {
        assert_eq!(ToolChoice::default(), ToolChoice::Auto);
    }

    #[test]
    fn tool_choice_specific_round_trips() {
        let c = ToolChoice::Specific { name: "search".into() };
        let s = serde_json::to_string(&c).unwrap();
        let back: ToolChoice = serde_json::from_str(&s).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn complete_args_round_trip() {
        let args = ToolCallArguments::Complete { value: json!({"q": "test"}) };
        let s = serde_json::to_string(&args).unwrap();
        let back: ToolCallArguments = serde_json::from_str(&s).unwrap();
        assert_eq!(back, args);
    }
}
