use agent_shim_core::{
    content::{ContentBlock, ImageBlock, ReasoningBlock, RedactedReasoningBlock, TextBlock},
    extensions::ExtensionMap,
    ids::{RequestId, ToolCallId},
    media::BinarySource,
    message::{Message, MessageRole, SystemInstruction, SystemSource},
    request::{
        CanonicalRequest, GenerationOptions, ReasoningEffort, ReasoningOptions, RequestMetadata,
    },
    target::{FrontendInfo, FrontendKind, FrontendModel},
    tool::{ToolCallArguments, ToolCallBlock, ToolChoice, ToolDefinition, ToolResultBlock},
};
use serde_json::Value;

use super::mapping::role_from_anthropic;
use super::wire::{
    InboundContentBlock, InboundMessageContent, InboundToolChoice, MessagesRequest, SystemField,
    ToolResultContent,
};
use crate::FrontendError;

/// Decode a raw Anthropic Messages request body into a [`CanonicalRequest`].
///
/// Use this when you only have the raw bytes (e.g. from an HTTP handler that
/// hasn't deserialized the body yet). If you already have a [`MessagesRequest`]
/// struct in hand, prefer [`decode_request`] to avoid a redundant parse.
pub fn decode(body: &[u8]) -> Result<CanonicalRequest, FrontendError> {
    let req: MessagesRequest =
        serde_json::from_slice(body).map_err(|e| FrontendError::InvalidBody(e.to_string()))?;
    decode_request(req)
}

/// Decode an already-deserialized [`MessagesRequest`] into a [`CanonicalRequest`].
///
/// Use this when you already hold the typed struct — for example, after calling
/// [`CountTokensRequest::into_messages_request`] — to avoid serializing and
/// re-parsing the body a second time.
pub fn decode_request(req: MessagesRequest) -> Result<CanonicalRequest, FrontendError> {
    let model = FrontendModel(req.model.clone());

    // -- system --
    let mut system: Vec<SystemInstruction> = match req.system {
        None => vec![],
        Some(SystemField::Text(text)) => vec![SystemInstruction {
            source: SystemSource::AnthropicSystem,
            content: vec![ContentBlock::text(text)],
        }],
        Some(SystemField::Blocks(blocks)) => {
            let content = blocks
                .into_iter()
                .map(inbound_block_to_canonical)
                .collect::<Result<Vec<_>, _>>()?;
            vec![SystemInstruction {
                source: SystemSource::AnthropicSystem,
                content,
            }]
        }
    };

    // -- messages with prelude-fold rule (2026-06-10 spec §3.1) --
    // Leading-run role:"system" entries fold into the session-level
    // `system` vec, joining any entries already produced from the
    // top-level `system` field. The first non-system message ends the
    // prelude; later role:"system" entries stay positional as
    // `Message{role:System}`.
    let mut messages: Vec<Message> = Vec::with_capacity(req.messages.len());
    let mut prelude_phase = true;
    for m in req.messages {
        let role = role_from_anthropic(&m.role)
            .ok_or_else(|| FrontendError::InvalidBody(format!("unknown role: {}", m.role)))?;
        let content = match m.content {
            InboundMessageContent::Text(text) => vec![ContentBlock::text(text)],
            InboundMessageContent::Blocks(blocks) => blocks
                .into_iter()
                .map(inbound_block_to_canonical)
                .collect::<Result<Vec<_>, _>>()?,
        };
        match role {
            MessageRole::System if prelude_phase => {
                system.push(SystemInstruction {
                    source: SystemSource::AnthropicSystem,
                    content,
                });
            }
            MessageRole::System => {
                messages.push(Message {
                    role: MessageRole::System,
                    content,
                    name: None,
                    source: Some(SystemSource::AnthropicSystem),
                    extensions: ExtensionMap::new(),
                });
            }
            other => {
                prelude_phase = false;
                messages.push(Message {
                    role: other,
                    content,
                    name: None,
                    source: None,
                    extensions: ExtensionMap::new(),
                });
            }
        }
    }

    // -- tools --
    let tools = req
        .tools
        .unwrap_or_default()
        .into_iter()
        .map(|t| {
            let mut extensions = ExtensionMap::new();
            if let Some(cc) = t.cache_control {
                extensions.insert("cache_control", cc);
            }
            ToolDefinition {
                name: t.name,
                description: t.description,
                input_schema: t.input_schema,
                extensions,
            }
        })
        .collect();

    // -- tool_choice --
    let tool_choice = match req.tool_choice {
        None | Some(InboundToolChoice::Auto) => ToolChoice::Auto,
        Some(InboundToolChoice::Any) => ToolChoice::Required,
        Some(InboundToolChoice::None) => ToolChoice::None,
        Some(InboundToolChoice::Tool { name }) => ToolChoice::Specific { name },
    };

    // -- generation options --
    // Effort signal lives in output_config.effort (Anthropic `effort-2025-11-24`).
    // Legacy `thinking.budget_tokens` is intentionally NOT consumed for the
    // canonical effort axis — see 2026-05-28 spec.
    let reasoning = req
        .output_config
        .as_ref()
        .and_then(|oc| oc.effort.as_deref())
        .and_then(ReasoningEffort::parse)
        .map(|effort| ReasoningOptions {
            effort: Some(effort),
            budget_tokens: None,
        });
    let generation = GenerationOptions {
        max_tokens: Some(req.max_tokens),
        temperature: req.temperature,
        top_p: req.top_p,
        top_k: req.top_k,
        stop_sequences: req.stop_sequences.unwrap_or_default(),
        reasoning,
        ..Default::default()
    };

    let frontend = FrontendInfo {
        kind: FrontendKind::AnthropicMessages,
        requested_model: model.clone(),
    };

    Ok(CanonicalRequest {
        id: RequestId::new(),
        frontend,
        model,
        system,
        messages,
        tools,
        tool_choice,
        generation,
        response_format: None,
        stream: req.stream.unwrap_or(false),
        metadata: RequestMetadata::default(),
        inbound_anthropic_headers: vec![],
        resolved_policy: Default::default(),
        extensions: ExtensionMap::new(),
    })
}

