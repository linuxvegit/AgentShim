//! Build an OpenAI-compatible outbound request body from a CanonicalRequest.

use agent_shim_core::{
    request::{ReasoningEffort, ResponseFormat},
    BackendTarget, CanonicalRequest, ContentBlock, MessageRole, SystemSource, ToolCallArguments,
    ToolChoice,
};

use super::wire::{
    ChatBody, FunctionCallOut, FunctionDefOut, JsonSchemaOut, MsgOut, ResponseFormatOut,
    StreamOptions, ToolCallOut, ToolChoiceFunction, ToolChoiceOut, ToolOut,
};

/// Compress a canonical effort to whatever the OpenAI Chat-shape upstream accepts.
///
/// Pure OpenAI Chat tops out at `"high"`, so canonical `Xhigh` / `Max` get squashed.
/// Copilot, by contrast, accepts an `"xhigh"` extension on the Chat path — so when
/// `accepts_xhigh = true` we promote `Max` and pass `Xhigh` through.
fn effort_for_chat(e: ReasoningEffort, accepts_xhigh: bool) -> &'static str {
    use ReasoningEffort::*;
    match e {
        Minimal => "minimal",
        Low => "low",
        Medium => "medium",
        High => "high",
        Xhigh if accepts_xhigh => "xhigh",
        Xhigh => "high",
        Max if accepts_xhigh => "xhigh",
        Max => "high",
    }
}

/// Build the outbound OpenAI-Chat-shape request body as a `serde_json::Value`.
///
/// Public wrapper around [`build`] for callers outside the providers crate
/// (e.g. integration tests in `crates/protocol-tests/`). Returns JSON so the
/// crate's internal wire types stay `pub(crate)`.
pub fn build_json(
    req: &CanonicalRequest,
    target: &BackendTarget,
    accepts_xhigh: bool,
) -> serde_json::Value {
    serde_json::to_value(build(req, target, accepts_xhigh)).unwrap_or_default()
}

