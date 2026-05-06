//! Encoder: [`agent_shim_core::CanonicalRequest`] → Gemini
//! [`super::wire::GenerateContentRequest`].
//!
//! Plan 03 T4. The Gemini wire format is structurally distinct from OAI-Chat
//! — there is no `messages` array of role-tagged objects; instead the API
//! expects a `contents` array where each entry is a `Content { role, parts }`,
//! with `parts` carrying typed payloads (`text`, `inlineData`, `fileData`,
//! `functionCall`, `functionResponse`). System instructions live on a separate
//! top-level `systemInstruction` field rather than inside `contents`.
//!
//! ## Role mapping
//!
//! | Canonical role  | Gemini role |
//! |-----------------|-------------|
//! | `User`          | `"user"`    |
//! | `Assistant`     | `"model"`   |
//! | `Tool`          | `"function"`|
//!
//! ## Per-block translation
//!
//! - `ContentBlock::Text` → `Part { text }`
//! - `ContentBlock::Reasoning` → `Part { text, thought: true }` (round-trips
//!   Anthropic-style extended thinking when the inbound was Anthropic; a fresh
//!   `Reasoning` block on the way out wouldn't normally exist on a *request*,
//!   but if a frontend forwards prior `model` reasoning we keep it labelled).
//! - `ContentBlock::Image` (and `Audio` / `File`):
//!     - `BinarySource::Base64 { media_type, data }` → `Part { inlineData }`
//!     - `BinarySource::Url { url }` → `Part { fileData }` (Gemini accepts
//!       `fileData` for URL-form references; `inlineData` is base64-only).
//!     - `BinarySource::Bytes { .. }` → encoded as base64 inline (matches the
//!       `Base64` path; `Bytes` is a runtime-only variant, never on the wire).
//!     - `BinarySource::ProviderFileId { file_id }` → `Part { fileData }`
//!       with `fileUri = file_id` (operator's responsibility to ensure the id
//!       resolves on Gemini).
//! - `ContentBlock::ToolCall` → `Part { functionCall { name, args } }`. The
//!   canonical `ToolCallArguments` enum has two variants:
//!     - `Complete { value }` → `args: Some(value)` (real JSON object).
//!     - `Streaming { data }` → parsed via `serde_json::from_str`; if parse
//!       fails (mid-stream partial) we emit `args: None` rather than
//!       hand-corrupting JSON.
//! - `ContentBlock::ToolResult` → `Part { functionResponse { name, response } }`.
//!   Gemini requires a `name` here (not just an id, unlike OpenAI), so we look
//!   up the originating tool call's name via a HashMap built from prior
//!   `ToolCall` blocks. If no match (e.g. orphan tool result), we fall back
//!   to the `tool_call_id` string value as the name — the request will likely
//!   fail upstream, but it's preferable to dropping the block silently.
//! - `ContentBlock::RedactedReasoning`, `ContentBlock::Unsupported` are
//!   skipped — they have no representation in the Gemini protocol.
//!
//! ## Generation config
//!
//! Per-request knobs map straightforwardly. `stop_sequences` is forwarded as
//! `stopSequences`; `response_format` becomes `responseMimeType` +
//! `responseSchema`. The interesting case is reasoning — see
//! [`thinking_config`].
//!
//! ## Thinking-budget precedence
//!
//! Three sources can specify a thinking budget; precedence is:
//!
//! 1. `req.generation.reasoning.budget_tokens` (Anthropic-style explicit
//!    token count; if the inbound is Anthropic Messages with
//!    `thinking.budget_tokens`, it lands here verbatim).
//! 2. `resolved_policy.reasoning_effort` (route-default; populated when an
//!    inbound effort wasn't supplied — see `RoutePolicy::resolve`).
//! 3. `req.generation.reasoning.effort` (request-level qualitative effort).
//!
//! Sources (2) and (3) map effort levels to concrete budgets:
//!
//! | Effort   | Budget tokens |
//! |----------|---------------|
//! | Minimal  | 128           |
//! | Low      | 256           |
//! | Medium   | 1024          |
//! | High     | 4096          |
//! | Xhigh    | 16384         |
//!
//! Whenever a budget is set, `include_thoughts` is also set to `true` so the
//! parser (T6) can route reasoning into `ContentBlock::Reasoning`.
//!
//! When no source supplies a budget, `thinking_config` is `None` and the
//! upstream uses its model-specific default.

use std::collections::HashMap;

use agent_shim_core::{
    request::ResponseFormat, BackendTarget, BinarySource, CanonicalRequest, ContentBlock,
    MessageRole, ReasoningEffort, ToolCallArguments,
};
use base64::Engine as _;

