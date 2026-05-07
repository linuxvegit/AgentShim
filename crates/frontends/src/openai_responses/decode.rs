use agent_shim_core::{
    content::{ContentBlock, ReasoningBlock, UnsupportedBlock},
    extensions::ExtensionMap,
    ids::{RequestId, ToolCallId},
    message::{Message, MessageRole, SystemInstruction, SystemSource},
    request::{
        CanonicalRequest, GenerationOptions, ReasoningEffort, ReasoningOptions, RequestMetadata,
    },
    target::{FrontendInfo, FrontendKind, FrontendModel},
    tool::{ToolCallArguments, ToolCallBlock, ToolChoice, ToolDefinition, ToolResultBlock},
};
use serde_json::Value;

use super::wire::{
    InboundTool, InboundToolChoice, InputContentPart, InputField, InputItem, InputMessage,
    InputMessageContent, ReasoningContentPart, ReasoningSummaryPart, ResponsesRequest,
};
use crate::FrontendError;

pub fn decode(body: &[u8]) -> Result<CanonicalRequest, FrontendError> {
    let req: ResponsesRequest =
        serde_json::from_slice(body).map_err(|e| FrontendError::InvalidBody(e.to_string()))?;

    let model = FrontendModel(req.model.clone());

    let mut system: Vec<SystemInstruction> = Vec::new();
    if let Some(instructions) = req.instructions {
        system.push(SystemInstruction {
            source: SystemSource::OpenAiSystem,
            content: vec![ContentBlock::text(instructions)],
        });
    }

    let (input_system, messages) = decode_input(req.input)?;
    system.extend(input_system);

    let (tools, builtin_tools) = decode_tools(req.tools.unwrap_or_default())?;

    let tool_choice = match req.tool_choice {
        None => ToolChoice::Auto,
        Some(InboundToolChoice::Mode(s)) => match s.as_str() {
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => ToolChoice::Auto,
        },
        Some(InboundToolChoice::Specific { name, .. }) => ToolChoice::Specific { name },
    };

    let generation = GenerationOptions {
        max_tokens: req.max_output_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        reasoning: req.reasoning.as_ref().and_then(|r| {
            let effort = r.effort.as_deref().and_then(ReasoningEffort::parse);
            if effort.is_none() {
                None
            } else {
                Some(ReasoningOptions {
                    effort,
                    budget_tokens: None,
                })
            }
        }),
        ..Default::default()
    };

    let mut metadata = RequestMetadata::default();
    if let Some(meta) = req.metadata {
        if let Some(user_id) = meta.get("user_id").and_then(|v| v.as_str()) {
            metadata.user_id = Some(user_id.to_string());
        }
    }

    let frontend = FrontendInfo {
        kind: FrontendKind::OpenAiResponses,
        requested_model: model.clone(),
    };

    let mut extensions = ExtensionMap::new();
    if !builtin_tools.is_empty() {
        extensions.insert("builtin_tools", serde_json::Value::Array(builtin_tools));
    }

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
        metadata,
        inbound_anthropic_headers: vec![],
        resolved_policy: Default::default(),
        extensions,
    })
}

fn decode_input(
    input: InputField,
) -> Result<(Vec<SystemInstruction>, Vec<Message>), FrontendError> {
    match input {
        InputField::Text(text) => Ok((
            vec![],
            vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::text(text)],
                name: None,
                extensions: ExtensionMap::new(),
            }],
        )),
        InputField::Messages(msgs) => decode_messages(msgs),
        InputField::Items(items) => decode_items(items),
    }
}

fn decode_messages(
    msgs: Vec<InputMessage>,
) -> Result<(Vec<SystemInstruction>, Vec<Message>), FrontendError> {
    let mut system = Vec::new();
    let mut out = Vec::new();
    for msg in msgs {
        match msg.role.as_str() {
            "system" => {
                system.push(SystemInstruction {
                    source: SystemSource::OpenAiSystem,
                    content: decode_message_content(msg.content),
                });
            }
            "developer" => {
                system.push(SystemInstruction {
                    source: SystemSource::OpenAiDeveloper,
                    content: decode_message_content(msg.content),
                });
            }
            "user" | "assistant" => {
                let role = if msg.role == "user" {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                };
                let content = decode_message_content(msg.content);
                out.push(Message {
                    role,
                    content,
                    name: None,
                    extensions: ExtensionMap::new(),
                });
            }
            other => {
                return Err(FrontendError::InvalidBody(format!(
                    "unknown role in input message: {other}"
                )));
            }
        }
    }
    Ok((system, out))
}

