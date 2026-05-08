//! Cross-protocol smoke test: a Gemini unary response that carries
//! `safetyRatings` on `candidates[0]` flows through the canonical pipeline
//! into an OpenAI Responses encoded SSE without crashing or silently
//! dropping the rest of the payload.
//!
//! Pipeline exercised end-to-end:
//!
//! ```text
//! CanonicalRequest (frontend.kind = OpenAiResponses, stream = false)
//!   → GeminiProvider::complete()           [HTTP-mocked via mockito]
//!   → unary_bytes_to_canonical_stream      [parse_unary + synth events]
//!   → CanonicalStream
//!   → OpenAiResponses::encode_stream(stream)
//!   → SSE bytes
//! ```
//!
//! ## Where safety ratings land today (D7 / ADR-0002)
//!
//! In v0.3, the Gemini parser attaches `safetyRatings` to the FIRST
//! canonical content block under `extensions["gemini.safety_ratings"]`
//! (`crates/providers/src/gemini/response.rs::attach_extensions`, lines
//! 541-557). That extension is verified end-to-end at the parser layer by
//! `unary_safety_ratings_attach_to_first_block_extensions` in the same
//! file's test module — so the **canonical write path** is already covered.
//!
//! ## What this test pins down
//!
//! Two pieces the parser-level unit test cannot prove on its own:
//!
//! 1. **The pipeline doesn't crash.** A real `GenerateContentResponse` with
//!    `safetyRatings` flows from mockito bytes, through the unary→stream
//!    adapter (`canonical_response_to_events` in
//!    `crates/providers/src/gemini/mod.rs:240-323`), and out of the
//!    Responses encoder without panic, decode error, or stream error event.
//! 2. **The rest of the data isn't silently dropped.** The text content
//!    (`"hello"`) and the `usageMetadata` (3/1/4 tokens) still surface on
//!    the encoded SSE — `output_text.delta` carries `"hello"`,
//!    `response.completed` carries `input_tokens=3`/`output_tokens=1`. If a
//!    future change accidentally swallowed `safetyRatings` *along with the
//!    surrounding payload*, this test would fail loudly.
//!
//! ## Plan 04 / ADR-0003 forward reference
//!
//! Today, the unary→stream synthesizer at
//! `crates/providers/src/gemini/mod.rs::canonical_response_to_events` drops
//! per-block extensions when generating events (it only forwards
//! `text`/`reasoning`/`tool` data — see lines 252-301). The Responses
//! encoder also doesn't surface canonical extensions on SSE today
//! (`grep -n extensions crates/frontends/src/openai_responses/encode_stream.rs`
//! has no hits in any payload-emit path). Plan 04 will promote safety
//! ratings to a typed `Usage.safety_ratings` field that DOES travel via
//! `StreamEvent::ResponseStop { usage }`, after which this test will gain a
//! direct assertion on the encoded `response.completed` frame.

use agent_shim_core::FrontendKind;
use agent_shim_frontends::{openai_responses::OpenAiResponses, FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{
    collect_sse, make_canonical_request, make_gemini_provider, make_gemini_target, TEST_CLOCK,
};
use agent_shim_providers::BackendProvider;

/// Gemini `:generateContent` (unary) response body. The `candidates[0]`
/// payload carries:
/// - one `text` part (`"hello"`),
/// - `finishReason: "STOP"`,
/// - a non-empty `safetyRatings` array (`HARASSMENT: NEGLIGIBLE`,
///   `DANGEROUS: LOW`),
/// - `usageMetadata` with 3/1/4 tokens.
///
/// The `safetyRatings` field is what the parser writes into
/// `extensions["gemini.safety_ratings"]` on the first canonical block
/// (`crates/providers/src/gemini/response.rs:541-557`).
const SAFETY_JSON: &str = r#"{
  "candidates": [
    {
      "content": {
        "role": "model",
        "parts": [{"text": "hello"}]
      },
      "finishReason": "STOP",
      "safetyRatings": [
        {"category": "HARM_CATEGORY_HARASSMENT", "probability": "NEGLIGIBLE"},
        {"category": "HARM_CATEGORY_DANGEROUS", "probability": "LOW"}
      ]
    }
  ],
  "usageMetadata": {
    "promptTokenCount": 3,
    "candidatesTokenCount": 1,
    "totalTokenCount": 4
  }
}"#;

