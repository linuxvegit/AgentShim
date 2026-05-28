/// Build an OpenAI Responses API request body from a CanonicalRequest.
use agent_shim_core::{
    media::BinarySource,
    request::{CanonicalRequest, ReasoningEffort},
    BackendTarget, ContentBlock, MessageRole, ToolCallArguments, ToolChoice,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};

pub fn build(req: &CanonicalRequest, target: &BackendTarget) -> Value {
    let upstream_model = target.model.as_str();
    let mut body = json!({
        "model": upstream_model,
        "stream": req.stream,
    });

    // Instructions from system messages
    let instructions: Vec<String> = req
        .system
        .iter()
        .map(|s| extract_text(&s.content))
        .collect();
    if !instructions.is_empty() {
        body["instructions"] = Value::String(instructions.join("\n"));
    }

    // Input items
    let mut input: Vec<Value> = Vec::new();

    for msg in &req.messages {
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => {
                // Tool results become function_call_output items
                for block in &msg.content {
                    if let ContentBlock::ToolResult(tr) = block {
                        let output = match &tr.content {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": tr.tool_call_id.0,
                            "output": output,
                        }));
                    }
                }
                continue;
            }
        };

        // Check for tool calls in assistant messages
        let tool_calls: Vec<&agent_shim_core::ToolCallBlock> = msg
            .content
            .iter()
            .filter_map(|b| {
                if let ContentBlock::ToolCall(tc) = b {
                    Some(tc)
                } else {
                    None
                }
            })
            .collect();

        if !tool_calls.is_empty() {
            // Emit text as a message first if present
            let text = extract_text_only(&msg.content);
            if !text.is_empty() {
                input.push(json!({
                    "type": "message",
                    "role": role,
                    "content": text,
                }));
            }
            // Then emit each tool call as a function_call item
            for tc in tool_calls {
                let args = match &tc.arguments {
                    ToolCallArguments::Complete { value } => value.to_string(),
                    ToolCallArguments::Streaming { data } => data.clone(),
                };
                input.push(json!({
                    "type": "function_call",
                    "call_id": tc.id.0,
                    "name": tc.name,
                    "arguments": args,
                }));
            }
        } else {
            let content = build_message_content(&msg.content);
            input.push(json!({
                "type": "message",
                "role": role,
                "content": content,
            }));
        }
    }

    body["input"] = Value::Array(input);

    // Tools — both function tools and built-in tools
    let mut tools_arr: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            let mut tool = json!({
                "type": "function",
                "name": t.name,
                "parameters": t.input_schema,
            });
            if let Some(desc) = &t.description {
                tool["description"] = Value::String(desc.clone());
            }
            tool
        })
        .collect();

    // Append built-in tools (web_search, etc.) preserved from the frontend
    if let Some(Value::Array(builtin)) = req.extensions.get("builtin_tools") {
        tools_arr.extend(builtin.iter().cloned());
    }

    if !tools_arr.is_empty() {
        body["tools"] = Value::Array(tools_arr);
    }

    // Tool choice
    if !req.tools.is_empty() || req.extensions.get("builtin_tools").is_some() {
        match &req.tool_choice {
            ToolChoice::Auto => body["tool_choice"] = json!("auto"),
            ToolChoice::None => body["tool_choice"] = json!("none"),
            ToolChoice::Required => body["tool_choice"] = json!("required"),
            ToolChoice::Specific { name } => {
                body["tool_choice"] = json!({"type": "function", "name": name});
            }
        }
    }

    // Generation params
    if let Some(max) = req.generation.max_tokens {
        body["max_output_tokens"] = json!(max);
    }
    if let Some(temp) = req.generation.temperature {
        body["temperature"] = json!(temp);
    }
    if let Some(top_p) = req.generation.top_p {
        body["top_p"] = json!(top_p);
    }

    // Reasoning effort (Responses API uses `reasoning: { effort: "..." }`).
    // Responses tops out at "high" — `Xhigh` and `Max` compress.
    if let Some(effort) = req.resolved_policy.reasoning_effort {
        let effort_str = match effort {
            ReasoningEffort::Xhigh | ReasoningEffort::Max => "high",
            other => other.as_str(),
        };
        body["reasoning"] = json!({ "effort": effort_str });
    }

    body
}