use super::wire::{
    Content, FileData, FunctionCall, FunctionDeclaration, FunctionResponse, GenerateContentRequest,
    GenerationConfig, InlineData, Part, ThinkingConfig, Tool,
};

/// Build a Gemini `:generateContent` / `:streamGenerateContent` request body
/// from a canonical request.
pub(crate) fn build(req: &CanonicalRequest, _target: &BackendTarget) -> GenerateContentRequest {
    // Build a name lookup for tool calls so tool results can carry the
    // originating function name (Gemini requires it on `functionResponse`).
    let tool_call_name_by_id = collect_tool_call_names(req);

    // Conversation contents (system instructions go on the dedicated field).
    let contents = build_contents(req, &tool_call_name_by_id);

    // System instruction: aggregate all canonical system blocks into a single
    // `Content { role: "user", parts: [text...] }`. Gemini accepts a single
    // systemInstruction object, so multiple sources collapse into one.
    let system_instruction = build_system_instruction(req);

    // Tools become a single Tool wrapper carrying every FunctionDeclaration.
    let tools = build_tools(req);

    let generation_config = build_generation_config(req);

    GenerateContentRequest {
        contents,
        system_instruction,
        tools,
        generation_config,
        // No safety_settings emitted by AgentShim — we don't currently surface
        // a knob for them, and Gemini's defaults are sensible. T7 may revisit.
        safety_settings: None,
    }
}

// ---------------------------------------------------------------------------
// Tool-call name lookup
// ---------------------------------------------------------------------------

/// Walk every assistant-emitted `ToolCall` in the request and remember its
/// `(id → name)` mapping, so a later `ToolResult` block can carry the
/// originating function name on the wire (Gemini's `functionResponse.name`).
fn collect_tool_call_names(req: &CanonicalRequest) -> HashMap<String, String> {
    let mut by_id: HashMap<String, String> = HashMap::new();
    for msg in &req.messages {
        for block in &msg.content {
            if let ContentBlock::ToolCall(tc) = block {
                by_id.insert(tc.id.0.clone(), tc.name.clone());
            }
        }
    }
    by_id
}

// ---------------------------------------------------------------------------
// Contents
// ---------------------------------------------------------------------------

fn build_contents(
    req: &CanonicalRequest,
    tool_call_name_by_id: &HashMap<String, String>,
) -> Vec<Content> {
    let mut out: Vec<Content> = Vec::with_capacity(req.messages.len());

    for msg in &req.messages {
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "model",
            MessageRole::Tool => "function",
        };

        let parts: Vec<Part> = msg
            .content
            .iter()
            .filter_map(|b| block_to_part(b, tool_call_name_by_id))
            .collect();

        // Skip empty messages. Gemini rejects `Content` with no parts, and
        // dropping a fully-skipped message is preferable to a 400.
        if parts.is_empty() {
            continue;
        }

        out.push(Content {
            role: role.to_string(),
            parts,
        });
    }

    out
}

fn block_to_part(
    block: &ContentBlock,
    tool_call_name_by_id: &HashMap<String, String>,
) -> Option<Part> {
    match block {
        ContentBlock::Text(t) => Some(Part {
            text: Some(t.text.clone()),
            ..Default::default()
        }),
        ContentBlock::Reasoning(r) => Some(Part {
            text: Some(r.text.clone()),
            thought: Some(true),
            ..Default::default()
        }),
        ContentBlock::Image(img) => binary_to_part(&img.source),
        ContentBlock::Audio(a) => binary_to_part(&a.source),
        ContentBlock::File(f) => binary_to_part(&f.source),
        ContentBlock::ToolCall(tc) => {
            let args = match &tc.arguments {
                ToolCallArguments::Complete { value } => Some(value.clone()),
                ToolCallArguments::Streaming { data } => {
                    // A streaming-args canonical block is rare on a *request*
                    // (those originate on responses), but if it does appear
                    // we try to parse it as JSON. If parsing fails (truncated
                    // mid-stream), drop the args field entirely rather than
                    // shipping a string where the upstream expects an object.
                    serde_json::from_str::<serde_json::Value>(data).ok()
                }
            };
            Some(Part {
                function_call: Some(FunctionCall {
                    name: tc.name.clone(),
                    args,
                    // We don't currently propagate Gemini-side ids back into
                    // the canonical model, so don't fabricate one here.
                    id: None,
                }),
                ..Default::default()
            })
        }
        ContentBlock::ToolResult(tr) => {
            // Gemini requires a function name on `functionResponse`. Look up
            // the originating tool call by id; if missing, fall back to the
            // id itself as the name (likely upstream 400, but better than
            // dropping the block).
            let name = tool_call_name_by_id
                .get(&tr.tool_call_id.0)
                .cloned()
                .unwrap_or_else(|| tr.tool_call_id.0.clone());

            // Wrap the canonical content into a JSON object so Gemini sees an
            // object value. Strings/arrays/numbers are wrapped as
            // `{ "content": <value> }`; existing objects pass through.
            let response = wrap_tool_result_content(&tr.content);

            Some(Part {
                function_response: Some(FunctionResponse {
                    name,
                    response,
                    id: None,
                }),
                ..Default::default()
            })
        }
        // No representation on the Gemini wire — drop silently.
        ContentBlock::RedactedReasoning(_) | ContentBlock::Unsupported(_) => None,
    }
}

