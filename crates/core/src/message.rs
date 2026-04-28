use serde::{Deserialize, Serialize};

use crate::content::ContentBlock;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(content: Vec<ContentBlock>) -> Self {
        Self { role: MessageRole::User, content }
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self { role: MessageRole::Assistant, content }
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemInstruction {
    pub source: SystemSource,
    pub text: String,
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
        let msg = Message::user(vec![ContentBlock::Text(TextBlock { text: "hi".into() })]);
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn system_instruction_round_trips() {
        let si = SystemInstruction {
            source: SystemSource::AnthropicSystem,
            text: "You are helpful.".into(),
        };
        let json = serde_json::to_string(&si).unwrap();
        let back: SystemInstruction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, si);
    }
}
