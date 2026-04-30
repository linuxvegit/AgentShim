use agent_shim_core::{
    content::{ContentBlock, ReasoningBlock, RedactedReasoningBlock, TextBlock},
    extensions::ExtensionMap,
    ids::{RequestId, ToolCallId},
    message::{Message, SystemInstruction, SystemSource},
    request::{CanonicalRequest, GenerationOptions, ReasoningOptions, RequestMetadata},
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

pub fn decode(body: &[u8]) -> Result<CanonicalRequest, FrontendError> {
    let req: MessagesRequest =
        serde_json::from_slice(body).map_err(|e| FrontendError::InvalidBody(e.to_string()))?;

    let model = FrontendModel(req.model.clone());

    // -- system --
    let system = match req.system {
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

    // -- messages --
    let messages = req
        .messages
        .into_iter()
        .map(|m| {
            let role = role_from_anthropic(&m.role)
                .ok_or_else(|| FrontendError::InvalidBody(format!("unknown role: {}", m.role)))?;
            let content = match m.content {
                InboundMessageContent::Text(text) => vec![ContentBlock::text(text)],
                InboundMessageContent::Blocks(blocks) => blocks
                    .into_iter()
                    .map(inbound_block_to_canonical)
                    .collect::<Result<Vec<_>, _>>()?,
            };
            Ok(Message {
                role,
                content,
                name: None,
                extensions: ExtensionMap::new(),
            })
        })
        .collect::<Result<Vec<_>, FrontendError>>()?;

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
    let reasoning = req.thinking.as_ref().and_then(|t| {
        // Anthropic's `type: "disabled"` / absent budget should not enable reasoning.
        let enabled = t
            .mode
            .as_deref()
            .map(|m| m.eq_ignore_ascii_case("enabled"))
            .unwrap_or(t.budget_tokens.is_some());
        if !enabled {
            return None;
        }
        Some(ReasoningOptions {
            effort: None,
            budget_tokens: t.budget_tokens,
        })
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
            // Store as unsupported with raw value; image plumbing lives in the backend
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
    fn decode_bad_role_is_rejected() {
        let body = br#"{"model":"m","max_tokens":1,"messages":[{"role":"system","content":"hi"}]}"#;
        let err = decode(body).unwrap_err();
        assert!(matches!(err, FrontendError::InvalidBody(_)));
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
}