/// Translate a canonical `BinarySource` into a Gemini Part.
///
/// `Base64` and runtime-only `Bytes` both produce `inlineData` (base64-encoded);
/// `Url` and `ProviderFileId` produce `fileData`.
fn binary_to_part(source: &BinarySource) -> Option<Part> {
    match source {
        BinarySource::Base64 { media_type, data } => Some(Part {
            inline_data: Some(InlineData {
                mime_type: media_type.clone(),
                data: base64::engine::general_purpose::STANDARD.encode(data.as_ref()),
            }),
            ..Default::default()
        }),
        BinarySource::Bytes { media_type, data } => Some(Part {
            inline_data: Some(InlineData {
                mime_type: media_type.clone(),
                data: base64::engine::general_purpose::STANDARD.encode(data.as_ref()),
            }),
            ..Default::default()
        }),
        BinarySource::Url { url } => Some(Part {
            file_data: Some(FileData {
                // Gemini infers the type from URL extensions in many cases;
                // since canonical `Url` doesn't carry a media type, we leave
                // it empty rather than inventing one.
                mime_type: String::new(),
                file_uri: url.clone(),
            }),
            ..Default::default()
        }),
        BinarySource::ProviderFileId { file_id } => Some(Part {
            file_data: Some(FileData {
                mime_type: String::new(),
                file_uri: file_id.clone(),
            }),
            ..Default::default()
        }),
    }
}

/// Wrap a canonical tool-result content value into the JSON object Gemini
/// expects on `functionResponse.response`. Plain strings/arrays/numbers
/// become `{ "content": value }`; existing objects pass through.
fn wrap_tool_result_content(content: &serde_json::Value) -> serde_json::Value {
    if content.is_object() {
        content.clone()
    } else {
        serde_json::json!({ "content": content })
    }
}

// ---------------------------------------------------------------------------
// System instruction
// ---------------------------------------------------------------------------

