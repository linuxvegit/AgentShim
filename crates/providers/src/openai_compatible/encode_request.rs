/// Build an OpenAI-compatible outbound request body from a CanonicalRequest.
use agent_shim_core::{
    request::{ReasoningEffort, ResponseFormat},
    BackendTarget, CanonicalRequest, ContentBlock, MessageRole, SystemSource, ToolCallArguments,
    ToolChoice,
};

use super::wire::{
    ChatBody, FunctionCallOut, FunctionDefOut, JsonSchemaOut, MsgOut, ResponseFormatOut,
    StreamOptions, ToolCallOut, ToolChoiceFunction, ToolChoiceOut, ToolOut,
};

pub(crate) fn build(req: &CanonicalRequest, target: &BackendTarget) -> ChatBody {
    let upstream_model = target.model.as_str();
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
            // Emit each tool result as its own "tool" role message FIRST.
            //
            // Tool results MUST immediately follow the assistant message that
            // contained the corresponding tool_use blocks. Backends like
            // GitHub Copilot's Vertex-Anthropic route enforce Anthropic's strict
            // ordering rule and reject requests where any other message (e.g.
            // a user text message) is interleaved between tool_use and
            // tool_result. So the tool messages come first, and any sibling
            // text content from the same user message is emitted afterwards.
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

            // Then emit any non-tool-result content (e.g. user text that
            // accompanied the tool result in the same canonical message)
            // as a separate message AFTER the tool replies.
            let non_tool_result_blocks = msg
                .content
                .iter()
                .filter(|b| !matches!(b, ContentBlock::ToolResult(_)))
                .cloned()
                .collect::<Vec<_>>();
            if msg.role != MessageRole::Tool {
                if let Some(content) = build_content_value(&non_tool_result_blocks) {
                    messages.push(MsgOut {
                        role: role.to_string(),
                        content: Some(content),
                        name: msg.name.clone(),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
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
        reasoning_effort: resolve_reasoning_effort(req, target).map(|e| e.as_str().to_string()),
    }
}

fn resolve_reasoning_effort(req: &CanonicalRequest, target: &BackendTarget) -> Option<ReasoningEffort> {
    req.generation
        .reasoning
        .as_ref()
        .and_then(|r| r.effort)
        .or(target.default_reasoning_effort)
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
                .map(|item| {
                    // Anthropic blocks: {"type":"text","text":"..."}
                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                        text.to_string()
                    } else if let Some(s) = item.as_str() {
                        s.to_string()
                    } else {
                        item.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        ExtensionMap, FrontendInfo, FrontendKind, FrontendModel, GenerationOptions, Message,
        RequestId, ToolCallId, ToolResultBlock,
    };

    fn target(model: &str) -> BackendTarget {
        BackendTarget {
            provider: "test".into(),
            model: model.into(),
            default_reasoning_effort: None,
            default_anthropic_beta: None,
        }
    }

    fn request_with_messages(messages: Vec<Message>) -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: FrontendModel::from("claude-test"),
            },
            model: FrontendModel::from("claude-test"),
            system: vec![],
            messages,
            tools: vec![],
            tool_choice: Default::default(),
            generation: GenerationOptions::default(),
            response_format: None,
            stream: false,
            metadata: Default::default(),
            extensions: ExtensionMap::new(),
        }
    }

    #[test]
    fn mixed_text_and_tool_result_preserves_user_text() {
        // When a user message has both text and a tool_result, the tool
        // message MUST be emitted first so it sits immediately after the
        // assistant's tool_use message. The accompanying user text follows
        // afterwards to satisfy Anthropic-strict backends (e.g. Copilot's
        // Vertex Claude route) which reject any other message between
        // tool_use and tool_result.
        let req = request_with_messages(vec![Message::user(vec![
            ContentBlock::text("The tool returned this:"),
            ContentBlock::ToolResult(ToolResultBlock {
                tool_call_id: ToolCallId::from_provider("call_1"),
                content: serde_json::json!("weather"),
                is_error: false,
                extensions: ExtensionMap::new(),
            }),
        ])]);

        let body = build(&req, &target("gpt-test"));

        assert_eq!(body.messages.len(), 2);
        assert_eq!(body.messages[0].role, "tool");
        assert_eq!(body.messages[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(body.messages[0].content, Some(serde_json::json!("weather")));
        assert_eq!(body.messages[1].role, "user");
        assert_eq!(
            body.messages[1].content,
            Some(serde_json::json!("The tool returned this:"))
        );
    }

    #[test]
    fn tool_result_immediately_follows_tool_use_message() {
        // Regression test for the bug where Copilot's Vertex Claude backend
        // returned: "tool_use ids were found without tool_result blocks
        // immediately after". Reproduces the exact ordering scenario:
        // assistant emits tool_use, then user replies with text + tool_result.
        // The encoded sequence must be assistant(tool_calls) -> tool, with no
        // user-text message interleaved.
        use agent_shim_core::{ToolCallArguments, ToolCallBlock};

        let req = request_with_messages(vec![
            Message {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::ToolCall(ToolCallBlock {
                    id: ToolCallId::from_provider("toolu_vrtx_01"),
                    name: "search".into(),
                    arguments: ToolCallArguments::Complete {
                        value: serde_json::json!({"q": "rust"}),
                    },
                    extensions: ExtensionMap::new(),
                })],
                name: None,
                extensions: ExtensionMap::new(),
            },
            Message::user(vec![
                ContentBlock::text("here is your result"),
                ContentBlock::ToolResult(ToolResultBlock {
                    tool_call_id: ToolCallId::from_provider("toolu_vrtx_01"),
                    content: serde_json::json!("hello"),
                    is_error: false,
                    extensions: ExtensionMap::new(),
                }),
            ]),
        ]);

        let body = build(&req, &target("gpt-test"));

        // Order MUST be: assistant(tool_calls), tool, user(text).
        assert_eq!(body.messages.len(), 3, "got: {:#?}", body.messages);
        assert_eq!(body.messages[0].role, "assistant");
        assert!(body.messages[0].tool_calls.is_some());
        assert_eq!(body.messages[1].role, "tool");
        assert_eq!(
            body.messages[1].tool_call_id.as_deref(),
            Some("toolu_vrtx_01")
        );
        assert_eq!(body.messages[2].role, "user");
    }
}
