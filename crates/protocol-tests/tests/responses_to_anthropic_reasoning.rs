//! Cross-protocol smoke test: an OpenAI Responses-shaped `CanonicalRequest`
//! routed through the Anthropic provider, with the upstream Anthropic SSE
//! (containing a thinking block followed by a text block) re-encoded back
//! into the OpenAI Responses event stream.
//!
//! Pipeline exercised end-to-end:
//!
//! ```text
//! CanonicalRequest (frontend.kind = OpenAiResponses)
//!   → AnthropicProvider::complete()       [HTTP-mocked via mockito]
//!   → CanonicalStream                     [thinking + text content blocks,
//!                                          ReasoningDelta + TextDelta events]
//!   → OpenAiResponses::encode_stream(stream)
//!   → SSE bytes                           [reasoning.{delta,done} + text
//!                                          lifecycle from Plan 01 T3]
//! ```
//!
//! Verifies that the canonical glue between the Anthropic provider's SSE
//! parser and the OpenAI Responses encoder produces a well-formed Responses
//! event stream when the upstream emits a thinking block (deltas accumulate
//! into a single `response.reasoning.done`) followed by a text block.
//!
//! ## Signature drop (negative assertion)
//!
//! Anthropic also emits `signature_delta` events on thinking blocks carrying
//! a base64 signature. Per `crates/providers/src/anthropic/response.rs:9-23`
//! and `:201-205` (Plan 02 D2 design decision), the canonical path drops
//! signature deltas with a debug log because the canonical `Reasoning`
//! content block has no signature channel today. This test includes a
//! `signature_delta` in the upstream SSE and asserts the encoder neither
//! crashes nor leaks the signature into the encoded Responses output.

use agent_shim_core::FrontendKind;
use agent_shim_frontends::{openai_responses::OpenAiResponses, FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{
    collect_sse, make_anthropic_provider, make_anthropic_target, make_canonical_request, TEST_CLOCK,
};
use agent_shim_providers::BackendProvider;

/// Anthropic SSE response body — upstream emits a thinking block (two
/// `thinking_delta` chunks plus a `signature_delta` we expect to be dropped)
/// followed by a text block. The Responses encoder must surface the
/// reasoning lifecycle (output_item.added/reasoning.delta×2/reasoning.done/
/// output_item.done) on output_index 0 with item id `rs_0`, then the text
/// lifecycle (output_item.added/content_part.added/output_text.delta/
/// output_text.done/content_part.done/output_item.done) on output_index 1
/// with item id `msg_1`.
const REASONING_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_reason_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-4-7\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":15,\"output_tokens\":0}}}\n\n",
    // ── Thinking block at index 0 ────────────────────────────────────────
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me think \"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"about this.\"}}\n\n",
    // Anthropic also emits a `signature_delta` on thinking blocks; the
    // canonical path drops it (lossy by design — see response.rs module
    // doc / Plan 02 D2). The encoder must not see this and must not leak
    // `sig-abc123` into the Responses output stream.
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-abc123\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    // ── Text block at index 1 ────────────────────────────────────────────
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"The answer is 42.\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":8}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