fn decode_items(
    items: Vec<InputItem>,
) -> Result<(Vec<SystemInstruction>, Vec<Message>), FrontendError> {
    let mut system = Vec::new();
    let mut out = Vec::new();
    for item in items {
        match item {
            InputItem::Message { role, content } => match role.as_str() {
                "system" => {
                    system.push(SystemInstruction {
                        source: SystemSource::OpenAiSystem,
                        content: decode_message_content(content),
                    });
                }
                "developer" => {
                    system.push(SystemInstruction {
                        source: SystemSource::OpenAiDeveloper,
                        content: decode_message_content(content),
                    });
                }
                "user" | "assistant" => {
                    let msg_role = if role == "user" {
                        MessageRole::User
                    } else {
                        MessageRole::Assistant
                    };
                    let blocks = decode_message_content(content);
                    out.push(Message {
                        role: msg_role,
                        content: blocks,
                        name: None,
                        extensions: ExtensionMap::new(),
                    });
                }
                other => {
                    return Err(FrontendError::InvalidBody(format!(
                        "unknown role in input item: {other}"
                    )));
                }
            },
            InputItem::FunctionCall {
                id,
                call_id,
                name,
                arguments,
            } => {
                let args: Value =
                    serde_json::from_str(&arguments).unwrap_or(Value::String(arguments));
                out.push(Message {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::ToolCall(ToolCallBlock {
                        id: ToolCallId::from_provider(id.unwrap_or_else(|| call_id.clone())),
                        name,
                        arguments: ToolCallArguments::Complete { value: args },
                        extensions: ExtensionMap::new(),
                    })],
                    name: None,
                    extensions: ExtensionMap::new(),
                });
            }
            InputItem::FunctionCallOutput { call_id, output } => {
                out.push(Message {
                    role: MessageRole::Tool,
                    content: vec![ContentBlock::ToolResult(ToolResultBlock {
                        tool_call_id: ToolCallId::from_provider(call_id),
                        content: Value::String(output),
                        is_error: false,
                        extensions: ExtensionMap::new(),
                    })],
                    name: None,
                    extensions: ExtensionMap::new(),
                });
            }
            // Reasoning items become `ContentBlock::Reasoning` attached to
            // the preceding assistant message. If the previous message in
            // `out` is not an assistant message (or `out` is empty), a new
            // assistant message is created to hold the reasoning block. The
            // `id` and `status` fields are accepted but ignored — the
            // canonical model does not carry per-block IDs from input; we
            // synthesize them on encode.
            InputItem::Reasoning {
                summary, content, ..
            } => {
                let text = extract_reasoning_text(content, summary);
                let block = ContentBlock::Reasoning(ReasoningBlock {
                    text,
                    extensions: ExtensionMap::new(),
                });
                match out.last_mut() {
                    Some(msg) if msg.role == MessageRole::Assistant => {
                        msg.content.push(block);
                    }
                    _ => {
                        out.push(Message {
                            role: MessageRole::Assistant,
                            content: vec![block],
                            name: None,
                            extensions: ExtensionMap::new(),
                        });
                    }
                }
            }
        }
    }
    Ok((system, out))
}

/// Joins reasoning text from the typed `reasoning` input item. Prefers the
/// richer `content` array; falls back to `summary` when `content` is empty
/// or missing. Returns an empty string when both are empty — the caller
/// still emits the block so round-trip semantics are preserved.
fn extract_reasoning_text(
    content: Option<Vec<ReasoningContentPart>>,
    summary: Option<Vec<ReasoningSummaryPart>>,
) -> String {
    let from_content: String = content
        .into_iter()
        .flatten()
        .map(|p| match p {
            ReasoningContentPart::ReasoningText { text } => text,
        })
        .collect();
    if !from_content.is_empty() {
        return from_content;
    }
    summary
        .into_iter()
        .flatten()
        .map(|p| match p {
            ReasoningSummaryPart::SummaryText { text } => text,
        })
        .collect()
}

