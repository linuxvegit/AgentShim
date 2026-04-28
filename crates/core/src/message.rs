use serde::{Deserialize, Serialize};

use crate::content::ContentBlock;
use crate::extensions::ExtensionMap;

/// The role of a message participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    /// Used for tool result turns (OpenAI "tool" role).
    Tool,
}

/// A single conversation turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

impl Message {
    pub fn user(content: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::User,
            content,
            name: None,
            extensions: ExtensionMap::new(),
        }
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content,
            name: None,
            extensions: ExtensionMap::new(),
        }
    }
}

/// Where a system instruction originated — different frontends encode this differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemSource {
    /// Anthropic Messages API `system` field.
    AnthropicSystem,
    /// OpenAI Chat `system` role message.
    OpenAiSystem,
    /// OpenAI `developer` role message (o-series).
    OpenAiDeveloper,
}

/// A system / developer prompt extracted from the request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemInstruction {
    pub source: SystemSource,
    pub content: Vec<ContentBlock>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::TextBlock;

    #[test]
    fn message_role_serializes_snake_case() {
        let j = serde_json::to_string(&MessageRole::Assistant).unwrap();
        assert_eq!(j, "\"assistant\"");
    }

    #[test]
    fn message_round_trips() {
        let msg = Message::user(vec![ContentBlock::Text(TextBlock { text: "hi".into(), extensions: ExtensionMap::new() })]);
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn message_name_field_round_trips() {
        let mut msg = Message::user(vec![ContentBlock::text("hi")]);
        msg.name = Some("alice".into());
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"name\":\"alice\""));
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, Some("alice".into()));
    }

    #[test]
    fn system_instruction_round_trips() {
        let si = SystemInstruction {
            source: SystemSource::AnthropicSystem,
            content: vec![ContentBlock::text("You are helpful.")],
        };
        let json = serde_json::to_string(&si).unwrap();
        let back: SystemInstruction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, si);
    }
}