fn inbound_block_to_canonical(block: InboundContentBlock) -> Result<ContentBlock, FrontendError> {
    match block {
        InboundContentBlock::Text {
            text,
            cache_control,
        } => {
            let mut extensions = ExtensionMap::new();
            if let Some(cc) = cache_control {
                extensions.insert("cache_control", cc);
            }
            Ok(ContentBlock::Text(TextBlock { text, extensions }))
        }
        InboundContentBlock::Image {
            source,
            cache_control,
        } => {
            // Anthropic's wire `source` shapes:
            //   {"type":"base64","media_type":"image/png","data":"<base64>"}
            //   {"type":"url","url":"https://..."}
            // Both match `BinarySource`'s JSON layout 1:1, so deserialize
            // straight through. If a future Anthropic format ships an
            // unrecognised `source.type`, fall back to the historic
            // pass-through-as-Unsupported behaviour so the request still
            // reaches the upstream untouched (the backend will reject it
            // with a real error rather than this gateway losing fidelity).
            let mut extensions = ExtensionMap::new();
            if let Some(cc) = cache_control.clone() {
                extensions.insert("cache_control", cc);
            }
            match serde_json::from_value::<BinarySource>(source.clone()) {
                Ok(bin) => Ok(ContentBlock::Image(ImageBlock {
                    source: bin,
                    extensions,
                })),
                Err(_) => {
                    let mut raw = serde_json::json!({ "type": "image", "source": source });
                    if let Some(cc) = cache_control {
                        raw["cache_control"] = cc;
                    }
                    Ok(ContentBlock::Unsupported(
                        agent_shim_core::content::UnsupportedBlock {
                            origin: "anthropic_messages".into(),
                            raw,
                        },
                    ))
                }
            }
        }
        InboundContentBlock::ToolUse {
            id,
            name,
            input,
            cache_control,
        } => {
            let mut extensions = ExtensionMap::new();
            if let Some(cc) = cache_control {
                extensions.insert("cache_control", cc);
            }
            Ok(ContentBlock::ToolCall(ToolCallBlock {
                id: ToolCallId::from_provider(id),
                name,
                arguments: ToolCallArguments::Complete { value: input },
                extensions,
            }))
        }
        InboundContentBlock::ToolResult {
            tool_use_id,
            is_error,
            content,
            cache_control,
        } => {
            let mut extensions = ExtensionMap::new();
            if let Some(cc) = cache_control {
                extensions.insert("cache_control", cc);
            }
            let content_value: Value = match content {
                None => Value::Null,
                Some(ToolResultContent::Text(t)) => Value::String(t),
                Some(ToolResultContent::Blocks(b)) => Value::Array(b),
            };
            Ok(ContentBlock::ToolResult(ToolResultBlock {
                tool_call_id: ToolCallId::from_provider(tool_use_id),
                content: content_value,
                is_error: is_error.unwrap_or(false),
                extensions,
            }))
        }
        InboundContentBlock::Thinking {
            thinking,
            signature,
        } => {
            let mut extensions = ExtensionMap::new();
            extensions.insert("signature", Value::String(signature));
            Ok(ContentBlock::Reasoning(ReasoningBlock {
                text: thinking,
                extensions,
            }))
        }
        InboundContentBlock::RedactedThinking { data } => {
            Ok(ContentBlock::RedactedReasoning(RedactedReasoningBlock {
                data,
                extensions: ExtensionMap::new(),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::message::MessageRole;

    fn minimal_request(extra: &str) -> Vec<u8> {
        format!(
            r#"{{"model":"claude-3-opus-20240229","max_tokens":1024,"messages":[{{"role":"user","content":"hello"}}]{}}}"#,
            extra
        )
        .into_bytes()
    }

    #[test]
    fn decode_minimal_text_request() {
        let body = minimal_request("");
        let req = decode(&body).unwrap();
        assert_eq!(req.model.as_str(), "claude-3-opus-20240229");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, MessageRole::User);
        assert_eq!(req.generation.max_tokens, Some(1024));
        assert!(!req.stream);
    }

    #[test]
    fn decode_stream_flag() {
        let body = minimal_request(r#","stream":true"#);
        let req = decode(&body).unwrap();
        assert!(req.stream);
    }

    #[test]
    fn decode_system_string() {
        let body = minimal_request(r#","system":"You are helpful.""#);
        let req = decode(&body).unwrap();
        assert_eq!(req.system.len(), 1);
        assert_eq!(req.system[0].source, SystemSource::AnthropicSystem);
        match &req.system[0].content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "You are helpful."),
            other => panic!("expected Text, got {:?}", other),
        }
    }

    #[test]
    fn decode_blocks_with_tool_use_and_tool_result() {
        let body = br#"{
            "model": "claude-3-opus-20240229",
            "max_tokens": 512,
            "messages": [
                {
                    "role": "user",
                    "content": [{"type":"text","text":"call search"}]
                },
                {
                    "role": "assistant",
                    "content": [{"type":"tool_use","id":"call_1","name":"search","input":{"q":"rust"}}]
                },
                {
                    "role": "user",
                    "content": [{"type":"tool_result","tool_use_id":"call_1","content":"result text"}]
                }
            ]
        }"#;
        let req = decode(body).unwrap();
        assert_eq!(req.messages.len(), 3);
        match &req.messages[1].content[0] {
            ContentBlock::ToolCall(tc) => {
                assert_eq!(tc.name, "search");
                assert_eq!(tc.id.0, "call_1");
            }
            other => panic!("expected ToolCall, got {:?}", other),
        }
        match &req.messages[2].content[0] {
            ContentBlock::ToolResult(tr) => {
                assert_eq!(tr.tool_call_id.0, "call_1");
                assert!(!tr.is_error);
            }
            other => panic!("expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn decode_truly_unknown_role_is_rejected() {
        let body = br#"{"model":"m","max_tokens":1,"messages":[{"role":"human","content":"hi"}]}"#;
        let err = decode(body).unwrap_err();
        assert!(matches!(err, FrontendError::InvalidBody(_)));
    }

    #[test]
    fn decode_leading_system_in_messages_folds_to_session_vec() {
        let body = br#"{
            "model":"claude-opus-4-8",
            "max_tokens":1024,
            "messages":[
                {"role":"system","content":"You are a poet."},
                {"role":"user","content":"go"}
            ]
        }"#;
        let req = decode(body).unwrap();
        assert_eq!(req.system.len(), 1);
        assert_eq!(req.system[0].source, SystemSource::AnthropicSystem);
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, MessageRole::User);
    }

    #[test]
    fn decode_tail_system_in_messages_preserved_as_positional() {
        let body = br#"{
            "model":"claude-opus-4-8",
            "max_tokens":1024,
            "messages":[
                {"role":"user","content":"hi"},
                {"role":"system","content":"hook injection"}
            ]
        }"#;
        let req = decode(body).unwrap();
        assert!(req.system.is_empty());
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, MessageRole::User);
        assert_eq!(req.messages[1].role, MessageRole::System);
        assert_eq!(req.messages[1].source, Some(SystemSource::AnthropicSystem));
        match &req.messages[1].content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "hook injection"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn decode_mid_system_in_messages_preserved_as_positional() {
        let body = br#"{
            "model":"claude-opus-4-8",
            "max_tokens":1024,
            "messages":[
                {"role":"user","content":"a"},
                {"role":"system","content":"shift gears"},
                {"role":"user","content":"b"}
            ]
        }"#;
        let req = decode(body).unwrap();
        assert!(req.system.is_empty());
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.messages[1].role, MessageRole::System);
    }

    #[test]
    fn decode_top_level_system_plus_leading_messages_system_concatenate() {
        let body = br#"{
            "model":"claude-opus-4-8",
            "max_tokens":1024,
            "system":"Standing X",
            "messages":[
                {"role":"system","content":"Leading Y"},
                {"role":"user","content":"hi"}
            ]
        }"#;
        let req = decode(body).unwrap();
        assert_eq!(req.system.len(), 2);
        match &req.system[0].content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "Standing X"),
            other => panic!("expected Text, got {other:?}"),
        }
        match &req.system[1].content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "Leading Y"),
            other => panic!("expected Text, got {other:?}"),
        }
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, MessageRole::User);
    }

    #[test]
    fn decode_system_with_blocks_content_yields_text_blocks() {
        let body = br#"{
            "model":"claude-opus-4-8",
            "max_tokens":1024,
            "messages":[
                {"role":"user","content":"a"},
                {"role":"system","content":[{"type":"text","text":"shift"}]}
            ]
        }"#;
        let req = decode(body).unwrap();
        assert_eq!(req.messages[1].role, MessageRole::System);
        match &req.messages[1].content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "shift"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn decode_tool_choice_required() {
        let body = minimal_request(r#","tool_choice":{"type":"any"}"#);
        let req = decode(&body).unwrap();
        assert_eq!(req.tool_choice, ToolChoice::Required);
    }

    #[test]
    fn decode_tool_choice_specific() {
        let body = minimal_request(r#","tool_choice":{"type":"tool","name":"search"}"#);
        let req = decode(&body).unwrap();
        assert_eq!(
            req.tool_choice,
            ToolChoice::Specific {
                name: "search".into()
            }
        );
    }

    // ── image blocks (Plan 04 T2) ─────────────────────────────────────

    #[test]
    fn decode_image_base64_source_yields_canonical_image_block() {
        // Anthropic wire shape:
        //   {"type":"image","source":{"type":"base64","media_type":"image/png","data":"<b64>"}}
        // Per Plan 04 T2 the decoder must turn this into a real
        // `ContentBlock::Image` (not `Unsupported`) so the capability gate
        // sees it.
        let body = br#"{
            "model":"claude-3-5-sonnet-20241022",
            "max_tokens":256,
            "messages":[{
                "role":"user",
                "content":[
                    {"type":"text","text":"describe"},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0K"}}
                ]
            }]
        }"#;
        let req = decode(body).unwrap();
        let blocks = &req.messages[0].content;
        assert_eq!(blocks.len(), 2);
        match &blocks[1] {
            ContentBlock::Image(img) => match &img.source {
                BinarySource::Base64 { media_type, data } => {
                    assert_eq!(media_type, "image/png");
                    // base64 of "iVBORw0K" decodes to a recognisable PNG header.
                    assert_eq!(&data[..4], b"\x89PNG");
                }
                other => panic!("expected Base64, got {other:?}"),
            },
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn decode_image_url_source_yields_canonical_image_block() {
        let body = br#"{
            "model":"claude-3-5-sonnet-20241022",
            "max_tokens":256,
            "messages":[{
                "role":"user",
                "content":[
                    {"type":"image","source":{"type":"url","url":"https://example.com/cat.png"}}
                ]
            }]
        }"#;
        let req = decode(body).unwrap();
        match &req.messages[0].content[0] {
            ContentBlock::Image(img) => match &img.source {
                BinarySource::Url { url } => assert_eq!(url, "https://example.com/cat.png"),
                other => panic!("expected Url, got {other:?}"),
            },
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn decode_image_with_unrecognised_source_falls_back_to_unsupported() {
        // Future Anthropic format — gateway shouldn't crash; preserves
        // raw block so the upstream can decide.
        let body = br#"{
            "model":"claude-3-5-sonnet-20241022",
            "max_tokens":256,
            "messages":[{
                "role":"user",
                "content":[
                    {"type":"image","source":{"type":"future_format","blob":"abc"}}
                ]
            }]
        }"#;
        let req = decode(body).unwrap();
        match &req.messages[0].content[0] {
            ContentBlock::Unsupported(u) => {
                assert_eq!(u.origin, "anthropic_messages");
                assert_eq!(u.raw["type"], "image");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn decode_image_carries_cache_control_into_extensions() {
        let body = br#"{
            "model":"claude-3-5-sonnet-20241022",
            "max_tokens":256,
            "messages":[{
                "role":"user",
                "content":[
                    {"type":"image","source":{"type":"url","url":"https://example.com/x.png"},"cache_control":{"type":"ephemeral"}}
                ]
            }]
        }"#;
        let req = decode(body).unwrap();
        match &req.messages[0].content[0] {
            ContentBlock::Image(img) => {
                let cc = img.extensions.get("cache_control");
                assert!(cc.is_some(), "cache_control must round-trip via extensions");
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    // ── effort vocabulary (2026-05-28 spec) ────────────────────────────

    #[test]
    fn output_config_effort_decodes_to_canonical() {
        use agent_shim_core::request::ReasoningEffort;
        let body = br#"{
            "model":"claude-opus-4.7",
            "max_tokens":1024,
            "messages":[{"role":"user","content":"hi"}],
            "thinking":{"type":"adaptive"},
            "output_config":{"effort":"max"}
        }"#;
        let req = decode(body).expect("decodes");
        let r = req.generation.reasoning.expect("reasoning set");
        assert_eq!(r.effort, Some(ReasoningEffort::Max));
        assert_eq!(r.budget_tokens, None);
    }

    #[test]
    fn legacy_thinking_budget_no_longer_populates_canonical() {
        // Per 2026-05-28 spec, legacy `thinking.budget_tokens` decode is retired.
        // Clients on the old API lose effort intent unless they also send
        // `output_config.effort`; route default catches the common case.
        let body = br#"{
            "model":"claude-opus-4.7",
            "max_tokens":1024,
            "messages":[{"role":"user","content":"hi"}],
            "thinking":{"type":"enabled","budget_tokens":8192}
        }"#;
        let req = decode(body).expect("decodes");
        assert!(
            req.generation.reasoning.is_none(),
            "legacy thinking.budget_tokens path is intentionally dropped per 2026-05-28 spec"
        );
    }

    #[test]
    fn adaptive_thinking_without_output_config_is_silent() {
        let body = br#"{
            "model":"claude-opus-4.7",
            "max_tokens":1024,
            "messages":[{"role":"user","content":"hi"}],
            "thinking":{"type":"adaptive"}
        }"#;
        let req = decode(body).expect("decodes");
        assert!(
            req.generation.reasoning.is_none(),
            "adaptive without output_config means no effort signal"
        );
    }
}
