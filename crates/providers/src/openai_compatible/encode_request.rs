/// Build an OpenAI-compatible outbound request body from a CanonicalRequest.
use agent_shim_core::{
    request::ResponseFormat, CanonicalRequest, ContentBlock, MessageRole, SystemSource,
    ToolCallArguments, ToolChoice,
};

use super::wire::{
    ChatBody, FunctionCallOut, FunctionDefOut, JsonSchemaOut, MsgOut, ResponseFormatOut,
    StreamOptions, ToolCallOut, ToolChoiceFunction, ToolChoiceOut, ToolOut,
};

pub(crate) fn build(req: &CanonicalRequest, upstream_model: &str) -> ChatBody {
    let mut messages: Vec<MsgOut> = Vec::new();

    // System instructions become system/developer messages at the front.
    for sys in &req.system {
        let role = match sys.source {
            SystemSource::OpenAiDeveloper => "developer",
            _ => "system",
        };
        let text = extract_text_content(&sys.content);
        messages.push(MsgOut {
            role: role.to_string(),
            content: Some(serde_json::Value::String(text)),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        });
    }

    // Conversation messages.
    for msg in &req.messages {
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };

        // Collect tool calls from assistant messages.
        let tool_calls: Vec<ToolCallOut> = msg
            .content
            .iter()
            .filter_map(|b| {
                if let ContentBlock::ToolCall(tc) = b {
                    let args_str = match &tc.arguments {
                        ToolCallArguments::Complete { value } => value.to_string(),
                        ToolCallArguments::Streaming { data } => data.clone(),
                    };
                    Some(ToolCallOut {
                        id: tc.id.0.clone(),
                        r#type: "function".to_string(),
                        function: FunctionCallOut {
                            name: tc.name.clone(),
                            arguments: args_str,
                        },
                    })
                } else {
                    None
                }
            })
            .collect();

        // Collect ALL tool results — each becomes a separate "tool" role message in OpenAI format.
        let tool_results: Vec<_> = msg
            .content
            .iter()
            .filter_map(|b| {
                if let ContentBlock::ToolResult(tr) = b {
                    Some(tr)
                } else {
                    None
                }
            })
            .collect();

        if !tool_results.is_empty() {
            // Emit each tool result as its own message.
            for tr in &tool_results {
                let text_content = extract_text_from_tool_result(&tr.content);
                messages.push(MsgOut {
                    role: "tool".to_string(),
                    content: Some(serde_json::Value::String(text_content)),
                    name: msg.name.clone(),
                    tool_calls: None,
                    tool_call_id: Some(tr.tool_call_id.0.clone()),
                });
            }
        } else if !tool_calls.is_empty() {
            // Assistant message with tool calls — content may also have text.
            let text_content = build_text_content_value(&msg.content);
            messages.push(MsgOut {
                role: role.to_string(),
                content: text_content,
                name: msg.name.clone(),
                tool_calls: Some(tool_calls),
                tool_call_id: None,
            });
        } else {
            // Normal message.
            let content_val = build_content_value(&msg.content);
            messages.push(MsgOut {
                role: role.to_string(),
                content: content_val,
                name: msg.name.clone(),
                tool_calls: None,
                tool_call_id: None,
            });
        }
    }

    // Tools.
    let tools: Vec<ToolOut> = req
        .tools
        .iter()
        .map(|t| ToolOut {
            r#type: "function".to_string(),
            function: FunctionDefOut {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.input_schema.clone(),
            },
        })
        .collect();

    // Tool choice.
    let tool_choice = if tools.is_empty() {
        None
    } else {
        Some(match &req.tool_choice {
            ToolChoice::Auto => ToolChoiceOut::String("auto".to_string()),
            ToolChoice::None => ToolChoiceOut::String("none".to_string()),
            ToolChoice::Required => ToolChoiceOut::String("required".to_string()),
            ToolChoice::Specific { name } => ToolChoiceOut::Object {
                r#type: "function".to_string(),
                function: ToolChoiceFunction { name: name.clone() },
            },
        })
    };

    // Response format.
    let response_format = req.response_format.as_ref().map(|rf| match rf {
        ResponseFormat::Text => ResponseFormatOut::Text,
        ResponseFormat::JsonObject => ResponseFormatOut::JsonObject,
        ResponseFormat::JsonSchema {
            name,
            schema,
            strict,
        } => ResponseFormatOut::JsonSchema {
            json_schema: JsonSchemaOut {
                name: name.clone(),
                schema: schema.clone(),
                strict: *strict,
            },
        },
    });

    let stream_options = if req.stream {
        Some(StreamOptions {
            include_usage: true,
        })
    } else {
        None
    };

    ChatBody {
        model: upstream_model.to_string(),
        messages,
        max_tokens: req.generation.max_tokens,
        temperature: req.generation.temperature,
        top_p: req.generation.top_p,
        presence_penalty: req.generation.presence_penalty,
        frequency_penalty: req.generation.frequency_penalty,
        stop: req.generation.stop_sequences.clone(),
        seed: req.generation.seed,
        response_format,
        tools,
        tool_choice,
        stream: req.stream,
        stream_options,
    }
}

fn extract_text_content(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| {
            if let ContentBlock::Text(t) = b {
                Some(t.text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_text_content_value(blocks: &[ContentBlock]) -> Option<serde_json::Value> {
    let text = extract_text_content(blocks);
    if text.is_empty() {
        None
    } else {
        Some(serde_json::Value::String(text))
    }
}

fn build_content_value(blocks: &[ContentBlock]) -> Option<serde_json::Value> {
    // For simple text-only messages, use a plain string.
    let text_blocks: Vec<&str> = blocks
        .iter()
        .filter_map(|b| {
            if let ContentBlock::Text(t) = b {
                Some(t.text.as_str())
            } else {
                None
            }
        })
        .collect();

    let has_non_text = blocks.iter().any(|b| !matches!(b, ContentBlock::Text(_)));

    if !has_non_text && text_blocks.len() == 1 {
        return Some(serde_json::Value::String(text_blocks[0].to_string()));
    }

    if blocks.is_empty() {
        return None;
    }

    // Multi-part content array.
    let parts: Vec<serde_json::Value> = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(serde_json::json!({
                "type": "text",
                "text": t.text
            })),
            ContentBlock::Image(img) => {
                use agent_shim_core::BinarySource;
                match &img.source {
                    BinarySource::Base64 { media_type, data } => {
                        use base64::Engine as _;
                        let b64 = base64::engine::general_purpose::STANDARD.encode(data.as_ref());
                        Some(serde_json::json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{};base64,{}", media_type, b64)
                            }
                        }))
                    }
                    BinarySource::Url { url } => Some(serde_json::json!({
                        "type": "image_url",
                        "image_url": { "url": url }
                    })),
                    _ => None,
                }
            }
            // Skip tool_call/tool_result blocks — handled separately above
            _ => None,
        })
        .collect();

    if parts.is_empty() {
        None
    } else {
        Some(serde_json::Value::Array(parts))
    }
}

/// Extract text from a tool result's content value.
/// The content may be a plain string, an array of Anthropic-shaped blocks,
/// or a JSON value. We flatten it to a string for OpenAI-compatible APIs.
fn extract_text_from_tool_result(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            arr.iter()
                .filter_map(|item| {
                    // Anthropic blocks: {"type":"text","text":"..."}
                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                        Some(text.to_string())
                    } else if let Some(s) = item.as_str() {
                        Some(s.to_string())
                    } else {
                        Some(item.to_string())
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}
