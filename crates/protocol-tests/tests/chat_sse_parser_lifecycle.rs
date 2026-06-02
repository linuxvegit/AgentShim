//! PR-B2: Wire the canonical lifecycle validator into the real
//! `chat_sse_parser` (and the surrounding `oai_chat_wire` machinery) via the
//! `OpenAiCompatibleProvider`. Each test exercises one wire-shape scenario
//! and asserts both the test-specific behaviour AND
//! `assert_canonical_lifecycle(&events)`.
//!
//! See `crates/protocol-tests/src/lifecycle.rs` for the rules; see
//! `CONTEXT.md` "Canonical lifecycle" for the contract.
//!
//! Bugs surfaced here are fixed at the source (B2-clean) and the relevant
//! commit/PR notes what the validator caught.

use agent_shim_core::{
    BackendTarget, CanonicalRequest, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel,
    GenerationOptions, RequestId, StreamEvent,
};
use agent_shim_protocol_tests::lifecycle::assert_canonical_lifecycle;
use agent_shim_protocol_tests::sse_fixtures;
use agent_shim_providers::{openai_compatible::OpenAiCompatibleProvider, BackendProvider};
use futures::StreamExt;

fn make_req(stream: bool) -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::OpenAiChat,
            requested_model: FrontendModel::from("gpt-4o"),
        },
        model: FrontendModel::from("gpt-4o"),
        system: vec![],
        messages: vec![agent_shim_core::Message::user(vec![
            agent_shim_core::ContentBlock::text("hello"),
        ])],
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

fn make_target() -> BackendTarget {
    BackendTarget {
        provider: "openai".to_string(),
        model: "gpt-4o".to_string(),
        policy: Default::default(),
    }
}

async fn drive_stream(sse_body: &str) -> Vec<StreamEvent> {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(sse_body)
        .create_async()
        .await;

    let provider =
        OpenAiCompatibleProvider::new("openai", server.url(), "test-key", Default::default(), 30)
            .unwrap();

    let mut stream = provider
        .complete(make_req(true), make_target())
        .await
        .unwrap();

    let mut events: Vec<StreamEvent> = Vec::new();
    while let Some(ev) = stream.next().await {
        // Parser errors surface as `Err`; in the happy-path fixtures used by
        // this suite they would be a test failure, so unwrap loudly.
        events.push(ev.expect("parser emitted an error on a happy-path fixture"));
    }

    mock.assert_async().await;
    events
}

/// Two text deltas + `finish_reason=stop` + `[DONE]`. The "ordinary" case.
/// Validator-enforced: ResponseStart → MessageStart → ContentBlockStart →
/// TextDelta(s) → ContentBlockStop → MessageStop → ResponseStop, balanced.
#[tokio::test]
async fn two_text_deltas_lifecycle_is_clean() {
    let events = drive_stream(sse_fixtures::STREAM_TWO_TEXT_DELTAS_THEN_DONE).await;
    assert_canonical_lifecycle(&events);

    // Spot check the content is what we expect — defends against the
    // validator passing on an empty / wrong stream.
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello world");
}

/// `[DONE]` arrives before any content delta. Mirrors the corner case where
/// an upstream acknowledges the request with a `role: "assistant"` chunk
/// and then drops to `[DONE]` without sending any content or `finish_reason`.
/// The parser must still emit a complete, well-formed lifecycle envelope
/// (ResponseStart → MessageStart → MessageStop → ResponseStop).
#[tokio::test]
async fn done_before_any_delta_lifecycle_is_clean() {
    let events = drive_stream(sse_fixtures::STREAM_DONE_BEFORE_ANY_DELTA).await;
    assert_canonical_lifecycle(&events);

    // No content blocks should have been opened.
    let has_block_events = events.iter().any(|e| {
        matches!(
            e,
            StreamEvent::ContentBlockStart { .. } | StreamEvent::ContentBlockStop { .. }
        )
    });
    assert!(
        !has_block_events,
        "no content blocks should have been emitted for a content-less stream"
    );
}

/// Upstream sends one content delta and then drops the connection — no
/// `finish_reason` and no `[DONE]` arrive. The parser must still close the
/// text block, emit `MessageStop`, and emit `ResponseStop` so the gateway's
/// H7 hook and the usage logger see a complete lifecycle envelope.
#[tokio::test]
async fn content_then_silent_drop_lifecycle_is_clean() {
    let events = drive_stream(sse_fixtures::STREAM_CONTENT_THEN_DROP).await;
    assert_canonical_lifecycle(&events);

    // The single text delta we wrote must be present, proving the parser
    // didn't bail before producing content.
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello");
}

/// Single tool call across two delta chunks + `finish_reason=tool_calls` +
/// `[DONE]`. The tool block must open, accumulate the args fragment, then
/// close with `ToolCallStop` immediately preceding `ContentBlockStop` — the
/// rule-4 invariant the OpenAI Responses encoder downstream depends on.
#[tokio::test]
async fn single_tool_call_lifecycle_is_clean() {
    let events = drive_stream(sse_fixtures::STREAM_SINGLE_TOOL_CALL_THEN_DONE).await;
    assert_canonical_lifecycle(&events);

    // Tool call arguments accumulate to the full JSON object across deltas.
    let args: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallArgumentsDelta { json_fragment, .. } => {
                Some(json_fragment.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(args, "{\"city\":\"SF\"}");
}
