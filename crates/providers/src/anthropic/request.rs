//! Build the Anthropic Messages JSON request body from a [`CanonicalRequest`].
//!
//! Mirror image of `frontends::anthropic_messages::decode::decode`. The input
//! is a canonical request shape that may have arrived from any frontend
//! (OpenAI Chat, OpenAI Responses, even Anthropic if a route deliberately
//! forces the canonical path); the output is a `serde_json::Value` shaped
//! exactly the way `/v1/messages` expects it.
//!
//! Anthropic-specific data carried via canonical extension keys
//! (`anthropic.cache_control`, `anthropic.signature`) round-trips here.

use agent_shim_core::{
    content::ContentBlock,
    mapping::anthropic_wire::role_to_anthropic,
    media::BinarySource,
    message::{Message, SystemInstruction},
    request::{CanonicalRequest, ReasoningEffort},
    target::BackendTarget,
    tool::{ToolCallArguments, ToolChoice},
};
use serde_json::{json, Value};

use super::wire::{
    OutgoingContentBlock, OutgoingMessage, OutgoingRequest, OutgoingSystem, OutgoingThinking,
    OutgoingTool, OutgoingToolChoice, OutgoingToolResultContent,
};

/// Anthropic API requires `max_tokens`. Use this when the canonical request
/// didn't carry one (cross-protocol case from OpenAI inbound where it's
/// optional).
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Extension keys (namespace `anthropic.*`) carried through the canonical
/// model that need to be lifted onto the outbound wire shape.
///
/// Extension key conventions: provider canonical path reads BOTH the new
/// namespaced keys (`anthropic.cache_control`, `anthropic.signature`) AND
/// the unprefixed legacy keys (`cache_control`, `signature`) that the
/// existing Anthropic frontend writes during decode. This preserves
/// round-trip fidelity for Anthropic-frontend → canonical → Anthropic-provider
/// flows. A future plan will migrate the frontend to write the namespaced
/// keys, at which point the legacy aliases here can be removed.
const EXT_CACHE_CONTROL: &str = "anthropic.cache_control";
/// Backwards-compatible alias used by the Anthropic frontend's decoder.
/// See [`EXT_CACHE_CONTROL`] for the dual-read rationale.
const EXT_CACHE_CONTROL_LEGACY: &str = "cache_control";
const EXT_SIGNATURE: &str = "anthropic.signature";
/// Backwards-compatible alias used by the Anthropic frontend's decoder.
/// See [`EXT_CACHE_CONTROL`] for the dual-read rationale.
const EXT_SIGNATURE_LEGACY: &str = "signature";

pub fn build(req: &CanonicalRequest, target: &BackendTarget) -> Value {
    let messages: Vec<OutgoingMessage> = req.messages.iter().map(message_to_outgoing).collect();

    let system = build_system(&req.system);
    let tools = build_tools(req);
    let tool_choice = build_tool_choice(&req.tool_choice);
    let thinking = build_thinking(req);
    let metadata = build_metadata(req);
    let max_tokens = req.generation.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

    let stop_sequences = if req.generation.stop_sequences.is_empty() {
        None
    } else {
        Some(req.generation.stop_sequences.clone())
    };

    let outgoing = OutgoingRequest {
        model: target.model.clone(),
        messages,
        system,
        tools,
        tool_choice,
        max_tokens,
        temperature: req.generation.temperature,
        top_p: req.generation.top_p,
        top_k: req.generation.top_k,
        stop_sequences,
        stream: req.stream,
        metadata,
        thinking,
    };

    serde_json::to_value(&outgoing).unwrap_or(Value::Null)
}

// ── system ──────────────────────────────────────────────────────────────────

