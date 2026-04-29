use agent_shim_core::usage::StopReason;

/// Map a canonical `StopReason` to the Responses API `status` field.
pub fn status_from_stop_reason(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::ContentFilter => "incomplete",
        _ => "completed",
    }
}

/// Map a canonical `StopReason` to the OpenAI `finish_reason` equivalent
/// (same strings as Chat Completions — used in incomplete_details if needed).
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