#[tokio::test]
async fn responses_to_gemini_safety_ratings_flows_through_pipeline() {
    let mut server = mockito::Server::new_async().await;

    // Unary endpoint (`:generateContent`, NOT `:streamGenerateContent`) —
    // the request below pins `stream = false`, which routes
    // `GeminiProvider::complete` through the unary URL and the
    // `unary_bytes_to_canonical_stream` adapter. The parser's
    // `attach_extensions` only fires on the unary code path, so this is the
    // shape that exercises the safetyRatings → canonical extension write.
    let mock = server
        .mock(
            "POST",
            mockito::Matcher::Regex(
                r"/models/gemini-2\.0-flash:generateContent.*key=test-key".into(),
            ),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SAFETY_JSON)
        .create_async()
        .await;

    // Build a Responses-flavored request with `stream = false` so the
    // provider takes the unary path.
    let request = make_canonical_request(false, FrontendKind::OpenAiResponses);

    // Provider produces a CanonicalStream from the unary Gemini body.
    let provider = make_gemini_provider(server.url());
    let canonical_stream = provider
        .complete(request, make_gemini_target())
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

    // The mock URL pattern targets `:generateContent` — if `stream = false`
    // weren't honored end-to-end, this assertion would fail because the
    // provider would have hit `:streamGenerateContent` instead.
    mock.assert_async().await;

    // ── 1. The pipeline didn't error ─────────────────────────────────────
    // The Responses encoder emits an `event: error` line on `StreamError`
    // results. A `safetyRatings`-bearing payload that crashed the parser or
    // got rejected somewhere upstream would surface here.
    assert!(
        !text.contains("event: error"),
        "encoder emitted an error event — pipeline crashed on safetyRatings\n{}",
        text
    );

    // ── 2. The lifecycle is well-formed (created → completed) ────────────
    assert!(
        text.contains("event: response.created"),
        "missing response.created — pipeline did not start cleanly\n{}",
        text
    );
    assert!(
        text.contains("event: response.completed"),
        "missing response.completed — pipeline did not finish cleanly\n{}",
        text
    );

    // ── 3. The text payload survived alongside safety ratings ────────────
    // The `safetyRatings` array sits on the same candidate as the `"hello"`
    // text part. If the parser silently dropped the candidate while trying
    // to handle ratings, the text would be missing.
    assert!(
        text.contains("\"delta\":\"hello\""),
        "missing 'hello' text delta — text content silently dropped alongside safetyRatings\n{}",
        text
    );
    assert!(
        text.contains("event: response.output_text.done"),
        "missing output_text.done — text lifecycle silently dropped\n{}",
        text
    );

    // ── 4. The usage payload survived alongside safety ratings ───────────
    // `usageMetadata` is on the same response object as `safetyRatings`.
    // If the parser bailed early on ratings, usage would not reach
    // `response.completed`.
    let events: Vec<&str> = text.split("event: ").skip(1).collect();
    let completed_frame = events
        .iter()
        .find(|e| e.starts_with("response.completed"))
        .expect("response.completed frame present");
    assert!(
        completed_frame.contains("\"input_tokens\":3"),
        "completed frame missing input_tokens=3 — usage silently dropped alongside safetyRatings\n{}",
        completed_frame
    );
    assert!(
        completed_frame.contains("\"output_tokens\":1"),
        "completed frame missing output_tokens=1 — usage silently dropped alongside safetyRatings\n{}",
        completed_frame
    );
}
