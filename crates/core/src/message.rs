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
    /// A system instruction whose position within the conversation
    /// matters. Distinct from the top-level
    /// `CanonicalRequest::system` vec, which expresses session-level
    /// standing instructions with no temporal anchor.
    System,
}

/// A single conversation turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Origin tag for `MessageRole::System` messages. Meaningful only
    /// when `role == System`; ignored for other roles. Lets the outbound
    /// encoder choose between `"system"` and `"developer"` wire role on
    /// OpenAI upstreams that distinguish them. Authorised by ADR-0011 (v).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SystemSource>,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

impl Message {
    pub fn user(content: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::User,
            content,
            name: None,
            source: None,
            extensions: ExtensionMap::new(),
        }
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content,
            name: None,
            source: None,
            extensions: ExtensionMap::new(),
        }
    }

    pub fn system(source: SystemSource, content: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::System,
            content,
            name: None,
            source: Some(source),
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
        let msg = Message::user(vec![ContentBlock::Text(TextBlock {
            text: "hi".into(),
            extensions: ExtensionMap::new(),
        })]);
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

    #[test]
    fn message_role_system_serializes_snake_case() {
        let j = serde_json::to_string(&MessageRole::System).unwrap();
        assert_eq!(j, "\"system\"");
    }

    #[test]
    fn message_role_system_round_trips() {
        let role: MessageRole = serde_json::from_str("\"system\"").unwrap();
        assert_eq!(role, MessageRole::System);
    }

    #[test]
    fn message_system_constructor_sets_role_and_source() {
        let msg = Message::system(
            SystemSource::AnthropicSystem,
            vec![ContentBlock::text("hello")],
        );
        assert_eq!(msg.role, MessageRole::System);
        assert_eq!(msg.source, Some(SystemSource::AnthropicSystem));
        assert!(msg.name.is_none());
    }

    #[test]
    fn message_serializes_source_only_when_present() {
        let msg = Message::system(
            SystemSource::OpenAiDeveloper,
            vec![ContentBlock::text("dev hint")],
        );
        let json = serde_json::to_string(&msg).unwrap();
        // `SystemSource` derives `rename_all = "snake_case"`, so
        // `OpenAiDeveloper` -> `"open_ai_developer"`. The point of this
        // test is the field is *present*; the exact spelling is
        // covered by `SystemSource`'s own enum tests elsewhere.
        assert!(
            json.contains("\"source\":\"open_ai_developer\""),
            "expected source field present; got: {json}"
        );

        let user_msg = Message::user(vec![ContentBlock::text("hi")]);
        let user_json = serde_json::to_string(&user_msg).unwrap();
        assert!(!user_json.contains("\"source\""));
    }

    #[test]
    fn message_deserializes_old_wire_without_source() {
        let old_wire = r#"{"role":"user","content":[{"type":"text","text":"hi"}]}"#;
        let msg: Message = serde_json::from_str(old_wire).unwrap();
        assert_eq!(msg.role, MessageRole::User);
        assert!(msg.source.is_none());
    }
}
