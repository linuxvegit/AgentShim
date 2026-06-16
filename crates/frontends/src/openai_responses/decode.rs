use agent_shim_core::{
    content::{ContentBlock, ImageBlock, ReasoningBlock, UnsupportedBlock},
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
use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use serde_json::Value;

use super::wire::{
    InboundTool, InboundToolChoice, InputContentPart, InputField, InputItem, InputMessage,
    InputMessageContent, ReasoningContentPart, ReasoningSummaryPart, ResponsesRequest,
};
use crate::FrontendError;

/// Decode an OpenAI Responses `input_image.image_url` string into a canonical
/// [`BinarySource`].
///
/// Mirrors the OpenAI Chat decoder's `image_url_to_binary_source`. The
/// Responses wire shape differs from Chat — `image_url` is a bare string
/// here, not the `{ "url": "..." }` object Chat uses — but the string
/// payload itself follows the same rules:
///
///   * `data:<media_type>;base64,<payload>` → [`BinarySource::Base64`]
///   * `http://...` / `https://...`         → [`BinarySource::Url`]
///   * anything else                        → `None` (caller wraps as
///     `Unsupported` so the upstream's own validation can reject it)
fn image_url_to_binary_source(url: &str) -> Option<BinarySource> {
    if let Some(rest) = url.strip_prefix("data:") {
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
                source: None,
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
    // Prelude-fold rule (2026-06-10 spec §3.3): the contiguous leading run of
    // role:"system" / "developer" entries folds into the session-level
    // `system` vec. The first non-system entry ends the prelude; any later
    // system/developer entry becomes a positional `Message{role:System}` with
    // `source` preserved (OpenAiSystem vs OpenAiDeveloper). Note: the
    // top-level `instructions` field is already pushed to `system` by the
    // caller before this helper runs; the leading-run append concatenates
    // after it.
    let mut prelude_phase = true;
    for msg in msgs {
        match msg.role.as_str() {
            "system" if prelude_phase => {
                system.push(SystemInstruction {
                    source: SystemSource::OpenAiSystem,
                    content: decode_message_content(msg.content),
                });
            }
            "system" => {
                out.push(Message {
                    role: MessageRole::System,
                    content: decode_message_content(msg.content),
                    name: None,
                    source: Some(SystemSource::OpenAiSystem),
                    extensions: ExtensionMap::new(),
                });
            }
            "developer" if prelude_phase => {
                system.push(SystemInstruction {
                    source: SystemSource::OpenAiDeveloper,
                    content: decode_message_content(msg.content),
                });
            }
            "developer" => {
                out.push(Message {
                    role: MessageRole::System,
                    content: decode_message_content(msg.content),
                    name: None,
                    source: Some(SystemSource::OpenAiDeveloper),
                    extensions: ExtensionMap::new(),
                });
            }
            "user" | "assistant" => {
                prelude_phase = false;
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
                    source: None,
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
    // Prelude-fold rule (2026-06-10 spec §3.3): mirrors `decode_messages`.
    // Any non-system input item — message, function_call, function_call_output,
    // reasoning, or the forward-compat `Other` catch-all — ends the prelude
    // window. Later system/developer message items become positional
    // `Message{role:System}` with `source` preserved.
    let mut prelude_phase = true;
    for item in items {
        match item {
            InputItem::Message { role, content } => match role.as_str() {
                "system" if prelude_phase => {
                    system.push(SystemInstruction {
                        source: SystemSource::OpenAiSystem,
                        content: decode_message_content(content),
                    });
                }
                "system" => {
                    out.push(Message {
                        role: MessageRole::System,
                        content: decode_message_content(content),
                        name: None,
                        source: Some(SystemSource::OpenAiSystem),
                        extensions: ExtensionMap::new(),
                    });
                }
                "developer" if prelude_phase => {
                    system.push(SystemInstruction {
                        source: SystemSource::OpenAiDeveloper,
                        content: decode_message_content(content),
                    });
                }
                "developer" => {
                    out.push(Message {
                        role: MessageRole::System,
                        content: decode_message_content(content),
                        name: None,
                        source: Some(SystemSource::OpenAiDeveloper),
                        extensions: ExtensionMap::new(),
                    });
                }
                "user" | "assistant" => {
                    prelude_phase = false;
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
                        source: None,
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
                prelude_phase = false;
                // `arguments` can arrive as either a JSON-encoded string
                // (spec-canonical) or as a structured value (codex 0.5+
                // sometimes emits the object directly). Normalize: if it's
                // a string, try to parse; on failure, keep it as a string.
                // If it's already structured, use it as-is.
                let args = match arguments {
                    Value::String(s) => {
                        serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s))
                    }
                    other => other,
                };
                out.push(Message {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::ToolCall(ToolCallBlock {
                        id: ToolCallId::from_provider(id.unwrap_or_else(|| call_id.clone())),
                        name,
                        arguments: ToolCallArguments::Complete { value: args },
                        extensions: ExtensionMap::new(),
                    })],
                    name: None,
                    source: None,
                    extensions: ExtensionMap::new(),
                });
            }
            InputItem::FunctionCallOutput { call_id, output } => {
                prelude_phase = false;
                out.push(Message {
                    role: MessageRole::Tool,
                    content: vec![ContentBlock::ToolResult(ToolResultBlock {
                        tool_call_id: ToolCallId::from_provider(call_id),
                        // `output` is already a `Value` -- accept both
                        // String shapes (spec-canonical) and structured
                        // shapes (codex 0.5+ may send the object directly).
                        content: output,
                        is_error: false,
                        extensions: ExtensionMap::new(),
                    })],
                    name: None,
                    source: None,
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
                prelude_phase = false;
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
                            source: None,
                            extensions: ExtensionMap::new(),
                        });
                    }
                }
            }
            // Forward-compatibility catch-all (see `InputItem::Other` doc in
            // wire.rs). The canonical model can't express these items, so
            // we drop them on the canonical chain walk; the byte-passthrough
            // path still forwards the original body verbatim. We do still
            // end the prelude window here: an unknown item might be
            // content-bearing, so the next system/developer message is
            // unlikely to be a session-level standing instruction.
            InputItem::Other => {
                prelude_phase = false;
                continue;
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
        .filter_map(|p| match p {
            ReasoningContentPart::ReasoningText { text } => Some(text),
            // Forward-compatibility catch-all (see wire.rs). Drop unknown
            // reasoning content parts on the canonical path.
            ReasoningContentPart::Other => None,
        })
        .collect();
    if !from_content.is_empty() {
        return from_content;
    }
    summary
        .into_iter()
        .flatten()
        .filter_map(|p| match p {
            ReasoningSummaryPart::SummaryText { text } => Some(text),
            // Forward-compatibility catch-all (see wire.rs). Drop unknown
            // reasoning summary parts on the canonical path.
            ReasoningSummaryPart::Other => None,
        })
        .collect()
}

fn decode_message_content(content: Option<InputMessageContent>) -> Vec<ContentBlock> {
    match content {
        None => vec![],
        Some(InputMessageContent::Text(t)) => vec![ContentBlock::text(t)],
        Some(InputMessageContent::Parts(parts)) => parts
            .into_iter()
            .filter_map(|p| match p {
                InputContentPart::InputText { text } => Some(ContentBlock::text(text)),
                // Assistant-side `output_text` parts (echoed back into input
                // from a prior turn) carry plain text content -- treat them
                // as a canonical text block. The canonical model collapses
                // user/assistant content into the same `ContentBlock::Text`
                // shape; the message's `role` carries the speaker
                // distinction, not the part's `type`.
                InputContentPart::OutputText { text } => Some(ContentBlock::text(text)),
                InputContentPart::InputImage { image_url } => {
                    // Plan 04 T4: input_image parts must surface as canonical
                    // `ContentBlock::Image` so providers' outbound encoders
                    // can render them in their native wire shapes (Anthropic
                    // `image`, Gemini `fileData`/`inlineData`, OAI-compat
                    // `image_url`). Wrapping as `Unsupported` would silently
                    // drop the image at the provider boundary.
                    let block = match image_url_to_binary_source(&image_url) {
                        Some(source) => ContentBlock::Image(ImageBlock {
                            source,
                            extensions: ExtensionMap::new(),
                        }),
                        None => ContentBlock::Unsupported(UnsupportedBlock {
                            origin: "openai_responses".into(),
                            raw: serde_json::json!({
                                "type": "input_image",
                                "image_url": image_url
                            }),
                        }),
                    };
                    Some(block)
                }
                // Forward-compatibility catch-all (see wire.rs). Drop unknown
                // content part types on the canonical path; passthrough still
                // forwards the original bytes verbatim.
                InputContentPart::Other => None,
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
            // Forward-compatibility: OpenAI/codex periodically introduces new
            // built-in tool types (e.g. `namespace`, `mcp`, `local_shell`).
            // Treat any unknown type as an opaque built-in: preserve every
            // top-level field as raw JSON so the byte-passthrough path can
            // forward it verbatim to an OpenAI-Responses-native upstream.
            // The canonical chain walk (non-passthrough) loses these tools
            // because they have no canonical representation -- that mirrors
            // the v0.8 behavior for `web_search` et al. routing to Anthropic.
            other => {
                let mut raw = serde_json::json!({"type": other});
                if let Some(name) = &tool.name {
                    raw["name"] = serde_json::Value::String(name.clone());
                }
                if let Some(desc) = &tool.description {
                    raw["description"] = serde_json::Value::String(desc.clone());
                }
                if let Some(params) = &tool.parameters {
                    raw["parameters"] = params.clone();
                }
                if let Some(function) = &tool.function {
                    raw["function"] = serde_json::json!({
                        "name": function.name,
                        "description": function.description,
                        "parameters": function.parameters,
                    });
                }
                builtin.push(raw);
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

    /// Forward-compatibility: codex 0.5+ and future OpenAI Responses clients
    /// emit tool types we don't enumerate (`namespace`, `mcp`, `local_shell`,
    /// etc.). These must survive decode -- the byte-passthrough path then
    /// forwards the original request body verbatim to the upstream, which
    /// is the authority on whether the tool type is valid.
    #[test]
    fn decode_unknown_tool_type_becomes_builtin_passthrough() {
        let body = br#"{
            "model": "gpt-5.5",
            "input": "Hi",
            "tools": [
                {"type":"namespace","name":"agent","description":"agent ns"},
                {"type":"mcp","name":"my_mcp","parameters":{"server":"x"}},
                {"type":"local_shell"}
            ]
        }"#;
        let req = decode(body).expect("unknown tool types should not hard-fail");
        assert!(req.tools.is_empty());
        let builtin = req
            .extensions
            .get("builtin_tools")
            .expect("builtin_tools key present")
            .as_array()
            .expect("builtin_tools is an array");
        assert_eq!(builtin.len(), 3);
        assert_eq!(builtin[0]["type"], "namespace");
        assert_eq!(builtin[0]["name"], "agent");
        assert_eq!(builtin[0]["description"], "agent ns");
        assert_eq!(builtin[1]["type"], "mcp");
        assert_eq!(builtin[1]["parameters"]["server"], "x");
        assert_eq!(builtin[2]["type"], "local_shell");
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

    /// Codex 0.5+ sometimes emits `function_call.arguments` as a JSON
    /// object directly rather than a JSON-encoded string. The spec is
    /// String, but accepting both is a permissive forward-compat move
    /// (the canonical model carries it as a parsed `Value` anyway).
    #[test]
    fn decode_function_call_arguments_as_structured_value() {
        let body = br#"{
            "model": "gpt-5.5",
            "input": [
                {"type":"function_call","call_id":"c1","name":"x","arguments":{"a":1,"b":[2,3]}}
            ]
        }"#;
        let req = decode(body).expect("structured `arguments` should be accepted");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, MessageRole::Assistant);
    }

    /// Spec-canonical string-form arguments still work (legacy shape).
    #[test]
    fn decode_function_call_arguments_as_string_still_works() {
        let body = br#"{
            "model": "gpt-5.5",
            "input": [
                {"type":"function_call","call_id":"c1","name":"x","arguments":"{\"a\":1}"}
            ]
        }"#;
        let req = decode(body).expect("string `arguments` should still work");
        assert_eq!(req.messages.len(), 1);
    }

    /// Same forward-compat story for `function_call_output.output`: codex
    /// 0.5+ may send structured output rather than a String.
    #[test]
    fn decode_function_call_output_as_structured_value() {
        let body = br#"{
            "model": "gpt-5.5",
            "input": [
                {"type":"function_call_output","call_id":"c1","output":{"result":"ok","count":7}}
            ]
        }"#;
        let req = decode(body).expect("structured `output` should be accepted");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, MessageRole::Tool);
    }

    /// `reasoning.summary` may carry vendor-specific part types like
    /// `encrypted_content`. Drop them silently on the canonical path
    /// rather than hard-failing the whole request decode.
    #[test]
    fn decode_reasoning_summary_with_unknown_part_type_is_skipped() {
        let body = br#"{
            "model": "gpt-5.5",
            "input": [
                {"type":"reasoning","summary":[
                    {"type":"summary_text","text":"first"},
                    {"type":"encrypted_content","content":"opaque"}
                ]}
            ]
        }"#;
        let req = decode(body).expect("unknown summary part types should not hard-fail");
        assert_eq!(req.messages.len(), 1);
    }

    /// `reasoning.content` mirrors `summary` for unknown part types.
    #[test]
    fn decode_reasoning_content_with_unknown_part_type_is_skipped() {
        let body = br#"{
            "model": "gpt-5.5",
            "input": [
                {"type":"reasoning","content":[
                    {"type":"reasoning_text","text":"hello"},
                    {"type":"something_new","payload":"x"}
                ]}
            ]
        }"#;
        let req = decode(body).expect("unknown content part types should not hard-fail");
        assert_eq!(req.messages.len(), 1);
    }

    /// Forward-compatibility: codex 0.5+ and future OpenAI Responses clients
    /// emit input item types we don't enumerate (`mcp_call`,
    /// `mcp_list_tools`, `local_shell_call`, `image_generation_call`,
    /// `namespace`, etc.). The wire enum's `#[serde(other)]` catch-all must
    /// keep the whole request from failing untagged-enum decoding -- the
    /// byte-passthrough path then forwards the original body verbatim to
    /// the upstream, which is the authority on whether the item is valid.
    /// The canonical chain walk drops these items (they have no canonical
    /// representation), the same way the unknown-tool-type fix handled it.
    #[test]
    fn decode_unknown_input_item_type_is_skipped_not_rejected() {
        let body = br#"{
            "model": "gpt-5.5",
            "input": [
                {"type":"message","role":"user","content":"hi"},
                {"type":"mcp_call","id":"mcp_1","name":"search","arguments":"{}"},
                {"type":"local_shell_call","id":"sh_1","action":{"command":"ls"}},
                {"type":"namespace","id":"ns_1","name":"agent"},
                {"type":"image_generation_call","id":"img_1","result":"..."}
            ]
        }"#;
        let req = decode(body).expect("unknown input item types should not hard-fail");
        // Only the message survives the canonical decode; the four unknown
        // items are silently dropped on the canonical path.
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, MessageRole::User);
    }

    /// Codex 0.5+ echoes prior assistant turns back into `input` with their
    /// original `output_text` content parts intact. Before this fix, those
    /// parts didn't match any `InputContentPart` variant, failing the
    /// `Messages` and `Items` decode passes and cascading up `InputField`
    /// as the now-infamous "untagged enum InputField" 400.
    #[test]
    fn decode_assistant_message_with_output_text_part_is_accepted() {
        let body = br#"{
            "model": "gpt-5.5",
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"q"}]},
                {"type":"message","role":"assistant","phase":"commentary","content":[
                    {"type":"output_text","text":"reasoning aloud"}
                ]},
                {"type":"message","role":"assistant","phase":"final_answer","content":[
                    {"type":"output_text","text":"final"}
                ]}
            ]
        }"#;
        let req = decode(body).expect("output_text parts should be accepted");
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.messages[0].role, MessageRole::User);
        assert_eq!(req.messages[1].role, MessageRole::Assistant);
        assert_eq!(req.messages[2].role, MessageRole::Assistant);
        // Assistant texts round-trip as ContentBlock::Text.
        match &req.messages[1].content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "reasoning aloud"),
            other => panic!("expected text block, got {other:?}"),
        }
        match &req.messages[2].content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "final"),
            other => panic!("expected text block, got {other:?}"),
        }
    }

    /// Forward-compat for unknown content part types (future Responses
    /// additions). Mirrors the InputItem::Other story one level down.
    #[test]
    fn decode_unknown_content_part_type_is_skipped() {
        let body = br#"{
            "model": "gpt-5.5",
            "input": [
                {"type":"message","role":"user","content":[
                    {"type":"input_text","text":"keep"},
                    {"type":"some_future_part","payload":"drop me"}
                ]}
            ]
        }"#;
        let req = decode(body).expect("unknown content part types should not hard-fail");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].content.len(), 1);
        match &req.messages[0].content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "keep"),
            other => panic!("expected text block, got {other:?}"),
        }
    }

    /// End-to-end regression: a verbatim codex 0.5+ wire capture that
    /// triggered the recurring `untagged enum InputField` 400. Combines
    /// every shape variation that landed in v0.9 patch series:
    /// `output_text` parts on assistant messages with `phase` metadata,
    /// `reasoning` items with `encrypted_content`, `web_search_call`
    /// (unknown InputItem type), `namespace` / `custom` / `tool_search`
    /// tools (unknown tool types). All must round-trip without error.
    #[test]
    fn decode_codex_0_5_full_capture_round_trips() {
        // Synthetic capture covering the same surface as the 100 KB
        // real-world body that produced openai-responses-26b54d46.json.
        // Inlined so the test stays self-contained.
        let body = br#"{
            "model": "gpt-5.5",
            "instructions": "you are a helpful assistant",
            "input": [
                {"type":"message","role":"developer","content":[{"type":"input_text","text":"sys"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"q1"}]},
                {"type":"reasoning","summary":[],"content":null,"encrypted_content":"opaque"},
                {"type":"message","role":"assistant","phase":"commentary","content":[
                    {"type":"output_text","text":"thinking aloud"}
                ]},
                {"type":"web_search_call","status":"completed","action":{"type":"search","query":"x"}},
                {"type":"reasoning","summary":[],"content":null,"encrypted_content":"opaque2"},
                {"type":"message","role":"assistant","phase":"final_answer","content":[
                    {"type":"output_text","text":"answer"}
                ]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"q2"}]}
            ],
            "tools": [
                {"type":"function","name":"exec_command","description":"d","parameters":{}},
                {"type":"namespace","name":"agent"},
                {"type":"custom","name":"x","description":"y"},
                {"type":"tool_search"},
                {"type":"web_search"}
            ],
            "parallel_tool_calls": true,
            "store": false,
            "prompt_cache_key": "k",
            "client_metadata": {"x":1}
        }"#;
        let req = decode(body).expect("codex 0.5+ full capture must decode without error");
        // Six message-bearing items (4 messages + 2 reasoning-on-assistant + 1
        // web_search_call dropped): we don't pin the exact message count so
        // this stays robust to attachment-order tweaks, but we do require
        // user/assistant roles to be present.
        let roles: Vec<_> = req.messages.iter().map(|m| m.role).collect();
        assert!(roles.contains(&MessageRole::User));
        assert!(roles.contains(&MessageRole::Assistant));
        // Tools: `function` and `custom` survive as canonical tools; the
        // other three (namespace/tool_search/web_search) become builtin
        // passthrough JSON on `extensions.builtin_tools`.
        assert_eq!(req.tools.len(), 2);
        let names: Vec<_> = req.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"exec_command"));
        assert!(names.contains(&"x"));
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

    // ── input_image parts (Plan 04 T4) ─────────────────────────────────

    #[test]
    fn decode_input_image_with_https_yields_canonical_url_image_block() {
        // Plan 04 T4: the Responses decoder previously mapped every
        // input_image to ContentBlock::Unsupported, which silently dropped
        // the image at the provider boundary. The fix mirrors the OpenAI
        // Chat decoder — https URLs surface as `BinarySource::Url`.
        let body = br#"{
            "model":"gpt-4o",
            "input":[{
                "role":"user",
                "content":[
                    {"type":"input_text","text":"describe"},
                    {"type":"input_image","image_url":"https://example.com/cat.png"}
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
    fn decode_input_image_with_data_uri_yields_canonical_base64_image_block() {
        // Bytes b"\x89PNG\r\n" base64-encode to "iVBORw0K". Verify the data
        // URI parser handles `data:<mime>;base64,<payload>` correctly.
        let body = br#"{
            "model":"gpt-4o",
            "input":[{
                "role":"user",
                "content":[
                    {"type":"input_image","image_url":"data:image/png;base64,iVBORw0K"}
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
    fn decode_input_image_with_unrecognised_scheme_falls_back_to_unsupported() {
        // Schemes other than http(s) and base64 data URIs cannot be safely
        // turned into a canonical `BinarySource`; fall back to Unsupported
        // so the upstream's validation surfaces the real error rather than
        // the gateway silently fabricating a URL.
        let body = br#"{
            "model":"gpt-4o",
            "input":[{
                "role":"user",
                "content":[
                    {"type":"input_image","image_url":"file:///tmp/cat.png"}
                ]
            }]
        }"#;
        let req = decode(body).unwrap();
        match &req.messages[0].content[0] {
            ContentBlock::Unsupported(u) => assert_eq!(u.origin, "openai_responses"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    // ── effort vocabulary (2026-05-28 spec) ────────────────────────────

    #[test]
    fn responses_reasoning_effort_max_decodes() {
        use agent_shim_core::request::ReasoningEffort;
        let body = br#"{
            "model":"gpt-5.5",
            "input":"hi",
            "reasoning":{"effort":"max"}
        }"#;
        let req = decode(body).expect("decodes");
        let r = req.generation.reasoning.expect("reasoning set");
        assert_eq!(r.effort, Some(ReasoningEffort::Max));
    }

    // ── prelude_phase fold (2026-06-10 spec §3.3) ──────────────────────

    #[test]
    fn responses_decode_input_first_system_folds_when_no_instructions() {
        let body = br#"{
            "model":"gpt-5",
            "input":[
                {"type":"message","role":"system","content":[{"type":"input_text","text":"A"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
            ]
        }"#;
        let req = decode(body).unwrap();
        assert_eq!(req.system.len(), 1);
        assert!(req.messages.iter().all(|m| m.role != MessageRole::System));
    }

    #[test]
    fn responses_decode_instructions_and_leading_input_system_concatenate() {
        let body = br#"{
            "model":"gpt-5",
            "instructions":"Standing X",
            "input":[
                {"type":"message","role":"system","content":[{"type":"input_text","text":"Leading Y"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
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
    }

    #[test]
    fn responses_decode_input_mid_system_preserved() {
        let body = br#"{
            "model":"gpt-5",
            "instructions":"Standing X",
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"a"}]},
                {"type":"message","role":"system","content":[{"type":"input_text","text":"mid"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"b"}]}
            ]
        }"#;
        let req = decode(body).unwrap();
        assert_eq!(req.system.len(), 1);
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.messages[1].role, MessageRole::System);
        assert_eq!(req.messages[1].source, Some(SystemSource::OpenAiSystem));
    }

    #[test]
    fn responses_decode_input_developer_round_trips_with_source() {
        let body = br#"{
            "model":"gpt-5",
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"a"}]},
                {"type":"message","role":"developer","content":[{"type":"input_text","text":"dev"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"b"}]}
            ]
        }"#;
        let req = decode(body).unwrap();
        assert_eq!(req.messages[1].role, MessageRole::System);
        assert_eq!(req.messages[1].source, Some(SystemSource::OpenAiDeveloper));
    }

    /// Regression: openai-responses-5879c5f3.json. codex/Hermes (gpt-5.5)
    /// mixes message items that OMIT the optional `type` field with typed
    /// items (reasoning / function_call / function_call_output) in the SAME
    /// `input` array. The OpenAI Responses spec makes `type` optional on
    /// input message items (it defaults to "message"), so a real-world array
    /// looks like `[{role,content}, {type:reasoning,...}, {role,content},
    /// {type:function_call,...}, {type:function_call_output,...}]`.
    ///
    /// Both untagged `InputField` variants failed on this shape: `Messages`
    /// because the typed items have no `role`, and `Items` because
    /// `InputItem`'s internally-tagged derive rejected the type-less
    /// messages (`#[serde(other)]` only catches *unknown* tag values, never
    /// a *missing* tag). The cascade surfaced as the recurring HTTP 400
    /// "data did not match any variant of untagged enum InputField".
    #[test]
    fn decode_input_array_mixes_typeless_messages_with_typed_items() {
        let body = br#"{
            "model": "gpt-5.5",
            "input": [
                {"role":"user","content":"hi"},
                {"type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"thinking"}]},
                {"role":"assistant","content":""},
                {"type":"function_call","call_id":"c1","name":"search","arguments":"{}"},
                {"type":"function_call_output","call_id":"c1","output":"results"}
            ]
        }"#;
        let req =
            decode(body).expect("type-less messages mixed with typed items must decode, not 400");
        assert_eq!(req.messages[0].role, MessageRole::User);
        match &req.messages[0].content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, "hi"),
            other => panic!("expected text block, got {other:?}"),
        }
        let roles: Vec<_> = req.messages.iter().map(|m| m.role).collect();
        assert!(roles.contains(&MessageRole::User));
        assert!(roles.contains(&MessageRole::Assistant));
        assert!(roles.contains(&MessageRole::Tool));
    }

    /// A type-less item that also lacks `role` is malformed; drop it via the
    /// `Other` catch-all rather than 400 the whole request. This keeps the
    /// type-less-message fix from regressing into a new hard-fail surface.
    #[test]
    fn decode_typeless_item_without_role_is_dropped_not_rejected() {
        let body = br#"{
            "model": "gpt-5.5",
            "input": [
                {"role":"user","content":"hi"},
                {"foo":"bar"}
            ]
        }"#;
        let req = decode(body).expect("type-less role-less item must be dropped, not 400");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, MessageRole::User);
    }
}
