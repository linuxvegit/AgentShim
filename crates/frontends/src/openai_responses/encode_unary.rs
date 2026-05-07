use std::time::{SystemTime, UNIX_EPOCH};

use super::mapping::status_from_stop_reason;
use super::wire::{OutputContent, OutputItem, ResponseObject, UsageOut};
use crate::FrontendError;
use agent_shim_core::{
    content::ContentBlock, response::CanonicalResponse, tool::ToolCallArguments,
};
use bytes::Bytes;

/// Flush any accumulated text fragments as a completed Message output item.
///
/// No-op if `text_parts` is empty. Advances `msg_index` only when a flush
/// actually emits an item.
fn flush_text(output: &mut Vec<OutputItem>, text_parts: &mut Vec<String>, msg_index: &mut u32) {
    if text_parts.is_empty() {
        return;
    }
    let text: String = std::mem::take(text_parts).into_iter().collect();
    output.push(OutputItem::Message {
        id: format!("msg_{msg_index}"),
        role: "assistant",
        status: "completed",
        content: vec![OutputContent::OutputText {
            text,
            annotations: vec![],
        }],
    });
    *msg_index += 1;
}

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
                flush_text(&mut output, &mut text_parts, &mut msg_index);

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
            ContentBlock::Reasoning(rb) => {
                // Flush accumulated text as a message item first
                flush_text(&mut output, &mut text_parts, &mut msg_index);

                output.push(OutputItem::Reasoning {
                    id: format!("rs_{msg_index}"),
                    status: "completed",
                    content: vec![OutputContent::Reasoning {
                        text: rb.text,
                        summary: None,
                    }],
                });
                msg_index += 1;
            }
            _ => {}
        }
    }

    // Flush remaining text
    flush_text(&mut output, &mut text_parts, &mut msg_index);

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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        content::{ContentBlock, ReasoningBlock},
        extensions::ExtensionMap,
        ids::ResponseId,
        usage::StopReason,
    };
    use serde_json::Value;

    fn build_response(content: Vec<ContentBlock>) -> CanonicalResponse {
        CanonicalResponse {
            id: ResponseId("test".to_string()),
            model: "gpt-test".to_string(),
            content,
            stop_reason: StopReason::EndTurn,
            stop_sequence: None,
            usage: None,
        }
    }

    #[test]
    fn unary_response_with_reasoning_block_emits_reasoning_item() {
        let resp = build_response(vec![
            ContentBlock::Reasoning(ReasoningBlock {
                text: "step by step".to_string(),
                extensions: ExtensionMap::new(),
            }),
            ContentBlock::text("the answer is 42"),
        ]);

        let bytes = encode_with_clock(resp, Some(0)).expect("encode succeeds");
        let json: Value = serde_json::from_slice(&bytes).expect("valid JSON");

        let output = json["output"].as_array().expect("output is array");
        assert_eq!(output.len(), 2, "expected 2 output items");

        // First item: reasoning with id rs_0
        let first = &output[0];
        assert_eq!(first["type"], "reasoning");
        assert_eq!(first["id"], "rs_0");
        assert_eq!(first["status"], "completed");
        let first_content = first["content"].as_array().expect("content array");
        assert_eq!(first_content.len(), 1);
        assert_eq!(first_content[0]["type"], "reasoning");
        assert_eq!(first_content[0]["text"], "step by step");
        assert!(
            first_content[0].get("summary").is_none(),
            "summary field should be omitted when None, got: {first_content:?}"
        );

        // Second item: assistant message at msg_1 (msg_index advanced past reasoning)
        let second = &output[1];
        assert_eq!(second["type"], "message");
        assert_eq!(second["id"], "msg_1");
        assert_eq!(second["role"], "assistant");
        assert_eq!(second["status"], "completed");
        let second_content = second["content"].as_array().expect("content array");
        assert_eq!(second_content.len(), 1);
        assert_eq!(second_content[0]["type"], "output_text");
        assert_eq!(second_content[0]["text"], "the answer is 42");
    }

    #[test]
    fn unary_response_with_only_reasoning_block_emits_single_reasoning_item() {
        let resp = build_response(vec![ContentBlock::Reasoning(ReasoningBlock {
            text: "only thinking".to_string(),
            extensions: ExtensionMap::new(),
        })]);

        let bytes = encode_with_clock(resp, Some(0)).expect("encode succeeds");
        let json: Value = serde_json::from_slice(&bytes).expect("valid JSON");

        let output = json["output"].as_array().expect("output is array");
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["id"], "rs_0");
        assert_eq!(output[0]["content"][0]["text"], "only thinking");
        assert!(output[0]["content"][0].get("summary").is_none());
    }

    #[test]
    fn unary_response_text_then_reasoning_flushes_text_first() {
        // Verify that text accumulated before a reasoning block is flushed as msg_0
        // and the reasoning block then takes rs_1.
        let resp = build_response(vec![
            ContentBlock::text("preamble"),
            ContentBlock::Reasoning(ReasoningBlock {
                text: "thinking".to_string(),
                extensions: ExtensionMap::new(),
            }),
        ]);

        let bytes = encode_with_clock(resp, Some(0)).expect("encode succeeds");
        let json: Value = serde_json::from_slice(&bytes).expect("valid JSON");

        let output = json["output"].as_array().expect("output is array");
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["id"], "msg_0");
        assert_eq!(output[0]["content"][0]["text"], "preamble");
        assert_eq!(output[1]["type"], "reasoning");
        assert_eq!(output[1]["id"], "rs_1");
        assert_eq!(output[1]["content"][0]["text"], "thinking");
    }
}
