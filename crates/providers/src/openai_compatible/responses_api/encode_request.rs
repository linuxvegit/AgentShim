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

    // Instructions (collected from req.system; mid-conv System messages
    // appended after the loop — spec §4.3).
    let mut instructions: Vec<String> = req
        .system
        .iter()
        .map(|s| extract_text(&s.content))
        .collect();

    // Input items
    let mut input: Vec<Value> = Vec::new();
    let mut mid_conv_systems: Vec<String> = Vec::new();

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
            MessageRole::System => {
                // Responses API has no in-position system message shape.
                // Downgrade: collect text and append to top-level
                // instructions after the message loop (spec §4.3).
                let text = extract_text(&msg.content);
                if !text.is_empty() {
                    tracing::debug!(
                        source = ?msg.source,
                        "openai_responses provider: mid-conversation system message \
                         collapsed to top-level instructions (position lost)"
                    );
                    mid_conv_systems.push(text);
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

    // Assemble final instructions field (after collecting mid-conv).
    instructions.extend(mid_conv_systems);
    if !instructions.is_empty() {
        body["instructions"] = Value::String(instructions.join("\n"));
    }

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
    // Prefer the per-model catalog: when the target advertises a
    // `reasoning_effort` vocabulary (e.g. Copilot's `claude-opus-4.8` listing
    // `max`), clamp against it so the strongest supported tier is sent. Only
    // when the catalog is absent do we fall back to the static rule that pure
    // OpenAI Responses tops out at "high" (`Xhigh`/`Max` compress).
    if let Some(effort) = req.resolved_policy.reasoning_effort {
        let effort_str: String = req
            .resolved_policy
            .supported_efforts
            .as_deref()
            .and_then(|adv| effort.clamp_to_advertised(adv))
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                match effort {
                    ReasoningEffort::Xhigh | ReasoningEffort::Max => "high",
                    other => other.as_str(),
                }
                .to_string()
            });
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
        GenerationOptions, Message, RequestId, SystemInstruction, SystemSource,
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

    // ── catalog-driven clamping (per-model supported_efforts) ─────────

    fn req_with_catalog(effort: ReasoningEffort, advertised: &[&str]) -> CanonicalRequest {
        let mut req = empty_request();
        req.resolved_policy.reasoning_effort = Some(effort);
        req.resolved_policy.supported_efforts =
            Some(advertised.iter().map(|s| s.to_string()).collect());
        req
    }

    #[test]
    fn responses_max_passes_through_when_model_advertises_max() {
        // Copilot claude-opus-4.8 via the Responses path: Max must reach
        // upstream as "max", not the static "high" compression.
        let req = req_with_catalog(
            ReasoningEffort::Max,
            &["low", "medium", "high", "xhigh", "max"],
        );
        let body = build(&req, &target());
        assert_eq!(body["reasoning"]["effort"], "max");
    }

    #[test]
    fn responses_xhigh_steps_down_when_model_lacks_xhigh() {
        // opus-4.6: max but no xhigh → Xhigh clamps to "high".
        let req = req_with_catalog(ReasoningEffort::Xhigh, &["low", "medium", "high", "max"]);
        let body = build(&req, &target());
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn responses_max_clamps_to_xhigh_when_model_lacks_max() {
        // gpt-5.5: xhigh but no max → Max clamps to "xhigh".
        let req = req_with_catalog(
            ReasoningEffort::Max,
            &["none", "low", "medium", "high", "xhigh"],
        );
        let body = build(&req, &target());
        assert_eq!(body["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn responses_absent_catalog_falls_back_to_compression() {
        // No supported_efforts: static rule still compresses Max → "high".
        let mut req = empty_request();
        req.resolved_policy.reasoning_effort = Some(ReasoningEffort::Max);
        let body = build(&req, &target());
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    // ── positional MessageRole::System collapse (spec §4.3) ───────────

    #[test]
    fn responses_mid_conv_system_collapses_to_instructions() {
        let mut req = empty_request();
        req.messages = vec![
            Message::user(vec![ContentBlock::text("a")]),
            Message::system(SystemSource::OpenAiSystem, vec![ContentBlock::text("mid hint")]),
            Message::user(vec![ContentBlock::text("b")]),
        ];
        let body = build(&req, &target());
        assert_eq!(body["instructions"], "mid hint");
        // Input array MUST NOT contain a system/developer item.
        let input = body["input"].as_array().unwrap();
        for item in input {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
            assert!(role != "system" && role != "developer", "input contained system: {item}");
        }
    }

    #[test]
    fn responses_top_level_and_mid_conv_systems_concatenate_in_order() {
        let mut req = empty_request();
        req.system.push(SystemInstruction {
            source: SystemSource::OpenAiSystem,
            content: vec![ContentBlock::text("standing")],
        });
        req.messages = vec![
            Message::user(vec![ContentBlock::text("a")]),
            Message::system(SystemSource::OpenAiSystem, vec![ContentBlock::text("mid hint")]),
            Message::user(vec![ContentBlock::text("b")]),
        ];
        let body = build(&req, &target());
        // Top-level system vec entries first, then positional in message order.
        assert_eq!(body["instructions"], "standing\nmid hint");
    }

    #[test]
    fn responses_empty_system_message_text_skipped_in_collapse() {
        let mut req = empty_request();
        req.messages = vec![
            Message::user(vec![ContentBlock::text("a")]),
            Message::system(SystemSource::OpenAiSystem, vec![]),
        ];
        let body = build(&req, &target());
        // Nothing collapsed; instructions absent or empty.
        assert!(body.get("instructions").is_none() || body["instructions"] == "");
    }
}
