//! Cross-protocol smoke test: an OpenAI Responses-shaped `CanonicalRequest`
//! with multi-turn tool-call history routed through the Gemini provider,
//! with the upstream Gemini JSON-array `:streamGenerateContent` body
//! re-encoded back into the OpenAI Responses event stream.
//!
//! Pipeline exercised end-to-end:
//!
//! ```text
//! CanonicalRequest (frontend.kind = OpenAiResponses,
//!                   prior function_call/function_call_output history)
//!   → GeminiProvider::complete()           [HTTP-mocked via mockito]
//!   → CanonicalStream                      [functionCall in upstream body]
//!   → OpenAiResponses::encode_stream(stream)
//!   → SSE bytes                            [function_call output item]
//! ```
//!
//! Verifies two halves of the pipeline:
//!
//! 1. **Outbound (canonical → Gemini wire):** the prior `ToolCall` /
//!    `ToolResult` blocks in the request map to `functionCall` /
//!    `functionResponse` parts on `contents`, with role mapping
//!    `Assistant → "model"` and `Tool → "function"`. **`functionResponse`
//!    carries the originating `name` (D6), not the call_id** — Gemini
//!    requires the function name on tool replies.
//! 2. **Inbound (Gemini wire → Responses SSE):** a fresh `functionCall`
//!    in the upstream body surfaces as a Responses `function_call`
//!    output_item with the upstream `id` round-tripped verbatim as
//!    Responses `call_id`.

use agent_shim_core::{
    CanonicalRequest, ContentBlock, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel,
    GenerationOptions, Message, MessageRole, RequestId, ToolCallArguments, ToolCallBlock,
    ToolCallId, ToolDefinition, ToolResultBlock,
};
use agent_shim_frontends::{openai_responses::OpenAiResponses, FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{
    collect_sse, make_gemini_provider, make_gemini_target, TEST_CLOCK,
};
use agent_shim_providers::BackendProvider;
use serde_json::Value;

/// Gemini `:streamGenerateContent` JSON-array body — a single chunk with a
/// fresh `functionCall` for Tokyo. Gemini emits args as a real JSON object
/// (NOT stringified, unlike OpenAI Chat). The optional `id` field is
/// supplied so we can verify it round-trips into the Responses `call_id`.
const TOOL_JSON: &str = r#"[
{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"city":"Tokyo"},"id":"call_new"}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":15,"candidatesTokenCount":8,"totalTokenCount":23}}
]"#;

/// Build a multi-turn `CanonicalRequest` shaped as if it came from the
/// OpenAI Responses frontend after one full tool-call cycle:
///
/// 1. User asks about the weather in two cities.
/// 2. Assistant called `get_weather` for Paris (Responses
///    `function_call` → canonical `ToolCall`). The id uses Responses'
///    `call_*` prefix (not the Anthropic-flavored `toolu_*`).
/// 3. The tool returned a result (Responses `function_call_output` →
///    canonical `ToolResult`).
///
/// `tools` carries a single `get_weather` declaration so the Gemini
/// provider's `build_tools` path is exercised end-to-end.
fn make_tool_history_request() -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::OpenAiResponses,
            requested_model: FrontendModel::from("gemini-2.0-flash"),
        },
        model: FrontendModel::from("gemini-2.0-flash"),
        system: vec![],
        messages: vec![
            // 1. User asks about weather in two cities.
            Message::user(vec![ContentBlock::text(
                "What's the weather in Paris and Tokyo?",
            )]),
            // 2. Assistant previously called get_weather for Paris.
            Message {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::ToolCall(ToolCallBlock {
                    id: ToolCallId::from_provider("call_prev"),
                    name: "get_weather".to_string(),
                    arguments: ToolCallArguments::Complete {
                        value: serde_json::json!({"city": "Paris"}),
                    },
                    extensions: ExtensionMap::new(),
                })],
                name: None,
                extensions: ExtensionMap::new(),
            },
            // 3. Tool returned a result for the prior call.
            Message {
                role: MessageRole::Tool,
                content: vec![ContentBlock::ToolResult(ToolResultBlock {
                    tool_call_id: ToolCallId::from_provider("call_prev"),
                    content: Value::String("Sunny, 22C".to_string()),
                    is_error: false,
                    extensions: ExtensionMap::new(),
                })],
                name: None,
                extensions: ExtensionMap::new(),
            },
        ],
        tools: vec![ToolDefinition {
            name: "get_weather".to_string(),
            description: Some("Get current weather for a city.".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "city": {"type": "string"}
                }
            }),
            extensions: ExtensionMap::new(),
        }],
        tool_choice: Default::default(),
        generation: GenerationOptions::default(),
        response_format: None,
        stream: true,
        metadata: Default::default(),
        inbound_anthropic_headers: vec![],
        resolved_policy: Default::default(),
        extensions: ExtensionMap::new(),
    }
}