pub(crate) fn build(
    req: &CanonicalRequest,
    target: &BackendTarget,
    accepts_xhigh: bool,
) -> ChatBody {
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
        reasoning_effort: req
            .resolved_policy
            .reasoning_effort
            .map(|e| effort_for_chat(e, accepts_xhigh).to_string()),
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
                    BinarySource::Bytes { media_type, data } => {
                        // Same on the wire as Base64 — OpenAI Chat only
                        // understands data URIs, not opaque bytes. Treating
                        // Bytes as Base64 keeps the encoder lossless when a
                        // frontend hands us raw image bytes (e.g. Anthropic
                        // Messages → OpenAI Chat round-trip with an inline
                        // image).
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
                    BinarySource::ProviderFileId { file_id } => {
                        // OpenAI Chat doesn't have a `file_id` part for
                        // vision; this only makes sense when the provider
                        // is the SAME provider that minted the file_id
                        // (e.g. OpenAI → OpenAI passthrough). For the
                        // canonical OAI-compat encoder we drop with a
                        // warning rather than guessing — the upstream
                        // request will still complete, just without that
                        // image. Plan 04 T4 marks this as
                        // not-supported-in-v0.2.
                        tracing::warn!(
                            file_id = %file_id,
                            "BinarySource::ProviderFileId not supported by OpenAI-compat \
                             vision encoder; image part dropped"
                        );
                        None
                    }
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
        request::ReasoningEffort, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel,
        GenerationOptions, Message, RequestId, ToolCallId, ToolResultBlock,
    };

    fn target(model: &str) -> BackendTarget {
        BackendTarget {
            provider: "test".into(),
            model: model.into(),
            policy: Default::default(),
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
            inbound_anthropic_headers: vec![],
            resolved_policy: Default::default(),
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

        let body = build(&req, &target("gpt-test"), false);

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

        let body = build(&req, &target("gpt-test"), false);

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

    // ── vision encoder (Plan 04 T4) ───────────────────────────────────

    use agent_shim_core::{
        content::{ImageBlock, TextBlock},
        media::BinarySource,
    };

    #[test]
    fn image_url_part_emitted_for_url_source() {
        let req = request_with_messages(vec![Message::user(vec![
            ContentBlock::Text(TextBlock {
                text: "describe".into(),
                extensions: ExtensionMap::new(),
            }),
            ContentBlock::Image(ImageBlock {
                source: BinarySource::Url {
                    url: "https://example.com/cat.png".into(),
                },
                extensions: ExtensionMap::new(),
            }),
        ])]);
        let body = build(&req, &target("gpt-4o"), false);
        let content = body.messages[0]
            .content
            .as_ref()
            .expect("user content present");
        let parts = content.as_array().expect("multipart user content");
        // 0 = text, 1 = image_url
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "https://example.com/cat.png");
    }

    #[test]
    fn image_url_part_emits_data_uri_for_base64_source() {
        let req =
            request_with_messages(vec![Message::user(vec![ContentBlock::Image(ImageBlock {
                source: BinarySource::Base64 {
                    media_type: "image/png".into(),
                    data: bytes::Bytes::from_static(b"\x89PNG\r\n"),
                },
                extensions: ExtensionMap::new(),
            })])]);
        let body = build(&req, &target("gpt-4o"), false);
        let parts = body.messages[0]
            .content
            .as_ref()
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(parts[0]["type"], "image_url");
        let url = parts[0]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"), "got url: {url}");
        // Payload after the comma is the base64 of "\x89PNG\r\n" = "iVBORw0K".
        assert!(url.ends_with("iVBORw0K"));
    }

    #[test]
    fn image_bytes_source_encodes_as_base64_data_uri() {
        // Plan 04 T4: in-memory bytes are treated as base64 (the OpenAI
        // wire only understands data URIs). This makes the encoder
        // lossless when a frontend hands us raw bytes.
        let req =
            request_with_messages(vec![Message::user(vec![ContentBlock::Image(ImageBlock {
                source: BinarySource::Bytes {
                    media_type: "image/jpeg".into(),
                    data: bytes::Bytes::from_static(b"\xff\xd8\xff"),
                },
                extensions: ExtensionMap::new(),
            })])]);
        let body = build(&req, &target("gpt-4o"), false);
        let parts = body.messages[0]
            .content
            .as_ref()
            .unwrap()
            .as_array()
            .unwrap();
        let url = parts[0]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"), "got url: {url}");
    }

    #[test]
    fn image_provider_file_id_source_drops_with_warning() {
        // OpenAI Chat doesn't accept opaque file_ids in image_url parts.
        // The encoder warns and drops the part rather than fabricating a
        // wire shape that would fail at the upstream.
        let req = request_with_messages(vec![Message::user(vec![
            ContentBlock::Image(ImageBlock {
                source: BinarySource::ProviderFileId {
                    file_id: "file-abc".into(),
                },
                extensions: ExtensionMap::new(),
            }),
            ContentBlock::Text(TextBlock {
                text: "still here".into(),
                extensions: ExtensionMap::new(),
            }),
        ])]);
        let body = build(&req, &target("gpt-4o"), false);
        let parts = body.messages[0]
            .content
            .as_ref()
            .unwrap()
            .as_array()
            .unwrap();
        // The image part was dropped; only the text remains.
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "text");
    }

    // ── reasoning_effort compression (Plan A Task 7) ──────────────────

    fn req_with_effort(effort: ReasoningEffort) -> CanonicalRequest {
        let mut req = request_with_messages(vec![Message::user(vec![ContentBlock::text("hi")])]);
        req.resolved_policy.reasoning_effort = Some(effort);
        req
    }

    #[test]
    fn xhigh_serialises_as_xhigh_when_target_accepts() {
        let body = build(&req_with_effort(ReasoningEffort::Xhigh), &target("m"), true);
        assert_eq!(body.reasoning_effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn xhigh_compresses_to_high_when_target_rejects() {
        let body = build(
            &req_with_effort(ReasoningEffort::Xhigh),
            &target("m"),
            false,
        );
        assert_eq!(body.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn max_compresses_to_high_on_pure_openai() {
        let body = build(&req_with_effort(ReasoningEffort::Max), &target("m"), false);
        assert_eq!(body.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn max_serialises_as_xhigh_when_copilot() {
        // Canonical Max → Copilot xhigh: Copilot's top tier on the chat path.
        let body = build(&req_with_effort(ReasoningEffort::Max), &target("m"), true);
        assert_eq!(body.reasoning_effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn low_medium_high_pass_through_identically() {
        for (eff, want) in [
            (ReasoningEffort::Minimal, "minimal"),
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::High, "high"),
        ] {
            let body_off = build(&req_with_effort(eff), &target("m"), false);
            let body_on = build(&req_with_effort(eff), &target("m"), true);
            assert_eq!(body_off.reasoning_effort.as_deref(), Some(want));
            assert_eq!(body_on.reasoning_effort.as_deref(), Some(want));
        }
    }
}
