use std::time::{SystemTime, UNIX_EPOCH};

use super::mapping::status_from_stop_reason;
use super::wire::{OutputContent, OutputItem, ResponseObject, UsageOut};
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
    let created_at = clock_override.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    });

    let mut output: Vec<OutputItem> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();
    let mut msg_index: u32 = 0;

    for block in response.content {
        match block {
            ContentBlock::Text(t) => text_parts.push(t.text),
            ContentBlock::ToolCall(tc) => {
                // Flush accumulated text as a message item first
                if !text_parts.is_empty() {
                    output.push(OutputItem::Message {
                        id: format!("msg_{msg_index}"),
                        role: "assistant",
                        status: "completed",
                        content: vec![OutputContent::OutputText {
                            text: text_parts.join(""),
                            annotations: vec![],
                        }],
                    });
                    msg_index += 1;
                    text_parts.clear();
                }

                let arguments = match tc.arguments {
                    ToolCallArguments::Complete { value } => {
                        serde_json::to_string(&value).unwrap_or_default()
                    }
                    ToolCallArguments::Streaming { data } => data,
                };
                output.push(OutputItem::FunctionCall {
                    id: format!("fc_{msg_index}"),
                    call_id: tc.id.0,
                    name: tc.name,
                    arguments,
                    status: "completed",
                });
                msg_index += 1;
            }
            _ => {}
        }
    }

    // Flush remaining text
    if !text_parts.is_empty() {
        output.push(OutputItem::Message {
            id: format!("msg_{msg_index}"),
            role: "assistant",
            status: "completed",
            content: vec![OutputContent::OutputText {
                text: text_parts.join(""),
                annotations: vec![],
            }],
        });
    }

    let status = status_from_stop_reason(&response.stop_reason);

    let usage = response.usage.as_ref().map(|u| UsageOut {
        input_tokens: u.input_tokens.unwrap_or(0),
        output_tokens: u.output_tokens.unwrap_or(0),
        total_tokens: u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0),
    });

    let resp_id = format!("resp_{}", response.id.0);

    let out = ResponseObject {
        id: resp_id,
        object: "response",
        status,
        model: response.model,
        created_at,
        output,
        usage,
    };

    serde_json::to_vec(&out)
        .map(Bytes::from)
        .map_err(|e| FrontendError::Encode(e.to_string()))
}