#[tokio::test]
async fn responses_to_anthropic_reasoning_round_trip() {
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("POST", "/v1/messages")
        .match_header("x-api-key", "test-key")
        .match_header("anthropic-version", "2023-06-01")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(REASONING_SSE)
        .create_async()
        .await;

    // Provider produces a CanonicalStream from the upstream Anthropic SSE.
    // The simple "hello" inbound request body is fine — this test cares
    // about the OUTBOUND reasoning round-trip, not the inbound shape.
    let provider = make_anthropic_provider(server.url());
    let canonical_stream = provider
        .complete(
            make_canonical_request(true, FrontendKind::OpenAiResponses),
            make_anthropic_target(),
        )
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
    // Encoder prefixes "resp_" onto the upstream id ("msg_reason_1" →
    // "resp_msg_reason_1").
    assert!(
        text.contains("\"resp_msg_reason_1\""),
        "missing prefixed response id\n{}",
        text
    );

    // ── 2. Reasoning lifecycle (output_index 0, item id rs_0) ────────────
    assert!(
        text.contains("event: response.output_item.added"),
        "missing output_item.added\n{}",
        text
    );
    assert!(
        text.contains("\"type\":\"reasoning\""),
        "missing reasoning item type\n{}",
        text
    );
    assert!(
        text.contains("\"id\":\"rs_0\""),
        "missing rs_0 item id\n{}",
        text
    );

    // Each upstream thinking_delta surfaces as a response.reasoning.delta.
    assert!(
        text.contains("event: response.reasoning.delta"),
        "missing response.reasoning.delta\n{}",
        text
    );
    assert!(
        text.contains("\"delta\":\"Let me think \""),
        "missing 'Let me think ' reasoning delta\n{}",
        text
    );
    assert!(
        text.contains("\"delta\":\"about this.\""),
        "missing 'about this.' reasoning delta\n{}",
        text
    );

    // response.reasoning.done carries the accumulated thinking text.
    assert!(
        text.contains("event: response.reasoning.done"),
        "missing response.reasoning.done\n{}",
        text
    );
    assert!(
        text.contains("\"text\":\"Let me think about this.\""),
        "missing accumulated reasoning text\n{}",
        text
    );

    // ── 3. Signature drop: substring must NOT leak into encoded output ───
    // Per response.rs:201-205, signature_delta is dropped on the canonical
    // path with a debug log. The base64 signature payload must never
    // appear in the Responses SSE bytes.
    assert!(
        !text.contains("sig-abc123"),
        "signature leaked into Responses output (canonical path should drop \
         signature_delta — see crates/providers/src/anthropic/response.rs:9-23)\n{}",
        text
    );
    assert!(
        !text.contains("signature"),
        "literal 'signature' field leaked into Responses output\n{}",
        text
    );

    // ── 4. Text lifecycle (output_index 1, item id msg_1) ────────────────
    assert!(
        text.contains("\"type\":\"message\""),
        "missing message item type\n{}",
        text
    );
    // Second content block (text, after thinking) lands at output_index 1.
    assert!(
        text.contains("\"id\":\"msg_1\""),
        "missing msg_1 item id\n{}",
        text
    );
    assert!(
        text.contains("event: response.content_part.added"),
        "missing content_part.added\n{}",
        text
    );
    assert!(
        text.contains("\"delta\":\"The answer is 42.\""),
        "missing 'The answer is 42.' text delta\n{}",
        text
    );
    assert!(
        text.contains("event: response.output_text.done"),
        "missing output_text.done\n{}",
        text
    );
    assert!(
        text.contains("\"text\":\"The answer is 42.\""),
        "missing accumulated 'The answer is 42.' text\n{}",
        text
    );
    assert!(
        text.contains("event: response.content_part.done"),
        "missing content_part.done\n{}",
        text
    );

    // ── 5. response.completed at the tail with usage ─────────────────────
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

    // ── 6. Critical orderings: split per-event and verify positions ──────
    // Each entry is one event block (`event: name\ndata: {...}`).
    let events: Vec<&str> = text.split("event: ").skip(1).collect();

    let created_pos = events
        .iter()
        .position(|e| e.starts_with("response.created"))
        .expect("response.created present");

    // Reasoning lifecycle on output_index 0.
    let reasoning_added_pos = events
        .iter()
        .position(|e| {
            e.starts_with("response.output_item.added")
                && e.contains(r#""type":"reasoning""#)
                && e.contains(r#""id":"rs_0""#)
        })
        .expect("reasoning output_item.added (rs_0) present");
    let first_reasoning_delta_pos = events
        .iter()
        .position(|e| {
            e.starts_with("response.reasoning.delta") && e.contains(r#""delta":"Let me think ""#)
        })
        .expect("first reasoning.delta ('Let me think ') present");
    let second_reasoning_delta_pos = events
        .iter()
        .position(|e| {
            e.starts_with("response.reasoning.delta") && e.contains(r#""delta":"about this.""#)
        })
        .expect("second reasoning.delta ('about this.') present");
    let reasoning_done_pos = events
        .iter()
        .position(|e| e.starts_with("response.reasoning.done"))
        .expect("response.reasoning.done present");
    let reasoning_item_done_pos = events
        .iter()
        .position(|e| {
            e.starts_with("response.output_item.done") && e.contains(r#""type":"reasoning""#)
        })
        .expect("reasoning output_item.done present");

    // Text lifecycle on output_index 1.
    let message_added_pos = events
        .iter()
        .position(|e| {
            e.starts_with("response.output_item.added")
                && e.contains(r#""type":"message""#)
                && e.contains(r#""id":"msg_1""#)
        })
        .expect("message output_item.added (msg_1) present");
    let part_added_pos = events
        .iter()
        .position(|e| e.starts_with("response.content_part.added"))
        .expect("content_part.added present");
    let text_delta_pos = events
        .iter()
        .position(|e| {
            e.starts_with("response.output_text.delta")
                && e.contains(r#""delta":"The answer is 42.""#)
        })
        .expect("output_text.delta present");
    let text_done_pos = events
        .iter()
        .position(|e| e.starts_with("response.output_text.done"))
        .expect("output_text.done present");
    let part_done_pos = events
        .iter()
        .position(|e| e.starts_with("response.content_part.done"))
        .expect("content_part.done present");
    let message_item_done_pos = events
        .iter()
        .position(|e| {
            e.starts_with("response.output_item.done") && e.contains(r#""type":"message""#)
        })
        .expect("message output_item.done present");
    let completed_pos = events
        .iter()
        .position(|e| e.starts_with("response.completed"))
        .expect("response.completed present");

    // Reasoning lifecycle ordering.
    assert!(
        created_pos < reasoning_added_pos,
        "created → reasoning.added"
    );
    assert!(
        reasoning_added_pos < first_reasoning_delta_pos,
        "reasoning.added → first reasoning.delta"
    );
    assert!(
        first_reasoning_delta_pos < second_reasoning_delta_pos,
        "reasoning deltas in upstream order"
    );
    assert!(
        second_reasoning_delta_pos < reasoning_done_pos,
        "reasoning deltas → reasoning.done"
    );
    assert!(
        reasoning_done_pos < reasoning_item_done_pos,
        "reasoning.done → reasoning output_item.done"
    );

    // Reasoning fully closes before text lifecycle opens.
    assert!(
        reasoning_item_done_pos < message_added_pos,
        "reasoning item.done → message.added"
    );

    // Text lifecycle ordering.
    assert!(
        message_added_pos < part_added_pos,
        "message.added → part.added"
    );
    assert!(part_added_pos < text_delta_pos, "part.added → text.delta");
    assert!(text_delta_pos < text_done_pos, "text.delta → text.done");
    assert!(text_done_pos < part_done_pos, "text.done → part.done");
    assert!(
        part_done_pos < message_item_done_pos,
        "part.done → message item.done"
    );

    // Tail.
    assert!(
        message_item_done_pos < completed_pos,
        "message item.done → completed"
    );

    // ── 7. response.completed frame contains the usage tokens ────────────
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
