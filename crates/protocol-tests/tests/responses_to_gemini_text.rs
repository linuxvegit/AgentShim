//! Cross-protocol smoke test: an OpenAI Responses-shaped `CanonicalRequest`
//! routed through the Gemini provider, with the upstream Gemini JSON-array
//! `:streamGenerateContent` body re-encoded back into the OpenAI Responses
//! event stream.
//!
//! Pipeline exercised end-to-end:
//!
//! ```text
//! CanonicalRequest (frontend.kind = OpenAiResponses)
//!   → GeminiProvider::complete()           [HTTP-mocked via mockito]
//!   → CanonicalStream
//!   → OpenAiResponses::encode_stream(stream)
//!   → SSE bytes
//! ```
//!
//! Verifies that the canonical glue between the Gemini provider's
//! JSON-array stream parser and the OpenAI Responses encoder produces a
//! well-formed Responses event stream for a text completion (deltas
//! accumulate into a single `output_text.done`, lifecycle events fire in
//! the right order, and `response.completed` carries usage from
//! `usageMetadata`).

use agent_shim_core::FrontendKind;
use agent_shim_frontends::{openai_responses::OpenAiResponses, FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{
    collect_sse, make_canonical_request, make_gemini_provider, make_gemini_target, TEST_CLOCK,
};
use agent_shim_providers::BackendProvider;

/// Gemini `:streamGenerateContent` JSON-array body — the upstream emits the
/// assistant's text in two `text` parts (`"Hello"` then `" world"`). The
/// Responses encoder must accumulate these into a single `output_text.done`
/// payload containing `"Hello world"`. The second response carries
/// `usageMetadata`, which must surface on `response.completed`.
const TEXT_JSON: &str = r#"[
{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]}}]},
{"candidates":[{"content":{"role":"model","parts":[{"text":" world"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}}
]"#;

#[tokio::test]
async fn responses_to_gemini_text_round_trip() {
    let mut server = mockito::Server::new_async().await;

    // Gemini auth is via `?key=<api_key>` query string, not headers — match
    // on the streaming URL pattern (regex) + the api key in the query.
    let mock = server
        .mock(
            "POST",
            mockito::Matcher::Regex(
                r"/models/gemini-2\.0-flash:streamGenerateContent.*key=test-key".into(),
            ),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(TEXT_JSON)
        .create_async()
        .await;

    // Provider produces a CanonicalStream from the upstream Gemini JSON array.
    let provider = make_gemini_provider(server.url());
    let canonical_stream = provider
        .complete(
            make_canonical_request(true, FrontendKind::OpenAiResponses),
            make_gemini_target(),
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

    // ── 3. Each upstream text part surfaces as an output_text.delta ──────
    assert!(
        text.contains("\"delta\":\"Hello\""),
        "missing 'Hello' delta\n{}",
        text
    );
    assert!(
        text.contains("\"delta\":\" world\""),
        "missing ' world' delta\n{}",
        text
    );

    // ── 4. output_text.done carries the accumulated text ─────────────────
    assert!(
        text.contains("event: response.output_text.done"),
        "missing output_text.done\n{}",
        text
    );
    assert!(
        text.contains("\"text\":\"Hello world\""),
        "missing accumulated 'Hello world' text\n{}",
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

    // ── 6. response.completed at the tail with usage from usageMetadata ──
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
        text.contains("\"output_tokens\":2"),
        "missing output_tokens=2 on response.completed\n{}",
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
        .position(|e| e.starts_with("response.output_text.delta") && e.contains(r#"" world""#))
        .expect("second output_text.delta (' world') present");
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