#[tokio::test]
async fn responses_to_gemini_tools_round_trip() {
    let mut server = mockito::Server::new_async().await;

    // Outbound body assertions (D6 verification): match the streaming URL
    // AND verify the canonical→Gemini encoding produced the right shape on
    // the wire. `Matcher::PartialJsonString` checks the body JSON contains
    // the supplied subset — perfect for a multi-key/multi-element shape
    // assertion without pinning fields we don't care about.
    let mock = server
        .mock(
            "POST",
            mockito::Matcher::Regex(
                r"/models/gemini-2\.0-flash:streamGenerateContent.*key=test-key".into(),
            ),
        )
        .match_body(mockito::Matcher::AllOf(vec![
            // contents[0]: user turn carries the original question.
            mockito::Matcher::PartialJsonString(
                r#"{"contents":[{"role":"user","parts":[{"text":"What's the weather in Paris and Tokyo?"}]}]}"#
                    .into(),
            ),
            // contents[1]: prior assistant tool call → role "model" + functionCall.
            // Args MUST be a real JSON object (not stringified, unlike OpenAI Chat).
            mockito::Matcher::PartialJsonString(
                r#"{"contents":[{},{"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"city":"Paris"}}}]}]}"#
                    .into(),
            ),
            // contents[2]: prior tool reply → role "function" + functionResponse.
            // CRUCIAL D6 assertion: functionResponse.name MUST be "get_weather"
            // (the originating function name), NOT the call_id "call_prev" —
            // Gemini requires the name on tool replies. The provider's
            // `collect_tool_call_names` walks prior ToolCall blocks to look
            // this up. The string content is wrapped as `{"content": ...}`
            // since Gemini's `response` field expects an object.
            mockito::Matcher::PartialJsonString(
                r#"{"contents":[{},{},{"role":"function","parts":[{"functionResponse":{"name":"get_weather","response":{"content":"Sunny, 22C"}}}]}]}"#
                    .into(),
            ),
            // tools[0]: ToolDefinition becomes a single Tool wrapper carrying
            // every functionDeclaration.
            mockito::Matcher::PartialJsonString(
                r#"{"tools":[{"functionDeclarations":[{"name":"get_weather"}]}]}"#.into(),
            ),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(TOOL_JSON)
        .create_async()
        .await;

    // Provider produces a CanonicalStream from the upstream Gemini JSON array.
    let provider = make_gemini_provider(server.url());
    let canonical_stream = provider
        .complete(make_tool_history_request(), make_gemini_target())
        .await
        .expect("provider returned a canonical stream");

    // Pipe the canonical stream through the OpenAI Responses encoder.
    let frontend = OpenAiResponses {
        keepalive: None,
        clock_override: Some(TEST_CLOCK),
    };
    let response_stream = frontend.encode_stream(canonical_stream);
    let body = match response_stream {
        FrontendResponse::Stream { stream, .. } => collect_sse(stream).await,
        FrontendResponse::Unary { .. } => panic!("expected stream"),
    };
    let text = String::from_utf8(body.to_vec()).expect("utf8 sse");

    mock.assert_async().await;

    // ── 1. response.created arrives with a "resp_<...>" id ───────────────
    // Gemini's parser mints a fresh ResponseId per response (no upstream
    // id to echo), so the encoder's "resp_" prefix lands on a UUID.
    assert!(
        text.contains("event: response.created"),
        "missing response.created\n{}",
        text
    );
    assert!(
        text.contains("\"id\":\"resp_"),
        "missing resp_-prefixed response id\n{}",
        text
    );

    // ── 2. function_call output_item.added carries the round-tripped id ──
    // The Gemini parser preserves `FunctionCall.id` verbatim onto the
    // canonical `ToolCallId`, and the Responses encoder maps that → Responses
    // `call_id` verbatim. So `call_new` from the upstream `id` field must
    // appear unchanged in the function_call output item.
    assert!(
        text.contains("event: response.output_item.added"),
        "missing output_item.added\n{}",
        text
    );
    assert!(
        text.contains("\"type\":\"function_call\""),
        "missing function_call item type\n{}",
        text
    );
    assert!(
        text.contains("\"call_id\":\"call_new\""),
        "missing round-tripped call_id 'call_new'\n{}",
        text
    );
    assert!(
        text.contains("\"name\":\"get_weather\""),
        "missing function name 'get_weather'\n{}",
        text
    );

    // ── 3. function_call_arguments.delta carries the full args object ────
    // Gemini emits args as a complete JSON object in one canonical event
    // (the parser serializes the object once as a single fragment), so the
    // delta is a single chunk carrying the whole stringified JSON. The
    // inner JSON quotes must be SSE/JSON-escaped on the wire.
    assert!(
        text.contains("event: response.function_call_arguments.delta"),
        "missing function_call_arguments.delta\n{}",
        text
    );
    assert!(
        text.contains(r#""delta":"{\"city\":\"Tokyo\"}""#),
        "missing arguments delta '{{\"city\":\"Tokyo\"}}'\n{}",
        text
    );

    // ── 4. function_call_arguments.done with the accumulated arguments ───
    assert!(
        text.contains("event: response.function_call_arguments.done"),
        "missing function_call_arguments.done\n{}",
        text
    );
    assert!(
        text.contains(r#""arguments":"{\"city\":\"Tokyo\"}""#),
        "missing accumulated arguments '{{\"city\":\"Tokyo\"}}'\n{}",
        text
    );

    // ── 5. output_item.done at completed status for the function call ────
    assert!(
        text.contains("event: response.output_item.done"),
        "missing output_item.done\n{}",
        text
    );
    assert!(
        text.contains("\"status\":\"completed\""),
        "missing completed status on output_item.done\n{}",
        text
    );

    // ── 6. response.completed at the tail with usage from usageMetadata ──
    assert!(
        text.contains("event: response.completed"),
        "missing response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"input_tokens\":15"),
        "missing input_tokens=15 on response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"output_tokens\":8"),
        "missing output_tokens=8 on response.completed\n{}",
        text
    );

    // ── 7. Critical orderings: split per-event and verify positions ──────
    // Each entry is one event block (`event: name\ndata: {...}`).
    let events: Vec<&str> = text.split("event: ").skip(1).collect();

    let created_pos = events
        .iter()
        .position(|e| e.starts_with("response.created"))
        .expect("response.created present");
    let function_call_added_pos = events
        .iter()
        .position(|e| {
            e.starts_with("response.output_item.added")
                && e.contains(r#""type":"function_call""#)
                && e.contains(r#""call_id":"call_new""#)
        })
        .expect("function_call output_item.added (call_new) present");
    let args_delta_pos = events
        .iter()
        .position(|e| {
            e.starts_with("response.function_call_arguments.delta")
                && e.contains(r#""delta":"{\"city\":\"Tokyo\"}""#)
        })
        .expect("function_call_arguments.delta (Tokyo) present");
    let args_done_pos = events
        .iter()
        .position(|e| e.starts_with("response.function_call_arguments.done"))
        .expect("function_call_arguments.done present");
    let item_done_pos = events
        .iter()
        .position(|e| {
            e.starts_with("response.output_item.done") && e.contains(r#""type":"function_call""#)
        })
        .expect("function_call output_item.done present");
    let completed_pos = events
        .iter()
        .position(|e| e.starts_with("response.completed"))
        .expect("response.completed present");

    assert!(created_pos < function_call_added_pos, "created → fc.added");
    assert!(
        function_call_added_pos < args_delta_pos,
        "fc.added → args delta"
    );
    assert!(args_delta_pos < args_done_pos, "args delta → args.done");
    assert!(args_done_pos < item_done_pos, "args.done → item.done");
    assert!(item_done_pos < completed_pos, "item.done → completed");

    // ── 8. response.completed frame contains the usage tokens ────────────
    let completed_frame = events
        .iter()
        .find(|e| e.starts_with("response.completed"))
        .expect("response.completed frame present");
    assert!(
        completed_frame.contains("\"input_tokens\":15"),
        "completed frame missing input_tokens=15\n{}",
        completed_frame
    );
    assert!(
        completed_frame.contains("\"output_tokens\":8"),
        "completed frame missing output_tokens=8\n{}",
        completed_frame
    );
}
