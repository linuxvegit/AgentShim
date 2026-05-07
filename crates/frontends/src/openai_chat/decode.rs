use agent_shim_core::{
    content::{ContentBlock, ImageBlock, UnsupportedBlock},
    extensions::ExtensionMap,
    ids::{RequestId, ToolCallId},
    media::BinarySource,
    message::{Message, MessageRole, SystemInstruction},
    request::{
        CanonicalRequest, GenerationOptions, ReasoningEffort, ReasoningOptions, RequestMetadata,
        ResponseFormat,
    },
    target::{FrontendInfo, FrontendKind, FrontendModel},
    tool::{ToolCallArguments, ToolCallBlock, ToolChoice, ToolDefinition, ToolResultBlock},
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use serde_json::Value;

use super::mapping::{role_to_canonical, RoleClass};
use super::wire::{
    ChatCompletionsRequest, InboundContentPart, InboundMessageContent, InboundToolChoice,
};
use crate::FrontendError;

/// Decode an OpenAI Chat `image_url.url` string into a canonical
/// `BinarySource`. Returns `None` for shapes we can't safely interpret —
/// the caller falls back to wrapping the part as `Unsupported` so the
/// request still flows through (the upstream provider's own validation
/// will surface the real error).
///
/// Recognised shapes:
///   * `data:<mime>;base64,<payload>` → `BinarySource::Base64`
///   * `http://...` / `https://...`   → `BinarySource::Url`
fn image_url_to_binary_source(url: &str) -> Option<BinarySource> {
    if let Some(rest) = url.strip_prefix("data:") {
        // The URL is a data URI. The OpenAI shape we care about is always
        // `data:<media_type>;base64,<payload>`; if `;base64,` is missing the
        // payload is plain percent-encoded data which we don't support yet.
        let (header, payload) = rest.split_once(',')?;
        let (media_type, encoding) = header.split_once(';').unwrap_or((header, ""));
        if encoding != "base64" {
            return None;
        }
        let bytes = STANDARD.decode(payload).ok()?;
        return Some(BinarySource::Base64 {
            media_type: media_type.to_string(),
            data: Bytes::from(bytes),
        });
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(BinarySource::Url {
            url: url.to_string(),
        });
    }
    None
}

pub fn decode(body: &[u8]) -> Result<CanonicalRequest, FrontendError> {
    let req: ChatCompletionsRequest =
        serde_json::from_slice(body).map_err(|e| FrontendError::InvalidBody(e.to_string()))?;

    let model = FrontendModel(req.model.clone());

    let mut system: Vec<SystemInstruction> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();

    for inbound in req.messages {
        let role_class = role_to_canonical(&inbound.role)
            .ok_or_else(|| FrontendError::InvalidBody(format!("unknown role: {}", inbound.role)))?;

        let text_content: Vec<ContentBlock> = match inbound.content {
            None => vec![],
            Some(InboundMessageContent::Text(t)) => vec![ContentBlock::text(t)],
            Some(InboundMessageContent::Parts(parts)) => parts
                .into_iter()
                .map(|p| match p {
                    InboundContentPart::Text { text } => ContentBlock::text(text),
                    InboundContentPart::ImageUrl { image_url } => {
                        match image_url_to_binary_source(&image_url.url) {
                            Some(source) => ContentBlock::Image(ImageBlock {
                                source,
                                extensions: ExtensionMap::new(),
                            }),
                            None => ContentBlock::Unsupported(UnsupportedBlock {
                                origin: "openai_chat".into(),
                                raw: serde_json::json!({
                                    "type": "image_url",
                                    "image_url": { "url": image_url.url }
                                }),
                            }),
                        }
                    }
                })
                .collect(),
        };

        match role_class {
            RoleClass::System(source) => {
                system.push(SystemInstruction {
                    source,
                    content: text_content,
                });
            }

            RoleClass::Message(MessageRole::Tool) => {
                // Tool result message — wrap in ToolResult content block
                let tool_call_id = inbound.tool_call_id.ok_or_else(|| {
                    FrontendError::InvalidBody("tool message missing tool_call_id".into())
                })?;
                let content_value: Value = match text_content.into_iter().next() {
                    Some(ContentBlock::Text(t)) => Value::String(t.text),
                    Some(other) => serde_json::to_value(other).unwrap_or(Value::Null),
                    None => Value::Null,
                };
                messages.push(Message {
                    role: MessageRole::Tool,
                    content: vec![ContentBlock::ToolResult(ToolResultBlock {
                        tool_call_id: ToolCallId::from_provider(tool_call_id),
                        content: content_value,
                        is_error: false,
                        extensions: ExtensionMap::new(),
                    })],
                    name: inbound.name,
                    extensions: ExtensionMap::new(),
                });
            }

            RoleClass::Message(role) => {
                // Build content from text parts plus any tool_calls on assistant turns
                let mut content = text_content;

                for tc in inbound.tool_calls {
                    let args: Value = serde_json::from_str(&tc.function.arguments)
                        .unwrap_or(Value::String(tc.function.arguments.clone()));
                    content.push(ContentBlock::ToolCall(ToolCallBlock {
                        id: ToolCallId::from_provider(tc.id),
                        name: tc.function.name,
                        arguments: ToolCallArguments::Complete { value: args },
                        extensions: ExtensionMap::new(),
                    }));
                }

                messages.push(Message {
                    role,
                    content,
                    name: inbound.name,
                    extensions: ExtensionMap::new(),
                });
            }
        }
    }

    // -- tools --
    let tools: Vec<ToolDefinition> = req
        .tools
        .unwrap_or_default()
        .into_iter()
        .map(|t| ToolDefinition {
            name: t.function.name,
            description: t.function.description,
            input_schema: t.function.parameters.unwrap_or(serde_json::json!({})),
            extensions: ExtensionMap::new(),
        })
        .collect();

    // -- tool_choice --
    let tool_choice = match req.tool_choice {
        None => ToolChoice::Auto,
        Some(InboundToolChoice::Mode(s)) => match s.as_str() {
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => ToolChoice::Auto,
        },
        Some(InboundToolChoice::Specific { function, .. }) => ToolChoice::Specific {
            name: function.name,
        },
    };

    // -- max_tokens: prefer max_completion_tokens, fall back to max_tokens --
    let max_tokens = req.max_completion_tokens.or(req.max_tokens);

    // -- response_format --
    let response_format = req.response_format.and_then(|rf| match rf.ty.as_str() {
        "json_object" => Some(ResponseFormat::JsonObject),
        "json_schema" => {
            let js = rf.json_schema?;
            Some(ResponseFormat::JsonSchema {
                name: js.name,
                schema: js.schema,
                strict: js.strict.unwrap_or(false),
            })
        }
        _ => None,
    });

    // -- generation --
    let reasoning = req
        .reasoning_effort
        .as_deref()
        .and_then(ReasoningEffort::parse)
        .map(|effort| ReasoningOptions {
            effort: Some(effort),
            budget_tokens: None,
        });
    let generation = GenerationOptions {
        max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        presence_penalty: req.presence_penalty,
        frequency_penalty: req.frequency_penalty,
        stop_sequences: req.stop.map(|s| s.into_vec()).unwrap_or_default(),
        seed: req.seed,
        reasoning,
        ..Default::default()
    };

    let mut metadata = RequestMetadata::default();
    if let Some(user) = req.user {
        metadata.user_id = Some(user);
    }

    let frontend = FrontendInfo {
        kind: FrontendKind::OpenAiChat,
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
        response_format,
        stream: req.stream.unwrap_or(false),
        metadata,
        inbound_anthropic_headers: vec![],
        resolved_policy: Default::default(),
        extensions: ExtensionMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::tool::ToolChoice;

    fn minimal(extra: &str) -> Vec<u8> {
        format!(
            r#"{{"model":"gpt-4o","messages":[{{"role":"user","content":"hi"}}]{}}}"#,
            extra
        )
        .into_bytes()
    }

    #[test]
    fn decode_minimal() {
        let req = decode(&minimal("")).unwrap();
        assert_eq!(req.model.as_str(), "gpt-4o");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, MessageRole::User);
        assert!(!req.stream);
    }

    #[test]
    fn decode_stream_flag() {
        let req = decode(&minimal(r#","stream":true"#)).unwrap();
        assert!(req.stream);
    }

    #[test]
    fn decode_system_and_developer_split() {
        let body = br#"{
            "model": "gpt-4o",
            "messages": [
                {"role":"system","content":"You are helpful."},
                {"role":"developer","content":"Use tools."},
                {"role":"user","content":"Hello"}
            ]
        }"#;
        let req = decode(body).unwrap();
        assert_eq!(req.system.len(), 2);
        assert_eq!(
            req.system[0].source,
            agent_shim_core::message::SystemSource::OpenAiSystem
        );
        assert_eq!(
            req.system[1].source,
            agent_shim_core::message::SystemSource::OpenAiDeveloper
        );
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn decode_tool_choice_none() {
        let req = decode(&minimal(r#","tool_choice":"none""#)).unwrap();
        assert_eq!(req.tool_choice, ToolChoice::None);
    }

    #[test]
    fn decode_tool_choice_required() {
        let req = decode(&minimal(r#","tool_choice":"required""#)).unwrap();
        assert_eq!(req.tool_choice, ToolChoice::Required);
    }

    #[test]
    fn decode_tool_choice_specific() {
        let req = decode(&minimal(
            r#","tool_choice":{"type":"function","function":{"name":"search"}}"#,
        ))
        .unwrap();
        assert_eq!(
            req.tool_choice,
            ToolChoice::Specific {
                name: "search".into()
            }
        );
    }

    #[test]
    fn decode_tool_result_message() {
        let body = br#"{
            "model": "gpt-4o",
            "messages": [
                {"role":"user","content":"call it"},
                {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"search","arguments":"{\"q\":\"rust\"}"}}]},
                {"role":"tool","tool_call_id":"call_1","content":"results here"}
            ]
        }"#;
        let req = decode(body).unwrap();
        // system empty, 3 messages
        assert_eq!(req.messages.len(), 3);
        match &req.messages[1].content[0] {
            ContentBlock::ToolCall(tc) => assert_eq!(tc.name, "search"),
            other => panic!("expected ToolCall, got {:?}", other),
        }
        match &req.messages[2].content[0] {
            ContentBlock::ToolResult(tr) => assert_eq!(tr.tool_call_id.0, "call_1"),
            other => panic!("expected ToolResult, got {:?}", other),
        }
    }

    #[test]
    fn decode_max_completion_tokens_takes_priority() {
        let body = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"max_tokens":100,"max_completion_tokens":200}"#;
        let req = decode(body).unwrap();
        assert_eq!(req.generation.max_tokens, Some(200));
    }

    #[test]
    fn decode_stop_string() {
        let req = decode(&minimal(r#","stop":"END""#)).unwrap();
        assert_eq!(req.generation.stop_sequences, vec!["END"]);
    }

    #[test]
    fn decode_stop_array() {
        let req = decode(&minimal(r#","stop":["END","STOP"]"#)).unwrap();
        assert_eq!(req.generation.stop_sequences, vec!["END", "STOP"]);
    }

    #[test]
    fn decode_response_format_json_object() {
        let req = decode(&minimal(r#","response_format":{"type":"json_object"}"#)).unwrap();
        assert!(matches!(
            req.response_format,
            Some(ResponseFormat::JsonObject)
        ));
    }

    // ── image_url parts (Plan 04 T3) ──────────────────────────────────

    #[test]
    fn decode_image_url_with_https_yields_canonical_url_image_block() {
        let body = br#"{
            "model":"gpt-4o",
            "messages":[{
                "role":"user",
                "content":[
                    {"type":"text","text":"describe"},
                    {"type":"image_url","image_url":{"url":"https://example.com/cat.png"}}
                ]
            }]
        }"#;
        let req = decode(body).unwrap();
        let blocks = &req.messages[0].content;
        assert_eq!(blocks.len(), 2);
        match &blocks[1] {
            ContentBlock::Image(img) => match &img.source {
                BinarySource::Url { url } => assert_eq!(url, "https://example.com/cat.png"),
                other => panic!("expected Url, got {other:?}"),
            },
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn decode_image_url_with_data_uri_yields_canonical_base64_image_block() {
        // Bytes b"\x89PNG\r\n" base64-encode to "iVBORw0K". Check the data
        // URI parser handles `data:<mime>;base64,<payload>` correctly.
        let body = br#"{
            "model":"gpt-4o",
            "messages":[{
                "role":"user",
                "content":[
                    {"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0K"}}
                ]
            }]
        }"#;
        let req = decode(body).unwrap();
        match &req.messages[0].content[0] {
            ContentBlock::Image(img) => match &img.source {
                BinarySource::Base64 { media_type, data } => {
                    assert_eq!(media_type, "image/png");
                    assert_eq!(&data[..4], b"\x89PNG");
                }
                other => panic!("expected Base64, got {other:?}"),
            },
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn decode_image_url_with_unrecognised_scheme_falls_back_to_unsupported() {
        // `file://` and bare relative paths can't be turned into either a
        // URL or a base64 blob — the decoder must NOT silently fabricate
        // a URL. Falling back to Unsupported keeps the upstream's error
        // path authoritative.
        let body = br#"{
            "model":"gpt-4o",
            "messages":[{
                "role":"user",
                "content":[
                    {"type":"image_url","image_url":{"url":"file:///tmp/cat.png"}}
                ]
            }]
        }"#;
        let req = decode(body).unwrap();
        match &req.messages[0].content[0] {
            ContentBlock::Unsupported(u) => assert_eq!(u.origin, "openai_chat"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn decode_image_url_with_data_uri_lacking_base64_marker_is_unsupported() {
        // `data:text/plain,Hello` is a valid data URI but not base64-encoded.
        // We don't have a percent-decoder for that path; fall back rather
        // than guess.
        let body = br#"{
            "model":"gpt-4o",
            "messages":[{
                "role":"user",
                "content":[
                    {"type":"image_url","image_url":{"url":"data:text/plain,Hello"}}
                ]
            }]
        }"#;
        let req = decode(body).unwrap();
        match &req.messages[0].content[0] {
            ContentBlock::Unsupported(u) => assert_eq!(u.origin, "openai_chat"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn image_url_to_binary_source_https() {
        let s = image_url_to_binary_source("https://x.test/a.png");
        assert!(matches!(s, Some(BinarySource::Url { .. })));
    }

    #[test]
    fn image_url_to_binary_source_data_uri() {
        let s = image_url_to_binary_source("data:image/png;base64,iVBORw0K").unwrap();
        match s {
            BinarySource::Base64 { media_type, data } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(&data[..4], b"\x89PNG");
            }
            _ => panic!("expected base64"),
        }
    }

    #[test]
    fn image_url_to_binary_source_rejects_garbage() {
        assert!(image_url_to_binary_source("ftp://example.com/x").is_none());
        assert!(image_url_to_binary_source("data:image/png;base64,@@@@").is_none());
        assert!(image_url_to_binary_source("just-a-string").is_none());
    }
}
