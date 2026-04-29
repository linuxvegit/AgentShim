use std::time::{SystemTime, UNIX_EPOCH};

use super::mapping::finish_reason_from_canonical;
use super::wire::{
    ChatCompletionOut, UnaryChoice, UnaryMessage, UnaryToolCall, UnaryToolCallFunction, UsageOut,
};
use crate::FrontendError;
use agent_shim_core::{
    content::ContentBlock, response::CanonicalResponse, tool::ToolCallArguments,
};
use bytes::Bytes;

pub fn encode(response: CanonicalResponse) -> Result<Bytes, FrontendError> {
    encode_with_clock(response, None)
}

pub fn encode_with_clock(
    response: CanonicalResponse,
    clock_override: Option<u64>,
) -> Result<Bytes, FrontendError> {
    let created = clock_override.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    });

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<UnaryToolCall> = Vec::new();

    for block in response.content {
        match block {
            ContentBlock::Text(t) => text_parts.push(t.text),
            ContentBlock::ToolCall(tc) => {
                let arguments = match tc.arguments {
                    ToolCallArguments::Complete { value } => {
                        serde_json::to_string(&value).unwrap_or_default()
                    }
                    ToolCallArguments::Streaming { data } => data,
                };
                tool_calls.push(UnaryToolCall {
                    id: tc.id.0,
                    ty: "function",
                    function: UnaryToolCallFunction {
                        name: tc.name,
                        arguments,
                    },
                });
            }
            // Reasoning, Image, Audio, File, ToolResult, Unsupported — skip
            _ => {}
        }
    }

    let content = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join(""))
    };

    let finish_reason = finish_reason_from_canonical(&response.stop_reason).to_owned();

    let usage = response.usage.as_ref().map(|u| UsageOut {
        prompt_tokens: u.input_tokens.unwrap_or(0),
        completion_tokens: u.output_tokens.unwrap_or(0),
        total_tokens: u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0),
    });

    let out = ChatCompletionOut {
        id: response.id.0,
        object: "chat.completion",
        created,
        model: response.model,
        choices: vec![UnaryChoice {
            index: 0,
            message: UnaryMessage {
                role: "assistant",
                content,
                tool_calls,
            },
            finish_reason,
        }],
        usage,
    };

    serde_json::to_vec(&out)
        .map(Bytes::from)
        .map_err(|e| FrontendError::Encode(e.to_string()))
}
