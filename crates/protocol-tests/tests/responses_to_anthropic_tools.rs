//! Cross-protocol smoke test: an OpenAI Responses-shaped `CanonicalRequest`
//! with multi-turn tool-call history routed through the Anthropic provider,
//! with the upstream Anthropic SSE re-encoded back into the OpenAI Responses
//! event stream.
//!
//! Pipeline exercised end-to-end:
//!
//! ```text
//! CanonicalRequest (frontend.kind = OpenAiResponses,
//!                   prior function_call/function_call_output history)
//!   → AnthropicProvider::complete()       [HTTP-mocked via mockito]
//!   → CanonicalStream                     [tool_use SSE]
//!   → OpenAiResponses::encode_stream(stream)
//!   → SSE bytes                           [function_call output item]
//! ```
//!
//! Verifies that the canonical glue between the Anthropic provider's
//! SSE parser and the OpenAI Responses encoder produces a well-formed
//! Responses event stream for a tool-use response (the upstream emits a
//! `tool_use` content block whose `input_json_delta` chunks must surface as
//! `response.function_call_arguments.delta` events, accumulate into a
//! single `response.function_call_arguments.done`, and the round-trip
//! preserves the upstream tool-use id verbatim as the Responses `call_id`).

use agent_shim_core::{
    CanonicalRequest, ContentBlock, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel,
    GenerationOptions, Message, MessageRole, RequestId, ToolCallArguments, ToolCallBlock,
    ToolCallId, ToolResultBlock,
};
use agent_shim_frontends::{openai_responses::OpenAiResponses, FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{
    collect_sse, make_anthropic_provider, make_anthropic_target, TEST_CLOCK,
};
use agent_shim_providers::BackendProvider;
use serde_json::Value;

/// Anthropic SSE response body for a tool-use completion (`get_weather`).
/// The arguments stream in two `input_json_delta` chunks (`{"city":` then
/// `"Paris"}`) which the Responses encoder must surface as two
/// `response.function_call_arguments.delta` events and accumulate into a
/// single `response.function_call_arguments.done` with the full JSON object.
const TOOL_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_tool_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-4-7\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\",\"input\":{}}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Paris\\\"}\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":7}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Build a multi-turn `CanonicalRequest` shaped as if it came from the
/// OpenAI Responses frontend after one full tool-call cycle:
///
/// 1. User asks about the weather in two cities.
/// 2. Assistant called `get_weather` for Paris (Responses
///    `function_call` → canonical `ToolCall`).
/// 3. The tool returned a result (Responses `function_call_output` →
///    canonical `ToolResult`).
///
/// The provider is now being asked to produce the next turn (which the
/// upstream mock answers with a fresh `tool_use` for Tokyo). The history
/// exercises the canonical→Anthropic mapping for prior tool-call/result
/// pairs without asserting deeply on the outbound body shape — that's
/// covered by `crates/providers/src/anthropic/request.rs` unit tests.
fn make_tool_history_request() -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::OpenAiResponses,
            requested_model: FrontendModel::from("gpt-4o"),
        },
        model: FrontendModel::from("gpt-4o"),
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
                    id: ToolCallId::from_provider("toolu_prev"),
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
                    tool_call_id: ToolCallId::from_provider("toolu_prev"),
                    content: Value::String("Sunny, 22C".to_string()),
                    is_error: false,
                    extensions: ExtensionMap::new(),
                })],
                name: None,
                extensions: ExtensionMap::new(),
            },
        ],
        tools: vec![],
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
async fn responses_to_anthropic_tools_round_trip() {
    let mut server = mockito::Server::new_async().await;

    // The outbound request body shape (canonical tool history → Anthropic
    // tool_use/tool_result blocks) is verified by the provider's own unit
    // tests; here we only assert the wire-shape pipeline runs end-to-end
    // and the OUTPUT events look right.
    let mock = server
        .mock("POST", "/v1/messages")
        .match_header("x-api-key", "test-key")
        .match_header("anthropic-version", "2023-06-01")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(TOOL_SSE)
        .create_async()
        .await;

    // Provider produces a CanonicalStream from the upstream Anthropic SSE.
    let provider = make_anthropic_provider(server.url());
    let canonical_stream = provider
        .complete(make_tool_history_request(), make_anthropic_target())
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

    // ── 1. response.created with the prefixed upstream id ────────────────
    assert!(
        text.contains("event: response.created"),
        "missing response.created\n{}",
        text
    );
    // Encoder prefixes "resp_" onto the upstream id ("msg_tool_1" →
    // "resp_msg_tool_1").
    assert!(
        text.contains("\"resp_msg_tool_1\""),
        "missing prefixed response id\n{}",
        text
    );

    // ── 2. function_call output_item.added carries the round-tripped id ──
    // The encoder maps Anthropic `tool_use.id` → Responses `call_id`
    // verbatim (no transformation), so `toolu_1` from the upstream must
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
        text.contains("\"call_id\":\"toolu_1\""),
        "missing round-tripped call_id 'toolu_1'\n{}",
        text
    );
    assert!(
        text.contains("\"name\":\"get_weather\""),
        "missing function name 'get_weather'\n{}",
        text
    );

    // ── 3. Two function_call_arguments.delta events with JSON fragments ──
    // The upstream `input_json_delta` chunks surface as `delta` strings on
    // the encoded events; the inner JSON quotes must be SSE-escaped.
    assert!(
        text.contains("event: response.function_call_arguments.delta"),
        "missing function_call_arguments.delta\n{}",
        text
    );
    // First fragment: `{"city":` (JSON-escaped: `{\"city\":`).
    assert!(
        text.contains(r#""delta":"{\"city\":""#),
        "missing first arguments delta '{{\"city\":'\n{}",
        text
    );
    // Second fragment: `"Paris"}` (JSON-escaped: `\"Paris\"}`).
    assert!(
        text.contains(r#""delta":"\"Paris\"}""#),
        "missing second arguments delta '\"Paris\"}}'\n{}",
        text
    );

    // ── 4. function_call_arguments.done with the accumulated arguments ───
    assert!(
        text.contains("event: response.function_call_arguments.done"),
        "missing function_call_arguments.done\n{}",
        text
    );
    assert!(
        text.contains(r#""arguments":"{\"city\":\"Paris\"}""#),
        "missing accumulated arguments '{{\"city\":\"Paris\"}}'\n{}",
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

    // ── 6. response.completed at the tail with usage tokens ──────────────
    assert!(
        text.contains("event: response.completed"),
        "missing response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"input_tokens\":12"),
        "missing input_tokens=12 on response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"output_tokens\":7"),
        "missing output_tokens=7 on response.completed\n{}",
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
                && e.contains(r#""call_id":"toolu_1""#)
        })
        .expect("function_call output_item.added (toolu_1) present");
    let first_args_delta_pos = events
        .iter()
        .position(|e| {
            e.starts_with("response.function_call_arguments.delta")
                && e.contains(r#""delta":"{\"city\":""#)
        })
        .expect("first function_call_arguments.delta present");
    let second_args_delta_pos = events
        .iter()
        .position(|e| {
            e.starts_with("response.function_call_arguments.delta")
                && e.contains(r#""delta":"\"Paris\"}""#)
        })
        .expect("second function_call_arguments.delta present");
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
        function_call_added_pos < first_args_delta_pos,
        "fc.added → first args delta"
    );
    assert!(
        first_args_delta_pos < second_args_delta_pos,
        "args deltas in upstream order"
    );
    assert!(
        second_args_delta_pos < args_done_pos,
        "args deltas → args.done"
    );
    assert!(args_done_pos < item_done_pos, "args.done → item.done");
    assert!(item_done_pos < completed_pos, "item.done → completed");

    // ── 8. response.completed frame contains the usage tokens ────────────
    let completed_frame = events
        .iter()
        .find(|e| e.starts_with("response.completed"))
        .expect("response.completed frame present");
    assert!(
        completed_frame.contains("\"input_tokens\":12"),
        "completed frame missing input_tokens=12\n{}",
        completed_frame
    );
    assert!(
        completed_frame.contains("\"output_tokens\":7"),
        "completed frame missing output_tokens=7\n{}",
        completed_frame
    );
}
