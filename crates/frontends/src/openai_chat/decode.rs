use agent_shim_core::{
    content::{ContentBlock, UnsupportedBlock},
    extensions::ExtensionMap,
    ids::{RequestId, ToolCallId},
    message::{Message, MessageRole, SystemInstruction},
    request::{CanonicalRequest, GenerationOptions, RequestMetadata, ResponseFormat},
    target::{FrontendInfo, FrontendKind, FrontendModel},
    tool::{ToolCallArguments, ToolCallBlock, ToolChoice, ToolDefinition, ToolResultBlock},
};
use serde_json::Value;

use crate::FrontendError;
use super::mapping::{role_to_canonical, RoleClass};
use super::wire::{
    ChatCompletionsRequest, InboundContentPart, InboundMessageContent, InboundToolChoice,
};

pub fn decode(body: &[u8]) -> Result<CanonicalRequest, FrontendError> {
    let req: ChatCompletionsRequest =
        serde_json::from_slice(body).map_err(|e| FrontendError::InvalidBody(e.to_string()))?;

    let model = FrontendModel(req.model.clone());

    let mut system: Vec<SystemInstruction> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();

    for inbound in req.messages {
        let role_class = role_to_canonical(&inbound.role).ok_or_else(|| {
            FrontendError::InvalidBody(format!("unknown role: {}", inbound.role))
        })?;

        let text_content: Vec<ContentBlock> = match inbound.content {
            None => vec![],
            Some(InboundMessageContent::Text(t)) => vec![ContentBlock::text(t)],
            Some(InboundMessageContent::Parts(parts)) => parts
                .into_iter()
                .map(|p| match p {
                    InboundContentPart::Text { text } => ContentBlock::text(text),
                    InboundContentPart::ImageUrl { image_url } => {
                        ContentBlock::Unsupported(UnsupportedBlock {
                            origin: "openai_chat".into(),
                            raw: serde_json::json!({
                                "type": "image_url",
                                "image_url": { "url": image_url.url }
                            }),
                        })
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
        Some(InboundToolChoice::Specific { function, .. }) => {
            ToolChoice::Specific { name: function.name }
        }
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
    let generation = GenerationOptions {
        max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        presence_penalty: req.presence_penalty,
        frequency_penalty: req.frequency_penalty,
        stop_sequences: req.stop.map(|s| s.into_vec()).unwrap_or_default(),
        seed: req.seed,
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
        assert_eq!(req.tool_choice, ToolChoice::Specific { name: "search".into() });
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
        assert!(matches!(req.response_format, Some(ResponseFormat::JsonObject)));
    }
}
