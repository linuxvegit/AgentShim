//! Cross-protocol smoke test: an OpenAI Responses-shaped `CanonicalRequest`
//! routed through the Anthropic provider, with the upstream Anthropic SSE
//! re-encoded back into the OpenAI Responses event stream.
//!
//! Pipeline exercised end-to-end:
//!
//! ```text
//! CanonicalRequest (frontend.kind = OpenAiResponses)
//!   → AnthropicProvider::complete()       [HTTP-mocked via mockito]
//!   → CanonicalStream
//!   → OpenAiResponses::encode_stream(stream)
//!   → SSE bytes
//! ```
//!
//! Verifies that the canonical glue between the Anthropic provider's
//! SSE parser and the OpenAI Responses encoder produces a well-formed
//! Responses event stream for a text completion (deltas accumulate into
//! a single `output_text.done`, lifecycle events fire in the right order,
//! and `response.completed` carries usage).

use agent_shim_core::{
    BackendTarget, CanonicalRequest, ContentBlock, ExtensionMap, FrontendInfo, FrontendKind,
    FrontendModel, GenerationOptions, Message, RequestId,
};
use agent_shim_frontends::{openai_responses::OpenAiResponses, FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::collect_sse;
use agent_shim_providers::{anthropic::AnthropicProvider, BackendProvider};

/// Anthropic SSE response body — the upstream emits the assistant's text
/// in two `text_delta` chunks (`"Hello"` then `", world!"`). The Responses
/// encoder must accumulate these into a single `output_text.done` payload
/// containing `"Hello, world!"`.
const TEXT_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_text_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-4-7\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\", world!\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":3}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

fn make_provider(base_url: String) -> AnthropicProvider {
    AnthropicProvider::new(
        "anthropic",
        base_url,
        "test-key",
        "2023-06-01",
        Default::default(),
        30,
    )
    .unwrap()
}

fn make_target() -> BackendTarget {
    BackendTarget {
        provider: "anthropic".to_string(),
        model: "claude-opus-4-7".to_string(),
        policy: Default::default(),
    }
}

/// Build a `CanonicalRequest` shaped as if it came from the OpenAI Responses
/// frontend. The provider's canonical path (`complete()`) is what we want to
/// exercise — gating on `FrontendKind::OpenAiResponses` is verified by the
/// passthrough tests.
fn make_req(stream: bool, frontend_kind: FrontendKind) -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: frontend_kind,
            requested_model: FrontendModel::from("gpt-4o"),
        },
        model: FrontendModel::from("gpt-4o"),
        system: vec![],
        messages: vec![Message::user(vec![ContentBlock::text("hello")])],
        tools: vec![],
        tool_choice: Default::default(),
        generation: GenerationOptions::default(),
        response_format: None,
        stream,
        metadata: Default::default(),
        inbound_anthropic_headers: vec![],
        resolved_policy: Default::default(),
        extensions: ExtensionMap::new(),
    }
}

