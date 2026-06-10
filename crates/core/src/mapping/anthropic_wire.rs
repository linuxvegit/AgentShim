use crate::message::MessageRole;
use crate::usage::StopReason;

pub fn stop_reason_to_anthropic(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::ToolUse => "tool_use",
        StopReason::StopSequence => "stop_sequence",
        StopReason::ContentFilter => "content_filter",
        StopReason::Other(_) => "end_turn",
    }
}

pub fn stop_reason_from_anthropic(s: &str) -> StopReason {
    match s {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "tool_use" => StopReason::ToolUse,
        "stop_sequence" => StopReason::StopSequence,
        "content_filter" => StopReason::ContentFilter,
        other => StopReason::Other(other.to_owned()),
    }
}

pub fn role_to_anthropic(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "user",
        MessageRole::System => "system",
    }
}

pub fn role_from_anthropic(s: &str) -> Option<MessageRole> {
    match s {
        "user" => Some(MessageRole::User),
        "assistant" => Some(MessageRole::Assistant),
        "system" => Some(MessageRole::System),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::StopReason;

    #[test]
    fn stop_reason_round_trip_end_turn() {
        let s = stop_reason_to_anthropic(&StopReason::EndTurn);
        assert_eq!(s, "end_turn");
        assert_eq!(stop_reason_from_anthropic(s), StopReason::EndTurn);
    }

    #[test]
    fn stop_reason_round_trip_tool_use() {
        let s = stop_reason_to_anthropic(&StopReason::ToolUse);
        assert_eq!(s, "tool_use");
        assert_eq!(stop_reason_from_anthropic(s), StopReason::ToolUse);
    }

    #[test]
    fn stop_reason_round_trip_max_tokens() {
        let s = stop_reason_to_anthropic(&StopReason::MaxTokens);
        assert_eq!(s, "max_tokens");
        assert_eq!(stop_reason_from_anthropic(s), StopReason::MaxTokens);
    }

    #[test]
    fn stop_reason_round_trip_stop_sequence() {
        let s = stop_reason_to_anthropic(&StopReason::StopSequence);
        assert_eq!(s, "stop_sequence");
        assert_eq!(stop_reason_from_anthropic(s), StopReason::StopSequence);
    }

    #[test]
    fn stop_reason_other_preserved() {
        assert_eq!(
            stop_reason_from_anthropic("weird_reason"),
            StopReason::Other("weird_reason".into()),
        );
    }

    #[test]
    fn role_round_trip_user() {
        let s = role_to_anthropic(MessageRole::User);
        assert_eq!(s, "user");
        assert_eq!(role_from_anthropic(s), Some(MessageRole::User));
    }

    #[test]
    fn role_round_trip_assistant() {
        let s = role_to_anthropic(MessageRole::Assistant);
        assert_eq!(s, "assistant");
        assert_eq!(role_from_anthropic(s), Some(MessageRole::Assistant));
    }

    #[test]
    fn role_round_trip_system() {
        let s = role_to_anthropic(MessageRole::System);
        assert_eq!(s, "system");
        assert_eq!(role_from_anthropic(s), Some(MessageRole::System));
    }

    #[test]
    fn role_system_string_decodes_to_system_variant() {
        // Was previously `role_unknown_returns_none` asserting "system" -> None.
        // ADR-0011 (iv) + 2026-06-10 spec flip this: "system" is now a valid role.
        assert_eq!(role_from_anthropic("system"), Some(MessageRole::System));
    }

    #[test]
    fn role_unknown_returns_none() {
        assert_eq!(role_from_anthropic("human"), None);
        assert_eq!(role_from_anthropic("developer"), None);
    }
}