fn build_system_instruction(req: &CanonicalRequest) -> Option<Content> {
    if req.system.is_empty() {
        return None;
    }
    let parts: Vec<Part> = req
        .system
        .iter()
        .flat_map(|si| si.content.iter())
        .filter_map(|b| {
            if let ContentBlock::Text(t) = b {
                Some(Part {
                    text: Some(t.text.clone()),
                    ..Default::default()
                })
            } else {
                None
            }
        })
        .collect();

    if parts.is_empty() {
        return None;
    }

    Some(Content {
        // Gemini's `systemInstruction` field expects a `Content` shape; the
        // role on it is conventionally "user" (the API ignores it but
        // requires the field to deserialize as `Content`).
        role: "user".to_string(),
        parts,
    })
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn build_tools(req: &CanonicalRequest) -> Option<Vec<Tool>> {
    if req.tools.is_empty() {
        return None;
    }
    let function_declarations: Vec<FunctionDeclaration> = req
        .tools
        .iter()
        .map(|t| FunctionDeclaration {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: Some(t.input_schema.clone()),
        })
        .collect();
    // Gemini accepts multiple `Tool` wrappers, but a single one carrying every
    // declaration is the common shape and matches AI Studio examples.
    Some(vec![Tool {
        function_declarations,
    }])
}

// ---------------------------------------------------------------------------
// GenerationConfig
// ---------------------------------------------------------------------------

fn build_generation_config(req: &CanonicalRequest) -> Option<GenerationConfig> {
    let g = &req.generation;

    let (response_mime_type, response_schema) = match req.response_format.as_ref() {
        None | Some(ResponseFormat::Text) => (None, None),
        Some(ResponseFormat::JsonObject) => (Some("application/json".to_string()), None),
        Some(ResponseFormat::JsonSchema { schema, .. }) => {
            (Some("application/json".to_string()), Some(schema.clone()))
        }
    };

    let stop_sequences = if g.stop_sequences.is_empty() {
        None
    } else {
        Some(g.stop_sequences.clone())
    };

    let thinking_config = thinking_config(req);

    let any_field_set = g.max_tokens.is_some()
        || g.temperature.is_some()
        || g.top_p.is_some()
        || g.top_k.is_some()
        || stop_sequences.is_some()
        || response_mime_type.is_some()
        || response_schema.is_some()
        || thinking_config.is_some();

    if !any_field_set {
        return None;
    }

    Some(GenerationConfig {
        temperature: g.temperature.map(|t| t as f64),
        top_p: g.top_p.map(|t| t as f64),
        top_k: g.top_k.map(|t| t as i64),
        max_output_tokens: g.max_tokens.map(|t| t as i64),
        stop_sequences,
        response_mime_type,
        response_schema,
        thinking_config,
    })
}

/// Compute the [`ThinkingConfig`] for the outbound request.
///
/// Precedence (returns the first match, never reads later sources):
///
/// 1. `req.generation.reasoning.budget_tokens` — explicit Anthropic-style
///    token count (frontend-provided).
/// 2. `req.resolved_policy.reasoning_effort` — route default merged with
///    inbound effort by `RoutePolicy::resolve`.
/// 3. `req.generation.reasoning.effort` — request-level effort, used only when
///    `resolved_policy` is empty (defensive: the canonical pipeline normally
///    populates `resolved_policy` from this same field).
///
/// `include_thoughts: Some(true)` is set whenever a budget is emitted, so the
/// response parser (T6) can route `thought: true` parts into
/// `ContentBlock::Reasoning`.
fn thinking_config(req: &CanonicalRequest) -> Option<ThinkingConfig> {
    // Source 1 — explicit budget on the request.
    if let Some(reasoning) = req.generation.reasoning.as_ref() {
        if let Some(budget) = reasoning.budget_tokens {
            return Some(ThinkingConfig {
                thinking_budget: Some(budget as i64),
                include_thoughts: Some(true),
            });
        }
    }

    // Source 2 — resolved policy (route default merged with inbound effort).
    if let Some(effort) = req.resolved_policy.reasoning_effort {
        return Some(ThinkingConfig {
            thinking_budget: Some(effort_to_budget(effort)),
            include_thoughts: Some(true),
        });
    }

    // Source 3 — request-level effort. In the live pipeline `resolve()`
    // copies this into `resolved_policy.reasoning_effort`, so this branch
    // is mostly defensive (lets standalone tests skip the resolve step).
    if let Some(reasoning) = req.generation.reasoning.as_ref() {
        if let Some(effort) = reasoning.effort {
            return Some(ThinkingConfig {
                thinking_budget: Some(effort_to_budget(effort)),
                include_thoughts: Some(true),
            });
        }
    }

    None
}

fn effort_to_budget(effort: ReasoningEffort) -> i64 {
    match effort {
        ReasoningEffort::Minimal => 128,
        ReasoningEffort::Low => 256,
        ReasoningEffort::Medium => 1024,
        ReasoningEffort::High => 4096,
        ReasoningEffort::Xhigh => 16384,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        content::{ImageBlock, ReasoningBlock, RedactedReasoningBlock, UnsupportedBlock},
        request::{ReasoningOptions, RequestMetadata},
        ExtensionMap, FrontendInfo, FrontendKind, FrontendModel, GenerationOptions, Message,
        RequestId, ResolvedPolicy, ToolCallBlock, ToolCallId, ToolDefinition, ToolResultBlock,
    };
    use bytes::Bytes;
    use serde_json::json;

    fn target() -> BackendTarget {
        BackendTarget {
            provider: "gemini".into(),
            model: "gemini-2.0-flash".into(),
            policy: Default::default(),
        }
    }

    fn empty_request() -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: FrontendModel::from("claude-test"),
            },
            model: FrontendModel::from("claude-test"),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: Default::default(),
            generation: GenerationOptions::default(),
            response_format: None,
            stream: false,
            metadata: RequestMetadata::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: ResolvedPolicy::default(),
            extensions: ExtensionMap::new(),
        }
    }

    // ---- Roles & basic shape ---------------------------------------------

    #[test]
    fn empty_request_produces_empty_contents_and_no_optional_fields() {
        let body = build(&empty_request(), &target());
        assert!(body.contents.is_empty());
        assert!(body.system_instruction.is_none());
        assert!(body.tools.is_none());
        assert!(body.generation_config.is_none());
        assert!(body.safety_settings.is_none());
    }

    #[test]
    fn user_text_message_round_trips_to_user_role_with_text_part() {
        let mut req = empty_request();
        req.messages
            .push(Message::user(vec![ContentBlock::text("hello")]));

        let body = build(&req, &target());
        assert_eq!(body.contents.len(), 1);
        assert_eq!(body.contents[0].role, "user");
        assert_eq!(body.contents[0].parts.len(), 1);
        assert_eq!(body.contents[0].parts[0].text.as_deref(), Some("hello"));
    }

    #[test]
    fn assistant_role_maps_to_model() {
        let mut req = empty_request();
        req.messages
            .push(Message::assistant(vec![ContentBlock::text("hi back")]));

        let body = build(&req, &target());
        assert_eq!(body.contents[0].role, "model");
        assert_eq!(body.contents[0].parts[0].text.as_deref(), Some("hi back"));
    }

    #[test]
    fn tool_role_maps_to_function() {
        let mut req = empty_request();
        // Tool result with no prior tool call — name falls back to the id.
        req.messages.push(Message {
            role: MessageRole::Tool,
            content: vec![ContentBlock::ToolResult(ToolResultBlock {
                tool_call_id: ToolCallId::from_provider("call_abc"),
                content: json!({"ok": true}),
                is_error: false,
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });

        let body = build(&req, &target());
        assert_eq!(body.contents[0].role, "function");
        let resp = body.contents[0].parts[0]
            .function_response
            .as_ref()
            .expect("functionResponse part");
        assert_eq!(resp.name, "call_abc"); // fallback to id
        assert_eq!(resp.response, json!({"ok": true}));
    }

    #[test]
    fn empty_message_is_skipped_to_avoid_upstream_400() {
        let mut req = empty_request();
        // RedactedReasoning has no Gemini representation — message becomes
        // empty and should be dropped.
        req.messages
            .push(Message::user(vec![ContentBlock::RedactedReasoning(
                RedactedReasoningBlock {
                    data: "redacted".into(),
                    extensions: ExtensionMap::new(),
                },
            )]));

        let body = build(&req, &target());
        assert!(body.contents.is_empty());
    }

    // ---- Reasoning -------------------------------------------------------

    #[test]
    fn reasoning_block_emits_part_with_thought_true() {
        let mut req = empty_request();
        req.messages.push(Message::assistant(vec![
            ContentBlock::Reasoning(ReasoningBlock {
                text: "thinking out loud".into(),
                extensions: ExtensionMap::new(),
            }),
            ContentBlock::text("the answer is 42"),
        ]));

        let body = build(&req, &target());
        assert_eq!(body.contents[0].parts.len(), 2);
        assert_eq!(
            body.contents[0].parts[0].text.as_deref(),
            Some("thinking out loud")
        );
        assert_eq!(body.contents[0].parts[0].thought, Some(true));
        assert_eq!(
            body.contents[0].parts[1].text.as_deref(),
            Some("the answer is 42")
        );
        assert!(body.contents[0].parts[1].thought.is_none());
    }

    // ---- Vision ----------------------------------------------------------

    #[test]
    fn base64_image_emits_inline_data_part() {
        let mut req = empty_request();
        req.messages
            .push(Message::user(vec![ContentBlock::Image(ImageBlock {
                source: BinarySource::Base64 {
                    media_type: "image/png".into(),
                    data: Bytes::from_static(b"\x89PNG\r\n"),
                },
                extensions: ExtensionMap::new(),
            })]));

        let body = build(&req, &target());
        let part = &body.contents[0].parts[0];
        let inline = part.inline_data.as_ref().expect("inline_data present");
        assert_eq!(inline.mime_type, "image/png");
        // base64 of "\x89PNG\r\n"
        assert_eq!(inline.data, "iVBORw0K");
    }

    #[test]
    fn url_image_emits_file_data_part() {
        let mut req = empty_request();
        req.messages
            .push(Message::user(vec![ContentBlock::Image(ImageBlock {
                source: BinarySource::Url {
                    url: "https://example.com/cat.png".into(),
                },
                extensions: ExtensionMap::new(),
            })]));

        let body = build(&req, &target());
        let part = &body.contents[0].parts[0];
        let fd = part.file_data.as_ref().expect("file_data present");
        assert_eq!(fd.file_uri, "https://example.com/cat.png");
    }

    #[test]
    fn provider_file_id_image_emits_file_data_part() {
        let mut req = empty_request();
        req.messages
            .push(Message::user(vec![ContentBlock::Image(ImageBlock {
                source: BinarySource::ProviderFileId {
                    file_id: "files/abc123".into(),
                },
                extensions: ExtensionMap::new(),
            })]));

        let body = build(&req, &target());
        let part = &body.contents[0].parts[0];
        let fd = part.file_data.as_ref().expect("file_data present");
        assert_eq!(fd.file_uri, "files/abc123");
    }

    // ---- Tool calls ------------------------------------------------------

    #[test]
    fn tool_call_with_complete_args_emits_function_call_part() {
        let mut req = empty_request();
        req.messages
            .push(Message::assistant(vec![ContentBlock::ToolCall(
                ToolCallBlock {
                    id: ToolCallId::from_provider("call_1"),
                    name: "get_weather".into(),
                    arguments: ToolCallArguments::Complete {
                        value: json!({"city": "Paris"}),
                    },
                    extensions: ExtensionMap::new(),
                },
            )]));

        let body = build(&req, &target());
        let fc = body.contents[0].parts[0]
            .function_call
            .as_ref()
            .expect("functionCall part");
        assert_eq!(fc.name, "get_weather");
        // Args MUST be a real JSON object (NOT stringified).
        assert_eq!(fc.args, Some(json!({"city": "Paris"})));
    }

    #[test]
    fn tool_call_with_streaming_args_parses_when_valid_json() {
        let mut req = empty_request();
        req.messages
            .push(Message::assistant(vec![ContentBlock::ToolCall(
                ToolCallBlock {
                    id: ToolCallId::from_provider("call_1"),
                    name: "get_weather".into(),
                    arguments: ToolCallArguments::Streaming {
                        data: r#"{"city":"Paris"}"#.into(),
                    },
                    extensions: ExtensionMap::new(),
                },
            )]));

        let body = build(&req, &target());
        let fc = body.contents[0].parts[0].function_call.as_ref().unwrap();
        assert_eq!(fc.args, Some(json!({"city": "Paris"})));
    }

    #[test]
    fn tool_call_with_invalid_streaming_args_drops_args_field() {
        let mut req = empty_request();
        req.messages
            .push(Message::assistant(vec![ContentBlock::ToolCall(
                ToolCallBlock {
                    id: ToolCallId::from_provider("call_1"),
                    name: "get_weather".into(),
                    arguments: ToolCallArguments::Streaming {
                        // Truncated mid-stream.
                        data: r#"{"city":"Par"#.into(),
                    },
                    extensions: ExtensionMap::new(),
                },
            )]));

        let body = build(&req, &target());
        let fc = body.contents[0].parts[0].function_call.as_ref().unwrap();
        // We never ship a string where the upstream expects an object.
        assert!(fc.args.is_none());
    }

    // ---- Tool results ----------------------------------------------------

    #[test]
    fn tool_result_resolves_function_name_from_prior_call() {
        let mut req = empty_request();
        req.messages
            .push(Message::assistant(vec![ContentBlock::ToolCall(
                ToolCallBlock {
                    id: ToolCallId::from_provider("call_1"),
                    name: "get_weather".into(),
                    arguments: ToolCallArguments::Complete {
                        value: json!({"city": "Paris"}),
                    },
                    extensions: ExtensionMap::new(),
                },
            )]));
        req.messages.push(Message {
            role: MessageRole::Tool,
            content: vec![ContentBlock::ToolResult(ToolResultBlock {
                tool_call_id: ToolCallId::from_provider("call_1"),
                content: json!({"temp": 20}),
                is_error: false,
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });

        let body = build(&req, &target());
        // contents[0] is the assistant tool call; contents[1] is the function reply.
        let resp = body.contents[1].parts[0]
            .function_response
            .as_ref()
            .expect("functionResponse part");
        assert_eq!(resp.name, "get_weather");
        assert_eq!(resp.response, json!({"temp": 20}));
    }

    #[test]
    fn tool_result_with_string_content_wraps_in_content_object() {
        let mut req = empty_request();
        req.messages.push(Message {
            role: MessageRole::Tool,
            content: vec![ContentBlock::ToolResult(ToolResultBlock {
                tool_call_id: ToolCallId::from_provider("call_1"),
                content: json!("23 degrees"),
                is_error: false,
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });

        let body = build(&req, &target());
        let resp = body.contents[0].parts[0]
            .function_response
            .as_ref()
            .unwrap();
        // String wraps as { "content": "..." } so it's an object on the wire.
        assert_eq!(resp.response, json!({"content": "23 degrees"}));
    }

    // ---- System instruction ---------------------------------------------

    #[test]
    fn system_blocks_aggregate_into_system_instruction() {
        use agent_shim_core::message::{SystemInstruction, SystemSource};
        let mut req = empty_request();
        req.system.push(SystemInstruction {
            source: SystemSource::AnthropicSystem,
            content: vec![ContentBlock::text("You are helpful.")],
        });
        req.system.push(SystemInstruction {
            source: SystemSource::AnthropicSystem,
            content: vec![ContentBlock::text("Be concise.")],
        });

        let body = build(&req, &target());
        let si = body.system_instruction.expect("systemInstruction set");
        assert_eq!(si.role, "user");
        assert_eq!(si.parts.len(), 2);
        assert_eq!(si.parts[0].text.as_deref(), Some("You are helpful."));
        assert_eq!(si.parts[1].text.as_deref(), Some("Be concise."));
    }

    #[test]
    fn system_with_no_text_blocks_yields_no_system_instruction() {
        use agent_shim_core::message::{SystemInstruction, SystemSource};
        let mut req = empty_request();
        // A non-text system block (would never occur in practice, but the
        // encoder must not panic and must not emit empty Content).
        req.system.push(SystemInstruction {
            source: SystemSource::AnthropicSystem,
            content: vec![ContentBlock::Unsupported(UnsupportedBlock {
                origin: "test".into(),
                raw: json!({"type": "weird"}),
            })],
        });
        let body = build(&req, &target());
        assert!(body.system_instruction.is_none());
    }

    // ---- Tools -----------------------------------------------------------

    #[test]
    fn tools_emit_single_tool_with_function_declarations() {
        let mut req = empty_request();
        req.tools.push(ToolDefinition {
            name: "get_weather".into(),
            description: Some("Get weather for a city".into()),
            input_schema: json!({
                "type": "object",
                "properties": {"city": {"type": "string"}}
            }),
            extensions: ExtensionMap::new(),
        });
        req.tools.push(ToolDefinition {
            name: "send_email".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            extensions: ExtensionMap::new(),
        });

        let body = build(&req, &target());
        let tools = body.tools.expect("tools set");
        assert_eq!(tools.len(), 1);
        let decls = &tools[0].function_declarations;
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0].name, "get_weather");
        assert_eq!(
            decls[0].description.as_deref(),
            Some("Get weather for a city")
        );
        assert_eq!(
            decls[0].parameters,
            Some(json!({
                "type": "object",
                "properties": {"city": {"type": "string"}}
            }))
        );
        assert_eq!(decls[1].name, "send_email");
        assert!(decls[1].description.is_none());
    }

    // ---- GenerationConfig -----------------------------------------------

    #[test]
    fn generation_config_omitted_when_no_knobs_set() {
        let body = build(&empty_request(), &target());
        assert!(body.generation_config.is_none());
    }

    #[test]
    fn generation_config_carries_basic_knobs() {
        let mut req = empty_request();
        req.generation = GenerationOptions {
            max_tokens: Some(512),
            temperature: Some(0.5),
            top_p: Some(0.75),
            top_k: Some(40),
            stop_sequences: vec!["END".into(), "STOP".into()],
            ..Default::default()
        };
        let body = build(&req, &target());
        let gc = body.generation_config.expect("generation_config set");
        assert_eq!(gc.max_output_tokens, Some(512));
        assert_eq!(gc.temperature, Some(0.5));
        assert_eq!(gc.top_p, Some(0.75));
        assert_eq!(gc.top_k, Some(40));
        assert_eq!(gc.stop_sequences, Some(vec!["END".into(), "STOP".into()]));
    }

    #[test]
    fn response_format_json_object_sets_mime_type_only() {
        let mut req = empty_request();
        req.response_format = Some(ResponseFormat::JsonObject);
        let body = build(&req, &target());
        let gc = body.generation_config.unwrap();
        assert_eq!(gc.response_mime_type.as_deref(), Some("application/json"));
        assert!(gc.response_schema.is_none());
    }

    #[test]
    fn response_format_json_schema_sets_mime_and_schema() {
        let mut req = empty_request();
        req.response_format = Some(ResponseFormat::JsonSchema {
            name: "answer".into(),
            schema: json!({"type": "object"}),
            strict: true,
        });
        let body = build(&req, &target());
        let gc = body.generation_config.unwrap();
        assert_eq!(gc.response_mime_type.as_deref(), Some("application/json"));
        assert_eq!(gc.response_schema, Some(json!({"type": "object"})));
    }

    // ---- ThinkingConfig precedence --------------------------------------

    #[test]
    fn explicit_budget_tokens_wins_over_resolved_policy() {
        let mut req = empty_request();
        req.generation.reasoning = Some(ReasoningOptions {
            effort: Some(ReasoningEffort::Low), // would map to 256
            budget_tokens: Some(7777),          // wins
        });
        req.resolved_policy.reasoning_effort = Some(ReasoningEffort::High); // would map to 4096

        let body = build(&req, &target());
        let tc = body
            .generation_config
            .unwrap()
            .thinking_config
            .expect("thinking_config set");
        assert_eq!(tc.thinking_budget, Some(7777));
        assert_eq!(tc.include_thoughts, Some(true));
    }

    #[test]
    fn resolved_policy_effort_wins_over_request_effort_when_no_explicit_budget() {
        let mut req = empty_request();
        req.generation.reasoning = Some(ReasoningOptions {
            effort: Some(ReasoningEffort::Minimal), // would map to 128
            budget_tokens: None,
        });
        req.resolved_policy.reasoning_effort = Some(ReasoningEffort::Xhigh); // wins → 16384

        let body = build(&req, &target());
        let tc = body
            .generation_config
            .unwrap()
            .thinking_config
            .expect("thinking_config set");
        assert_eq!(tc.thinking_budget, Some(16384));
    }

    #[test]
    fn request_effort_used_when_no_resolved_policy_and_no_explicit_budget() {
        let mut req = empty_request();
        req.generation.reasoning = Some(ReasoningOptions {
            effort: Some(ReasoningEffort::Medium),
            budget_tokens: None,
        });
        // resolved_policy.reasoning_effort stays None.

        let body = build(&req, &target());
        let tc = body
            .generation_config
            .unwrap()
            .thinking_config
            .expect("thinking_config set");
        assert_eq!(tc.thinking_budget, Some(1024));
        assert_eq!(tc.include_thoughts, Some(true));
    }

    #[test]
    fn no_reasoning_anywhere_yields_no_thinking_config() {
        let mut req = empty_request();
        // Set an unrelated generation knob so generation_config itself is emitted.
        req.generation.temperature = Some(0.5);
        let body = build(&req, &target());
        let gc = body.generation_config.unwrap();
        assert!(gc.thinking_config.is_none());
    }

    #[test]
    fn effort_to_budget_table_matches_spec() {
        assert_eq!(effort_to_budget(ReasoningEffort::Minimal), 128);
        assert_eq!(effort_to_budget(ReasoningEffort::Low), 256);
        assert_eq!(effort_to_budget(ReasoningEffort::Medium), 1024);
        assert_eq!(effort_to_budget(ReasoningEffort::High), 4096);
        assert_eq!(effort_to_budget(ReasoningEffort::Xhigh), 16384);
    }

    // ---- Defensive drops -------------------------------------------------

    #[test]
    fn unsupported_and_redacted_blocks_are_dropped() {
        let mut req = empty_request();
        req.messages.push(Message::user(vec![
            ContentBlock::text("real content"),
            ContentBlock::Unsupported(UnsupportedBlock {
                origin: "exotic".into(),
                raw: json!({"weird": true}),
            }),
            ContentBlock::RedactedReasoning(RedactedReasoningBlock {
                data: "redacted".into(),
                extensions: ExtensionMap::new(),
            }),
        ]));
        let body = build(&req, &target());
        assert_eq!(body.contents[0].parts.len(), 1);
        assert_eq!(
            body.contents[0].parts[0].text.as_deref(),
            Some("real content")
        );
    }

    // ---- Serialization sanity check -------------------------------------

    #[test]
    fn full_request_serializes_to_camel_case_wire_shape() {
        // End-to-end: build -> serde_json::to_value, check representative
        // wire-format keys are camelCase and structurally correct.
        let mut req = empty_request();
        req.system
            .push(agent_shim_core::message::SystemInstruction {
                source: agent_shim_core::message::SystemSource::AnthropicSystem,
                content: vec![ContentBlock::text("Be helpful.")],
            });
        req.messages
            .push(Message::user(vec![ContentBlock::text("hi")]));
        req.tools.push(ToolDefinition {
            name: "ping".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            extensions: ExtensionMap::new(),
        });
        req.generation.max_tokens = Some(64);
        req.generation.temperature = Some(0.5);
        req.generation.reasoning = Some(ReasoningOptions {
            effort: Some(ReasoningEffort::High),
            budget_tokens: None,
        });

        let body = build(&req, &target());
        let value = serde_json::to_value(&body).unwrap();

        // System and messages are populated.
        assert_eq!(
            value["systemInstruction"]["parts"][0]["text"],
            json!("Be helpful.")
        );
        assert_eq!(value["contents"][0]["role"], json!("user"));
        assert_eq!(value["contents"][0]["parts"][0]["text"], json!("hi"));

        // Tools wrapper carries one functionDeclaration.
        assert_eq!(
            value["tools"][0]["functionDeclarations"][0]["name"],
            json!("ping")
        );

        // GenerationConfig camelCase.
        assert_eq!(value["generationConfig"]["maxOutputTokens"], json!(64));
        assert_eq!(value["generationConfig"]["temperature"], json!(0.5));
        assert_eq!(
            value["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            json!(4096)
        );
        assert_eq!(
            value["generationConfig"]["thinkingConfig"]["includeThoughts"],
            json!(true)
        );
    }
}
