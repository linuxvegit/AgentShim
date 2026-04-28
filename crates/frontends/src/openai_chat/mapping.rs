use agent_shim_core::{message::MessageRole, message::SystemSource, usage::StopReason};

/// Describes how a role string maps into the canonical model.
pub enum RoleClass {
    Message(MessageRole),
    System(SystemSource),
}

/// Map an OpenAI role string to a `RoleClass`.
/// Returns `None` for unknown roles.
pub fn role_to_canonical(role: &str) -> Option<RoleClass> {
    match role {
        "user" => Some(RoleClass::Message(MessageRole::User)),
        "assistant" => Some(RoleClass::Message(MessageRole::Assistant)),
        "tool" => Some(RoleClass::Message(MessageRole::Tool)),
        "system" => Some(RoleClass::System(SystemSource::OpenAiSystem)),
        "developer" => Some(RoleClass::System(SystemSource::OpenAiDeveloper)),
        _ => None,
    }
}

/// Map a canonical `StopReason` to the OpenAI `finish_reason` string.
pub fn finish_reason_from_canonical(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "stop",
        StopReason::MaxTokens => "length",
        StopReason::ToolUse => "tool_calls",
        StopReason::ContentFilter => "content_filter",
        StopReason::StopSequence => "stop",
        StopReason::Other(_) => "stop",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_user() {
        assert!(matches!(
            role_to_canonical("user"),
            Some(RoleClass::Message(MessageRole::User))
        ));
    }

    #[test]
    fn role_assistant() {
        assert!(matches!(
            role_to_canonical("assistant"),
            Some(RoleClass::Message(MessageRole::Assistant))
        ));
    }

    #[test]
    fn role_tool() {
        assert!(matches!(
            role_to_canonical("tool"),
            Some(RoleClass::Message(MessageRole::Tool))
        ));
    }

    #[test]
    fn role_system() {
        assert!(matches!(
            role_to_canonical("system"),
            Some(RoleClass::System(SystemSource::OpenAiSystem))
        ));
    }

    #[test]
    fn role_developer() {
        assert!(matches!(
            role_to_canonical("developer"),
            Some(RoleClass::System(SystemSource::OpenAiDeveloper))
        ));
    }

    #[test]
    fn role_unknown_returns_none() {
        assert!(role_to_canonical("human").is_none());
    }

    #[test]
    fn finish_reason_end_turn() {
        assert_eq!(finish_reason_from_canonical(&StopReason::EndTurn), "stop");
    }

    #[test]
    fn finish_reason_max_tokens() {
        assert_eq!(finish_reason_from_canonical(&StopReason::MaxTokens), "length");
    }

    #[test]
    fn finish_reason_tool_use() {
        assert_eq!(finish_reason_from_canonical(&StopReason::ToolUse), "tool_calls");
    }

    #[test]
    fn finish_reason_content_filter() {
        assert_eq!(finish_reason_from_canonical(&StopReason::ContentFilter), "content_filter");
    }
}