fn build_system(system: &[SystemInstruction]) -> Option<OutgoingSystem> {
    if system.is_empty() {
        return None;
    }

    // If every system instruction is a single text block with no extensions,
    // emit the simple string form (joined by "\n\n" across multiple
    // instructions — the cross-protocol case).
    let all_simple_text = system
        .iter()
        .all(|si| si.content.iter().all(is_simple_text_block));
    if all_simple_text {
        let joined = system
            .iter()
            .flat_map(|si| si.content.iter())
            .filter_map(|block| match block {
                ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if joined.is_empty() {
            return None;
        }
        return Some(OutgoingSystem::Text(joined));
    }

    // Otherwise emit the block-array form, preserving extensions like
    // cache_control on individual blocks.
    let blocks: Vec<OutgoingContentBlock> = system
        .iter()
        .flat_map(|si| si.content.iter())
        .filter_map(content_block_to_outgoing)
        .collect();
    if blocks.is_empty() {
        None
    } else {
        Some(OutgoingSystem::Blocks(blocks))
    }
}

fn is_simple_text_block(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Text(t) => t.extensions.is_empty(),
        _ => false,
    }
}

// ── messages ────────────────────────────────────────────────────────────────

fn message_to_outgoing(msg: &Message) -> OutgoingMessage {
    let role = role_to_anthropic(msg.role);
    let content: Vec<OutgoingContentBlock> = msg
        .content
        .iter()
        .filter_map(content_block_to_outgoing)
        .collect();
    OutgoingMessage { role, content }
}

fn content_block_to_outgoing(block: &ContentBlock) -> Option<OutgoingContentBlock> {
    match block {
        ContentBlock::Text(t) => Some(OutgoingContentBlock::Text {
            text: t.text.clone(),
            cache_control: cache_control_from_ext(&t.extensions),
        }),
        ContentBlock::Image(img) => image_block_to_outgoing(img),
        ContentBlock::ToolCall(tc) => {
            let input = match &tc.arguments {
                ToolCallArguments::Complete { value } => value.clone(),
                ToolCallArguments::Streaming { data } => {
                    serde_json::from_str::<Value>(data).unwrap_or_else(|_| json!({}))
                }
            };
            Some(OutgoingContentBlock::ToolUse {
                id: tc.id.0.clone(),
                name: tc.name.clone(),
                input,
                cache_control: cache_control_from_ext(&tc.extensions),
            })
        }
        ContentBlock::ToolResult(tr) => {
            let content = tool_result_content(&tr.content);
            let is_error = if tr.is_error { Some(true) } else { None };
            Some(OutgoingContentBlock::ToolResult {
                tool_use_id: tr.tool_call_id.0.clone(),
                is_error,
                content,
                cache_control: cache_control_from_ext(&tr.extensions),
            })
        }
        ContentBlock::Reasoning(r) => Some(OutgoingContentBlock::Thinking {
            thinking: r.text.clone(),
            signature: signature_from_ext(&r.extensions),
        }),
        ContentBlock::RedactedReasoning(r) => Some(OutgoingContentBlock::RedactedThinking {
            data: r.data.clone(),
        }),
        ContentBlock::Audio(_) | ContentBlock::File(_) => {
            tracing::debug!("anthropic provider: skipping unsupported content block kind for v0.2");
            None
        }
        ContentBlock::Unsupported(_) => {
            tracing::debug!(
                "anthropic provider: skipping Unsupported content block (Plan 04 will preserve it)"
            );
            None
        }
    }
}

fn image_block_to_outgoing(
    img: &agent_shim_core::content::ImageBlock,
) -> Option<OutgoingContentBlock> {
    let source = match &img.source {
        BinarySource::Base64 { media_type, data } => {
            use base64::{engine::general_purpose::STANDARD, Engine as _};
            let encoded = STANDARD.encode(data);
            json!({
                "type": "base64",
                "media_type": media_type,
                "data": encoded,
            })
        }
        BinarySource::Url { url } => json!({
            "type": "url",
            "url": url,
        }),
        BinarySource::Bytes { .. } | BinarySource::ProviderFileId { .. } => {
            tracing::debug!(
                "anthropic provider: skipping image with non-Base64/Url BinarySource for v0.2"
            );
            return None;
        }
    };
    Some(OutgoingContentBlock::Image {
        source,
        cache_control: cache_control_from_ext(&img.extensions),
    })
}

fn tool_result_content(content: &Value) -> Option<OutgoingToolResultContent> {
    match content {
        Value::Null => None,
        Value::String(s) => Some(OutgoingToolResultContent::Text(s.clone())),
        Value::Array(arr) => Some(OutgoingToolResultContent::Blocks(arr.clone())),
        // For object/scalar shapes, stringify into a text block — Anthropic's
        // tool_result content field accepts text or a block-array, not a free
        // JSON object.
        other => Some(OutgoingToolResultContent::Text(other.to_string())),
    }
}

// ── tools ───────────────────────────────────────────────────────────────────

fn build_tools(req: &CanonicalRequest) -> Option<Vec<OutgoingTool>> {
    if req.tools.is_empty() {
        return None;
    }
    let tools: Vec<OutgoingTool> = req
        .tools
        .iter()
        .map(|t| OutgoingTool {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
            cache_control: cache_control_from_ext(&t.extensions),
        })
        .collect();
    Some(tools)
}

fn build_tool_choice(choice: &ToolChoice) -> Option<OutgoingToolChoice> {
    match choice {
        ToolChoice::Auto => None,
        ToolChoice::Required => Some(OutgoingToolChoice::Any),
        ToolChoice::None => Some(OutgoingToolChoice::None),
        ToolChoice::Specific { name } => Some(OutgoingToolChoice::Tool { name: name.clone() }),
    }
}

// ── thinking ────────────────────────────────────────────────────────────────

fn build_thinking(req: &CanonicalRequest) -> Option<OutgoingThinking> {
    let reasoning = req.generation.reasoning.as_ref()?;
    let budget = reasoning
        .budget_tokens
        .or_else(|| reasoning.effort.map(effort_to_budget))?;
    Some(OutgoingThinking {
        ty: "enabled",
        budget_tokens: budget,
    })
}

fn effort_to_budget(effort: ReasoningEffort) -> u32 {
    match effort {
        ReasoningEffort::Minimal => 128,
        ReasoningEffort::Low => 512,
        ReasoningEffort::Medium => 2048,
        ReasoningEffort::High => 8192,
        ReasoningEffort::Xhigh => 16384,
    }
}

// ── metadata ────────────────────────────────────────────────────────────────

fn build_metadata(req: &CanonicalRequest) -> Option<Value> {
    let user_id = req.metadata.user_id.as_ref()?;
    Some(json!({ "user_id": user_id }))
}

// ── extension helpers ───────────────────────────────────────────────────────

fn cache_control_from_ext(ext: &agent_shim_core::ExtensionMap) -> Option<Value> {
    ext.get(EXT_CACHE_CONTROL)
        .or_else(|| ext.get(EXT_CACHE_CONTROL_LEGACY))
        .cloned()
}

fn signature_from_ext(ext: &agent_shim_core::ExtensionMap) -> Option<String> {
    ext.get(EXT_SIGNATURE)
        .or_else(|| ext.get(EXT_SIGNATURE_LEGACY))
        .and_then(|v| v.as_str().map(|s| s.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        content::{ImageBlock, ReasoningBlock, TextBlock},
        extensions::ExtensionMap,
        ids::{RequestId, ToolCallId},
        media::BinarySource,
        message::{Message, MessageRole, SystemInstruction, SystemSource},
        request::{
            CanonicalRequest, GenerationOptions, ReasoningEffort, ReasoningOptions, RequestMetadata,
        },
        target::{FrontendInfo, FrontendKind, FrontendModel},
        tool::{ToolCallBlock, ToolDefinition, ToolResultBlock},
    };

    fn empty_request(stream: bool) -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::OpenAiChat,
                requested_model: FrontendModel::from("gpt-4o"),
            },
            model: FrontendModel::from("gpt-4o"),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            generation: GenerationOptions::default(),
            response_format: None,
            stream,
            metadata: RequestMetadata::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: Default::default(),
            extensions: ExtensionMap::new(),
        }
    }

    fn target() -> BackendTarget {
        BackendTarget {
            provider: "anthropic".into(),
            model: "claude-3-5-sonnet-20241022".into(),
            policy: Default::default(),
        }
    }

    #[test]
    fn text_only_user_message_serializes_correctly() {
        let mut req = empty_request(false);
        req.messages
            .push(Message::user(vec![ContentBlock::text("hello, world")]));
        let body = build(&req, &target());
        assert_eq!(body["model"], "claude-3-5-sonnet-20241022");
        assert_eq!(body["max_tokens"], 4096); // default fallback
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hello, world");
        assert!(body.get("system").is_none());
        assert!(body.get("tools").is_none());
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn max_tokens_passes_through_when_set() {
        let mut req = empty_request(true);
        req.generation.max_tokens = Some(1024);
        req.messages
            .push(Message::user(vec![ContentBlock::text("x")]));
        let body = build(&req, &target());
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn system_text_string_form() {
        let mut req = empty_request(false);
        req.system.push(SystemInstruction {
            source: SystemSource::AnthropicSystem,
            content: vec![ContentBlock::text("You are helpful.")],
        });
        req.messages
            .push(Message::user(vec![ContentBlock::text("hi")]));
        let body = build(&req, &target());
        assert_eq!(body["system"], "You are helpful.");
    }

    #[test]
    fn multi_system_flattens_with_double_newline() {
        let mut req = empty_request(false);
        req.system.push(SystemInstruction {
            source: SystemSource::OpenAiSystem,
            content: vec![ContentBlock::text("Be concise.")],
        });
        req.system.push(SystemInstruction {
            source: SystemSource::OpenAiDeveloper,
            content: vec![ContentBlock::text("Use British English.")],
        });
        req.messages
            .push(Message::user(vec![ContentBlock::text("hi")]));
        let body = build(&req, &target());
        assert_eq!(body["system"], "Be concise.\n\nUse British English.");
    }

    #[test]
    fn system_with_cache_control_uses_block_array() {
        let mut req = empty_request(false);
        let mut ext = ExtensionMap::new();
        ext.insert(EXT_CACHE_CONTROL, json!({"type": "ephemeral"}));
        req.system.push(SystemInstruction {
            source: SystemSource::AnthropicSystem,
            content: vec![ContentBlock::Text(TextBlock {
                text: "Cache me".into(),
                extensions: ext,
            })],
        });
        req.messages
            .push(Message::user(vec![ContentBlock::text("hi")]));
        let body = build(&req, &target());
        assert!(body["system"].is_array());
        assert_eq!(body["system"][0]["type"], "text");
        assert_eq!(body["system"][0]["text"], "Cache me");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn tool_use_and_tool_result_round_trip() {
        let mut req = empty_request(false);
        req.messages
            .push(Message::user(vec![ContentBlock::text("search please")]));
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolCall(ToolCallBlock {
                id: ToolCallId::from_provider("call_1"),
                name: "search".into(),
                arguments: ToolCallArguments::Complete {
                    value: json!({"q": "rust"}),
                },
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        req.messages.push(Message {
            role: MessageRole::Tool,
            content: vec![ContentBlock::ToolResult(ToolResultBlock {
                tool_call_id: ToolCallId::from_provider("call_1"),
                content: Value::String("ok".into()),
                is_error: false,
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let body = build(&req, &target());

        // Assistant tool_use
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][1]["content"][0]["id"], "call_1");
        assert_eq!(body["messages"][1]["content"][0]["name"], "search");
        assert_eq!(body["messages"][1]["content"][0]["input"]["q"], "rust");

        // Tool result is mapped to user role per role_to_anthropic
        assert_eq!(body["messages"][2]["role"], "user");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(body["messages"][2]["content"][0]["content"], "ok");
    }

    #[test]
    fn tool_definitions_serialize() {
        let mut req = empty_request(false);
        req.messages
            .push(Message::user(vec![ContentBlock::text("hi")]));
        let mut ext = ExtensionMap::new();
        ext.insert(EXT_CACHE_CONTROL, json!({"type": "ephemeral"}));
        req.tools.push(ToolDefinition {
            name: "search".into(),
            description: Some("search the web".into()),
            input_schema: json!({"type": "object"}),
            extensions: ext,
        });
        req.tool_choice = ToolChoice::Specific {
            name: "search".into(),
        };
        let body = build(&req, &target());
        assert_eq!(body["tools"][0]["name"], "search");
        assert_eq!(body["tools"][0]["description"], "search the web");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tool_choice"]["type"], "tool");
        assert_eq!(body["tool_choice"]["name"], "search");
    }

    #[test]
    fn tool_choice_required_maps_to_any() {
        let mut req = empty_request(false);
        req.messages
            .push(Message::user(vec![ContentBlock::text("hi")]));
        req.tool_choice = ToolChoice::Required;
        let body = build(&req, &target());
        assert_eq!(body["tool_choice"]["type"], "any");
    }

    #[test]
    fn tool_choice_auto_is_omitted() {
        let mut req = empty_request(false);
        req.messages
            .push(Message::user(vec![ContentBlock::text("hi")]));
        req.tool_choice = ToolChoice::Auto;
        let body = build(&req, &target());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn thinking_with_explicit_budget_is_emitted() {
        let mut req = empty_request(false);
        req.messages
            .push(Message::user(vec![ContentBlock::text("hi")]));
        req.generation.reasoning = Some(ReasoningOptions {
            effort: None,
            budget_tokens: Some(4096),
        });
        let body = build(&req, &target());
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 4096);
    }

    #[test]
    fn thinking_effort_maps_to_budget() {
        let mut req = empty_request(false);
        req.messages
            .push(Message::user(vec![ContentBlock::text("hi")]));
        req.generation.reasoning = Some(ReasoningOptions {
            effort: Some(ReasoningEffort::High),
            budget_tokens: None,
        });
        let body = build(&req, &target());
        assert_eq!(body["thinking"]["budget_tokens"], 8192);
    }

    #[test]
    fn thinking_omitted_when_neither_field_set() {
        let mut req = empty_request(false);
        req.messages
            .push(Message::user(vec![ContentBlock::text("hi")]));
        req.generation.reasoning = Some(ReasoningOptions::default());
        let body = build(&req, &target());
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn reasoning_block_round_trips_signature_via_extensions() {
        let mut req = empty_request(false);
        let mut ext = ExtensionMap::new();
        ext.insert(EXT_SIGNATURE, Value::String("sig-abc".into()));
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Reasoning(ReasoningBlock {
                text: "thinking out loud".into(),
                extensions: ext,
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let body = build(&req, &target());
        assert_eq!(body["messages"][0]["content"][0]["type"], "thinking");
        assert_eq!(
            body["messages"][0]["content"][0]["thinking"],
            "thinking out loud"
        );
        assert_eq!(body["messages"][0]["content"][0]["signature"], "sig-abc");
    }

    #[test]
    fn thinking_block_omits_empty_signature() {
        // A Reasoning block without a signature extension must produce a wire
        // block where the `signature` field is ABSENT (not `""`). Anthropic
        // rejects extended-thinking blocks with empty signatures during
        // multi-turn replays.
        let mut req = empty_request(false);
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Reasoning(ReasoningBlock {
                text: "thinking out loud".into(),
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let body = build(&req, &target());
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "thinking");
        assert_eq!(block["thinking"], "thinking out loud");
        let obj = block.as_object().expect("thinking block is an object");
        assert!(
            !obj.contains_key("signature"),
            "signature field must be omitted when no extension is present, got: {block:?}"
        );
    }

    #[test]
    fn image_base64_source_serializes() {
        let mut req = empty_request(false);
        req.messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Image(ImageBlock {
                source: BinarySource::Base64 {
                    media_type: "image/png".into(),
                    data: bytes::Bytes::from_static(b"\x89PNG"),
                },
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let body = build(&req, &target());
        assert_eq!(body["messages"][0]["content"][0]["type"], "image");
        assert_eq!(
            body["messages"][0]["content"][0]["source"]["type"],
            "base64"
        );
        assert_eq!(
            body["messages"][0]["content"][0]["source"]["media_type"],
            "image/png"
        );
        assert!(body["messages"][0]["content"][0]["source"]["data"].is_string());
    }

    #[test]
    fn image_url_source_serializes() {
        let mut req = empty_request(false);
        req.messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Image(ImageBlock {
                source: BinarySource::Url {
                    url: "https://example.com/cat.png".into(),
                },
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let body = build(&req, &target());
        assert_eq!(body["messages"][0]["content"][0]["source"]["type"], "url");
        assert_eq!(
            body["messages"][0]["content"][0]["source"]["url"],
            "https://example.com/cat.png"
        );
    }

    #[test]
    fn cache_control_lifts_to_top_level_field_on_text_block() {
        let mut req = empty_request(false);
        let mut ext = ExtensionMap::new();
        ext.insert(EXT_CACHE_CONTROL, json!({"type": "ephemeral"}));
        req.messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text(TextBlock {
                text: "cached".into(),
                extensions: ext,
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let body = build(&req, &target());
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "text");
        assert_eq!(block["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn metadata_user_id_passes_through() {
        let mut req = empty_request(false);
        req.metadata.user_id = Some("user-42".into());
        req.messages
            .push(Message::user(vec![ContentBlock::text("hi")]));
        let body = build(&req, &target());
        assert_eq!(body["metadata"]["user_id"], "user-42");
    }

    #[test]
    fn metadata_omitted_when_no_user_id() {
        let mut req = empty_request(false);
        req.messages
            .push(Message::user(vec![ContentBlock::text("hi")]));
        let body = build(&req, &target());
        assert!(body.get("metadata").is_none());
    }

    #[test]
    fn streaming_tool_arguments_parsed_to_value() {
        let mut req = empty_request(false);
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolCall(ToolCallBlock {
                id: ToolCallId::from_provider("call_1"),
                name: "search".into(),
                arguments: ToolCallArguments::Streaming {
                    data: r#"{"q":"rust"}"#.into(),
                },
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let body = build(&req, &target());
        assert_eq!(body["messages"][0]["content"][0]["input"]["q"], "rust");
    }

    #[test]
    fn streaming_tool_arguments_with_invalid_json_falls_back_to_empty_object() {
        let mut req = empty_request(false);
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolCall(ToolCallBlock {
                id: ToolCallId::from_provider("call_1"),
                name: "search".into(),
                arguments: ToolCallArguments::Streaming {
                    data: "not-json".into(),
                },
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let body = build(&req, &target());
        assert!(body["messages"][0]["content"][0]["input"].is_object());
        assert_eq!(body["messages"][0]["content"][0]["input"], json!({}));
    }
}