#[tokio::test]
async fn responses_to_anthropic_text_round_trip_emits_well_formed_responses_sse() {
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("POST", "/v1/messages")
        .match_header("x-api-key", "test-key")
        .match_header("anthropic-version", "2023-06-01")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(TEXT_SSE)
        .create_async()
        .await;

    // Provider produces a CanonicalStream from the upstream Anthropic SSE.
    let provider = make_provider(server.url());
    let canonical_stream = provider
        .complete(make_req(true, FrontendKind::OpenAiResponses), make_target())
        .await
        .expect("provider returned a canonical stream");

    // Pipe the canonical stream through the OpenAI Responses encoder.
    let frontend = OpenAiResponses {
        keepalive: None,
        clock_override: Some(1700000000),
    };
    let response_stream = frontend.encode_stream(canonical_stream);
    let body = match response_stream {
        FrontendResponse::Stream { stream, .. } => collect_sse(stream).await,
        FrontendResponse::Unary { .. } => panic!("expected stream"),
    };
    let text = String::from_utf8(body.to_vec()).expect("utf8 sse");

    mock.assert_async().await;

    // ── 1. response.created arrives with the prefixed upstream id ────────
    assert!(
        text.contains("event: response.created"),
        "missing response.created\n{}",
        text
    );
    // Encoder prefixes "resp_" onto whatever id the upstream gave us
    // ("msg_text_1" → "resp_msg_text_1").
    assert!(
        text.contains("\"resp_msg_text_1\""),
        "missing prefixed response id\n{}",
        text
    );

    // ── 2. Output item lifecycle for the assistant message ───────────────
    assert!(
        text.contains("event: response.output_item.added"),
        "missing output_item.added\n{}",
        text
    );
    assert!(
        text.contains("\"type\":\"message\""),
        "missing message item type\n{}",
        text
    );
    // First (and only) assistant message lands at output_index 0 → "msg_0".
    assert!(
        text.contains("\"id\":\"msg_0\""),
        "missing msg_0 item id\n{}",
        text
    );
    assert!(
        text.contains("event: response.content_part.added"),
        "missing content_part.added\n{}",
        text
    );

    // ── 3. Each upstream text_delta surfaces as an output_text.delta ─────
    assert!(
        text.contains("\"delta\":\"Hello\""),
        "missing 'Hello' delta\n{}",
        text
    );
    assert!(
        text.contains("\"delta\":\", world!\""),
        "missing ', world!' delta\n{}",
        text
    );

    // ── 4. output_text.done carries the accumulated text ─────────────────
    assert!(
        text.contains("event: response.output_text.done"),
        "missing output_text.done\n{}",
        text
    );
    assert!(
        text.contains("\"text\":\"Hello, world!\""),
        "missing accumulated 'Hello, world!' text\n{}",
        text
    );

    // ── 5. content_part.done and output_item.done at completed status ────
    assert!(
        text.contains("event: response.content_part.done"),
        "missing content_part.done\n{}",
        text
    );
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

    // ── 6. response.completed at the tail with usage ─────────────────────
    assert!(
        text.contains("event: response.completed"),
        "missing response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"input_tokens\":5"),
        "missing input_tokens=5 on response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"output_tokens\":3"),
        "missing output_tokens=3 on response.completed\n{}",
        text
    );

    // ── 7. Critical orderings: split per-event and verify positions ──────
    // Each entry is one event block (`event: name\ndata: {...}`).
    let events: Vec<&str> = text.split("event: ").skip(1).collect();

    let created_pos = events
        .iter()
        .position(|e| e.starts_with("response.created"))
        .expect("response.created present");
    let item_added_pos = events
        .iter()
        .position(|e| {
            e.starts_with("response.output_item.added") && e.contains(r#""type":"message""#)
        })
        .expect("response.output_item.added (message) present");
    let part_added_pos = events
        .iter()
        .position(|e| e.starts_with("response.content_part.added"))
        .expect("response.content_part.added present");
    let first_delta_pos = events
        .iter()
        .position(|e| e.starts_with("response.output_text.delta") && e.contains(r#""Hello""#))
        .expect("first output_text.delta ('Hello') present");
    let second_delta_pos = events
        .iter()
        .position(|e| e.starts_with("response.output_text.delta") && e.contains(r#"", world!""#))
        .expect("second output_text.delta (', world!') present");
    let text_done_pos = events
        .iter()
        .position(|e| e.starts_with("response.output_text.done"))
        .expect("response.output_text.done present");
    let part_done_pos = events
        .iter()
        .position(|e| e.starts_with("response.content_part.done"))
        .expect("response.content_part.done present");
    let item_done_pos = events
        .iter()
        .position(|e| e.starts_with("response.output_item.done"))
        .expect("response.output_item.done present");
    let completed_pos = events
        .iter()
        .position(|e| e.starts_with("response.completed"))
        .expect("response.completed present");

    assert!(created_pos < item_added_pos, "created → item.added");
    assert!(item_added_pos < part_added_pos, "item.added → part.added");
    assert!(part_added_pos < first_delta_pos, "part.added → first delta");
    assert!(
        first_delta_pos < second_delta_pos,
        "deltas in upstream order"
    );
    assert!(second_delta_pos < text_done_pos, "deltas → text.done");
    assert!(text_done_pos < part_done_pos, "text.done → part.done");
    assert!(part_done_pos < item_done_pos, "part.done → item.done");
    assert!(item_done_pos < completed_pos, "item.done → completed");
}