fn extract_text(blocks: &[ContentBlock]) -> String {
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

fn extract_text_only(blocks: &[ContentBlock]) -> String {
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

/// Build the `content` field for a Responses-API `message` input item.
///
/// Plan 04 T4: text-only messages still serialize as a bare string (the
/// shape the upstream has always accepted), but messages carrying any
/// `ContentBlock::Image` switch to the parts array form
/// (`[{type:"input_text",...},{type:"input_image",image_url:"..."}]`).
/// This is the wire shape the OpenAI Responses API documents for vision
/// — note `image_url` is a STRING here, NOT the `{url:"..."}` object the
/// Chat Completions API uses.
///
/// `BinarySource::Url` round-trips as the original URL; `Base64`/`Bytes`
/// land as a `data:<mime>;base64,<payload>` data URI;
/// `ProviderFileId` is dropped with a warn (the Responses API has no
/// opaque file_id image source today).
fn build_message_content(blocks: &[ContentBlock]) -> Value {
    let has_image = blocks.iter().any(|b| matches!(b, ContentBlock::Image(_)));
    if !has_image {
        return Value::String(extract_text(blocks));
    }

    let parts: Vec<Value> = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(json!({
                "type": "input_text",
                "text": t.text,
            })),
            ContentBlock::Image(img) => match &img.source {
                BinarySource::Url { url } => Some(json!({
                    "type": "input_image",
                    "image_url": url,
                })),
                BinarySource::Base64 { media_type, data } => {
                    let b64 = STANDARD.encode(data.as_ref());
                    Some(json!({
                        "type": "input_image",
                        "image_url": format!("data:{};base64,{}", media_type, b64),
                    }))
                }
                BinarySource::Bytes { media_type, data } => {
                    // Same wire shape as Base64 — Responses API accepts
                    // data URIs but not raw byte handles. Encoding here
                    // keeps round-trips lossless when a frontend hands us
                    // raw bytes (e.g. multipart upload).
                    let b64 = STANDARD.encode(data.as_ref());
                    Some(json!({
                        "type": "input_image",
                        "image_url": format!("data:{};base64,{}", media_type, b64),
                    }))
                }
                BinarySource::ProviderFileId { file_id } => {
                    tracing::warn!(
                        file_id = %file_id,
                        "BinarySource::ProviderFileId not supported by OpenAI Responses \
                         vision encoder; image part dropped"
                    );
                    None
                }
            },
            // Tool calls / tool results are emitted as separate items in
            // the outer loop, not as message content parts.
            _ => None,
        })
        .collect();

    Value::Array(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        request::ReasoningEffort, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel,
        GenerationOptions, Message, RequestId,
    };

    fn target() -> BackendTarget {
        BackendTarget {
            provider: "openai".into(),
            model: "gpt-5".into(),
            policy: Default::default(),
        }
    }

    fn empty_request() -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::OpenAiResponses,
                requested_model: FrontendModel::from("gpt-5"),
            },
            model: FrontendModel::from("gpt-5"),
            system: vec![],
            messages: vec![Message::user(vec![ContentBlock::text("hi")])],
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
    fn responses_xhigh_compresses_to_high() {
        let mut req = empty_request();
        req.resolved_policy.reasoning_effort = Some(ReasoningEffort::Xhigh);
        let body = build(&req, &target());
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn responses_max_compresses_to_high() {
        let mut req = empty_request();
        req.resolved_policy.reasoning_effort = Some(ReasoningEffort::Max);
        let body = build(&req, &target());
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn responses_low_medium_high_pass_through() {
        for (eff, want) in [
            (ReasoningEffort::Minimal, "minimal"),
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::High, "high"),
        ] {
            let mut req = empty_request();
            req.resolved_policy.reasoning_effort = Some(eff);
            let body = build(&req, &target());
            assert_eq!(body["reasoning"]["effort"], want);
        }
    }

    #[test]
    fn responses_no_effort_means_no_reasoning_field() {
        let body = build(&empty_request(), &target());
        assert!(body.get("reasoning").is_none());
    }
}
