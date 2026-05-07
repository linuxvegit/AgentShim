//! Cross-protocol smoke test: an OpenAI Responses-shaped `CanonicalRequest`
//! with `reasoning: { effort: medium }` routed through the Gemini provider,
//! with the upstream Gemini JSON-array `:streamGenerateContent` body
//! (carrying `thoughts: true` parts mixed with regular content) re-encoded
//! back into the OpenAI Responses event stream.
//!
//! Pipeline exercised end-to-end:
//!
//! ```text
//! CanonicalRequest (frontend.kind = OpenAiResponses,
//!                   generation.reasoning.effort = Medium)
//!   → GeminiProvider::complete()           [HTTP-mocked via mockito]
//!   → CanonicalStream                      [thought parts → Reasoning block,
//!                                           non-thought parts → Text block]
//!   → OpenAiResponses::encode_stream(stream)
//!   → SSE bytes                            [reasoning.{delta,done} + text
//!                                           lifecycle from Plan 01 T3]
//! ```
//!
//! Verifies two halves of the pipeline:
//!
//! 1. **Outbound (canonical → Gemini wire):** Medium effort lands on outbound
//!    JSON as `thinkingConfig.thinkingBudget = 1024` with `includeThoughts =
//!    true`. The mapping is from `effort_to_budget` in
//!    `crates/providers/src/gemini/request.rs:463-471` — D5 budget mapping
//!    verification.
//! 2. **Inbound (Gemini wire → Responses SSE):** consecutive `thought: true`
//!    parts coalesce into one `Reasoning` block with two `ReasoningDelta`s,
//!    then the non-thought part opens a fresh `Text` block. The encoder
//!    surfaces them as `response.reasoning.{delta,done}` followed by
//!    `response.output_text.{delta,done}` on output_index 0 (rs_0) then
//!    output_index 1 (msg_1).

use agent_shim_core::{
    request::{ReasoningEffort, ReasoningOptions},
    BackendTarget, FrontendKind,
};
use agent_shim_frontends::{openai_responses::OpenAiResponses, FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{
    collect_sse, make_canonical_request, make_gemini_provider, TEST_CLOCK,
};
use agent_shim_providers::BackendProvider;

/// Gemini `:streamGenerateContent` JSON-array body — the upstream emits two
/// `thought: true` parts (`"Let me think "` then `"about this."`) and then a
/// non-thought part (`"The answer is 42."`). The streaming parser at
/// `crates/providers/src/gemini/response.rs::handle_part` coalesces
/// consecutive thought parts into one `Reasoning` block with two
/// `ReasoningDelta` events, closes that block on the thought→text transition,
/// and opens a fresh `Text` block for the regular content.
///
/// `usageMetadata` on the last chunk carries both regular and reasoning
/// token counts; only the regular `input_tokens`/`output_tokens` reach
/// `response.completed` (the encoder doesn't surface reasoning_tokens
/// separately today). `finishReason: STOP` triggers MessageStop +
/// ResponseStop, which closes the outer lifecycle.
const THINKING_JSON: &str = r#"[
{"candidates":[{"content":{"role":"model","parts":[{"text":"Let me think ","thought":true}]}}]},
{"candidates":[{"content":{"role":"model","parts":[{"text":"about this.","thought":true}]}}]},
{"candidates":[{"content":{"role":"model","parts":[{"text":"The answer is 42."}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":15,"candidatesTokenCount":8,"thoughtsTokenCount":4,"totalTokenCount":27}}
]"#;

/// Build a `BackendTarget` pointing at `gemini-2.5-flash-thinking` on the
/// `gemini` provider. The helper `make_gemini_target()` returns
/// `gemini-2.0-flash`, which doesn't match the URL we want to assert on.
fn make_gemini_thinking_target() -> BackendTarget {
    BackendTarget {
        provider: "gemini".to_string(),
        model: "gemini-2.5-flash-thinking".to_string(),
        policy: Default::default(),
    }
}

#[tokio::test]
async fn responses_to_gemini_thinking_round_trip() {
    let mut server = mockito::Server::new_async().await;

    // Outbound body assertion (D5 verification): match the streaming URL
    // for the thinking model AND verify the canonical→Gemini encoding
    // emitted `thinkingConfig.thinkingBudget = 1024` with
    // `includeThoughts = true`. Medium effort → 1024 per
    // `effort_to_budget` in
    // `crates/providers/src/gemini/request.rs:463-471`.
    let mock = server
        .mock(
            "POST",
            mockito::Matcher::Regex(
                r"/models/gemini-2\.5-flash-thinking:streamGenerateContent.*key=test-key".into(),
            ),
        )
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::PartialJsonString(
                r#"{"generationConfig":{"thinkingConfig":{"thinkingBudget":1024,"includeThoughts":true}}}"#
                    .into(),
            ),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(THINKING_JSON)
        .create_async()
        .await;

    // Build a Responses-flavored request and turn on Medium effort. The
    // canonical pipeline normally copies `generation.reasoning.effort` into
    // `resolved_policy.reasoning_effort` via `RoutePolicy::resolve`, but
    // since this is a standalone test we exercise source #3 (request-level
    // effort) of the `thinking_config` precedence ladder.
    let mut req = make_canonical_request(true, FrontendKind::OpenAiResponses);
    req.generation.reasoning = Some(ReasoningOptions {
        effort: Some(ReasoningEffort::Medium),
        budget_tokens: None,
    });

    // Provider produces a CanonicalStream from the upstream Gemini JSON array.
    let provider = make_gemini_provider(server.url());
    let canonical_stream = provider
        .complete(req, make_gemini_thinking_target())
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

    // Each upstream thought part surfaces as one response.reasoning.delta.
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

    // ── 3. Text lifecycle (output_index 1, item id msg_1) ────────────────
    assert!(
        text.contains("\"type\":\"message\""),
        "missing message item type\n{}",
        text
    );
    // Second content block (text, after reasoning) lands at output_index 1.
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

    // ── 4. response.completed event present (usage tokens pinned to its
    //       frame in section 6 — the global contains-check is strictly
    //       weaker so we drop it here).
    assert!(
        text.contains("event: response.completed"),
        "missing response.completed\n{}",
        text
    );

    // ── 5. Critical orderings: split per-event and verify positions ──────
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

    // ── 6. response.completed frame contains the usage tokens ────────────
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