fn decode_message_content(content: Option<InputMessageContent>) -> Vec<ContentBlock> {
    match content {
        None => vec![],
        Some(InputMessageContent::Text(t)) => vec![ContentBlock::text(t)],
        Some(InputMessageContent::Parts(parts)) => parts
            .into_iter()
            .map(|p| match p {
                InputContentPart::InputText { text } => ContentBlock::text(text),
                InputContentPart::InputImage { image_url } => {
                    ContentBlock::Unsupported(UnsupportedBlock {
                        origin: "openai_responses".into(),
                        raw: serde_json::json!({
                            "type": "input_image",
                            "image_url": image_url
                        }),
                    })
                }
            })
            .collect(),
    }
}

fn decode_tools(
    tools: Vec<InboundTool>,
) -> Result<(Vec<ToolDefinition>, Vec<serde_json::Value>), FrontendError> {
    let mut out = Vec::new();
    let mut builtin = Vec::new();
    for tool in tools {
        match tool.ty.as_str() {
            "function" | "custom" => {
                let (name, description, parameters) = if let Some(f) = tool.function {
                    (f.name, f.description, f.parameters)
                } else if let Some(name) = tool.name {
                    (name, tool.description, tool.parameters)
                } else {
                    return Err(FrontendError::InvalidBody(
                        "function tool missing name".into(),
                    ));
                };
                out.push(ToolDefinition {
                    name,
                    description,
                    input_schema: parameters.unwrap_or(serde_json::json!({})),
                    extensions: ExtensionMap::new(),
                });
            }
            "web_search" | "web_search_preview" | "file_search" | "code_interpreter"
            | "computer_use" => {
                // Preserve raw JSON for passthrough to providers that support them
                let mut raw = serde_json::json!({"type": tool.ty});
                if let Some(name) = &tool.name {
                    raw["name"] = serde_json::Value::String(name.clone());
                }
                if let Some(desc) = &tool.description {
                    raw["description"] = serde_json::Value::String(desc.clone());
                }
                if let Some(params) = &tool.parameters {
                    raw["parameters"] = params.clone();
                }
                builtin.push(raw);
            }
            other => {
                return Err(FrontendError::InvalidBody(format!(
                    "unknown tool type: {other}"
                )));
            }
        }
    }
    Ok((out, builtin))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_text_input() {
        let body = br#"{"model":"gpt-4o","input":"Hello"}"#;
        let req = decode(body).unwrap();
        assert_eq!(req.model.as_str(), "gpt-4o");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, MessageRole::User);
        assert!(!req.stream);
    }

    #[test]
    fn decode_message_array_input() {
        let body = br#"{
            "model": "gpt-4o",
            "input": [{"role":"user","content":"Hello"}],
            "stream": true
        }"#;
        let req = decode(body).unwrap();
        assert_eq!(req.messages.len(), 1);
        assert!(req.stream);
    }

    #[test]
    fn decode_item_array_with_tool_result() {
        let body = br#"{
            "model": "gpt-4o",
            "input": [
                {"type":"message","role":"user","content":"call it"},
                {"type":"function_call","call_id":"call_1","name":"search","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"results"}
            ]
        }"#;
        let req = decode(body).unwrap();
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.messages[2].role, MessageRole::Tool);
    }

    #[test]
    fn decode_instructions_become_system() {
        let body = br#"{"model":"gpt-4o","input":"Hi","instructions":"Be helpful"}"#;
        let req = decode(body).unwrap();
        assert_eq!(req.system.len(), 1);
    }

    #[test]
    fn decode_preserves_builtin_tools() {
        let body = br#"{
            "model": "gpt-4o",
            "input": "Hi",
            "tools": [{"type":"web_search"}]
        }"#;
        let req = decode(body).unwrap();
        assert!(req.tools.is_empty());
        let builtin = req.extensions.get("builtin_tools").unwrap();
        assert_eq!(builtin.as_array().unwrap().len(), 1);
    }

    #[test]
    fn decode_function_tool() {
        let body = br#"{
            "model": "gpt-4o",
            "input": "Hi",
            "tools": [{"type":"function","name":"search","description":"Search","parameters":{"type":"object"}}]
        }"#;
        let req = decode(body).unwrap();
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "search");
    }

    #[test]
    fn decode_max_output_tokens() {
        let body = br#"{"model":"gpt-4o","input":"Hi","max_output_tokens":1024}"#;
        let req = decode(body).unwrap();
        assert_eq!(req.generation.max_tokens, Some(1024));
    }

    #[test]
    fn decode_tool_choice_specific_function_name() {
        // Plan 01 T6 — gap fill: the typed `{type:"function",name:"..."}` shape
        // of `InboundToolChoice` was previously untested. The string-mode shape
        // (auto/none/required) is exercised implicitly by request fixtures
        // omitting `tool_choice`, but the typed-name path requires explicit
        // coverage so it does not silently regress.

        // Arrange: a request that pins a specific tool by name.
        let body = br#"{
            "model": "gpt-4o",
            "input": "Hi",
            "tools": [{"type":"function","name":"search","parameters":{"type":"object"}}],
            "tool_choice": {"type":"function","name":"search"}
        }"#;

        // Act
        let req = decode(body).unwrap();

        // Assert: tool_choice resolves to ToolChoice::Specific with the chosen name.
        match req.tool_choice {
            ToolChoice::Specific { name } => assert_eq!(name, "search"),
            other => panic!("expected ToolChoice::Specific, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_items_attach_to_preceding_assistant_message() {
        // Case 1: reasoning AFTER an assistant message attaches to it.
        let body = br#"{
            "model": "gpt-4o",
            "input": [
                {"type":"message","role":"user","content":"hello"},
                {"type":"message","role":"assistant","content":"hi there"},
                {"type":"reasoning","id":"rs_1","summary":[],"content":[
                    {"type":"reasoning_text","text":"thought A"},
                    {"type":"reasoning_text","text":" + B"}
                ]}
            ]
        }"#;
        let req = decode(body).unwrap();
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[1].role, MessageRole::Assistant);
        assert_eq!(req.messages[1].content.len(), 2);
        match &req.messages[1].content[1] {
            ContentBlock::Reasoning(r) => assert_eq!(r.text, "thought A + B"),
            other => panic!("expected Reasoning block, got {other:?}"),
        }

        // Case 2: reasoning AS FIRST item creates a new assistant message,
        // and a subsequent assistant message item is NOT merged into it.
        let body = br#"{
            "model": "gpt-4o",
            "input": [
                {"type":"reasoning","content":[{"type":"reasoning_text","text":"pre-think"}]},
                {"type":"message","role":"assistant","content":"the answer"}
            ]
        }"#;
        let req = decode(body).unwrap();
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, MessageRole::Assistant);
        assert_eq!(req.messages[0].content.len(), 1);
        match &req.messages[0].content[0] {
            ContentBlock::Reasoning(r) => assert_eq!(r.text, "pre-think"),
            other => panic!("expected Reasoning block, got {other:?}"),
        }
        assert_eq!(req.messages[1].role, MessageRole::Assistant);
        match &req.messages[1].content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "the answer"),
            other => panic!("expected Text block, got {other:?}"),
        }

        // Case 3: content array present → text comes from content (joined),
        // even when a summary is also provided.
        let body = br#"{
            "model": "gpt-4o",
            "input": [
                {"type":"message","role":"assistant","content":"hi"},
                {"type":"reasoning",
                 "summary":[{"type":"summary_text","text":"summary-only"}],
                 "content":[
                    {"type":"reasoning_text","text":"part1 "},
                    {"type":"reasoning_text","text":"part2"}
                 ]}
            ]
        }"#;
        let req = decode(body).unwrap();
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].content.len(), 2);
        match &req.messages[0].content[1] {
            ContentBlock::Reasoning(r) => assert_eq!(r.text, "part1 part2"),
            other => panic!("expected Reasoning block, got {other:?}"),
        }

        // Case 4: empty content but summary present → fall back to summary.
        let body = br#"{
            "model": "gpt-4o",
            "input": [
                {"type":"message","role":"assistant","content":"hi"},
                {"type":"reasoning",
                 "summary":[
                    {"type":"summary_text","text":"sum-A"},
                    {"type":"summary_text","text":"/sum-B"}
                 ],
                 "content":[]}
            ]
        }"#;
        let req = decode(body).unwrap();
        assert_eq!(req.messages.len(), 1);
        match &req.messages[0].content[1] {
            ContentBlock::Reasoning(r) => assert_eq!(r.text, "sum-A/sum-B"),
            other => panic!("expected Reasoning block, got {other:?}"),
        }
    }
}
