use agent_shim_core::{
    content::ContentBlock, response::CanonicalResponse, tool::ToolCallArguments,
};
use bytes::Bytes;
use serde_json::Value;

use super::mapping::stop_reason_to_anthropic;
use super::wire::{MessagesResponse, OutboundContentBlock, OutboundUsage};
use crate::FrontendError;

pub fn encode(response: CanonicalResponse) -> Result<Bytes, FrontendError> {
    let mut content_blocks: Vec<OutboundContentBlock> = Vec::new();

    for block in response.content {
        match block {
            ContentBlock::Text(t) => {
                content_blocks.push(OutboundContentBlock::Text { text: t.text });
            }
            ContentBlock::ToolCall(tc) => {
                let input = match tc.arguments {
                    ToolCallArguments::Complete { value } => value,
                    ToolCallArguments::Streaming { data } => {
                        serde_json::from_str(&data).unwrap_or(Value::String(data))
                    }
                };
                content_blocks.push(OutboundContentBlock::ToolUse {
                    id: tc.id.0,
                    name: tc.name,
                    input,
                });
            }
            ContentBlock::Reasoning(r) => {
                content_blocks.push(OutboundContentBlock::Thinking { thinking: r.text });
            }
            ContentBlock::RedactedReasoning(r) => {
                content_blocks.push(OutboundContentBlock::RedactedThinking { data: r.data });
            }
            // Image, Audio, File, ToolResult, Unsupported — skip in assistant turn
            _ => {}
        }
    }

    let usage = response
        .usage
        .as_ref()
        .map(|u| OutboundUsage {
            input_tokens: u.input_tokens.unwrap_or(0),
            output_tokens: u.output_tokens.unwrap_or(0),
            cache_creation_input_tokens: u.cache_creation_input_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
        })
        .unwrap_or_default();

    let stop_reason = stop_reason_to_anthropic(&response.stop_reason).to_owned();

    let wire = MessagesResponse {
        id: response.id.0,
        ty: "message",
        role: "assistant",
        content: content_blocks,
        model: response.model,
        stop_reason,
        stop_sequence: response.stop_sequence,
        usage,
    };

    serde_json::to_vec(&wire)
        .map(Bytes::from)
        .map_err(|e| FrontendError::Encode(e.to_string()))
}
