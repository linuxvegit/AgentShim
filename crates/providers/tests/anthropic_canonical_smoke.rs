//! Integration tests for the Anthropic provider's canonical path
//! (`complete()` with non-`AnthropicMessages` frontends).
//!
//! These tests run against a mockito-served fake `api.anthropic.com` and
//! verify that:
//!
//! * A `CanonicalRequest` synthesised with `FrontendKind::OpenAiChat` (i.e.
//!   not coming from the Anthropic frontend decoder) is encoded into the
//!   Anthropic Messages JSON body shape, sent to `/v1/messages` with the
//!   correct headers, and the model field is rewritten to the route's
//!   upstream model.
//! * The upstream's SSE / unary response is parsed back into the expected
//!   sequence of canonical [`StreamEvent`]s for text-only and tool-use cases.
//! * The same canonical path applies for `FrontendKind::OpenAiResponses` — the
//!   provider does not gate on the specific non-Anthropic frontend kind.
//!
//! This is the cross-protocol counterpart to T7's passthrough fixtures: the
//! frontend-side encoding (canonical → OpenAI Chat SSE) is already covered by
//! the OpenAI Chat frontend's own tests and `protocol-tests`. Verifying the
//! provider's canonical path here is sufficient to demonstrate the full
//! cross-protocol round-trip works end-to-end.

use agent_shim_core::{
    BackendTarget, CanonicalRequest, ContentBlock, ContentBlockKind, ExtensionMap, FrontendInfo,
    FrontendKind, FrontendModel, GenerationOptions, Message, RequestId, StopReason, StreamEvent,
};
use agent_shim_providers::{anthropic::AnthropicProvider, BackendProvider};
use futures::StreamExt;

/// Anthropic SSE response body for a "Hello world" text completion. Mirrors
/// the shape `parse_stream` is expected to consume.
const TEXT_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_text_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-4-7\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":2}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Anthropic SSE response body for a tool-use completion (`get_weather`).
/// The arguments are streamed in two `input_json_delta` chunks to verify
/// fragment concatenation.
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

/// Anthropic unary JSON response body for a "Hi there!" text completion.
const TEXT_UNARY: &str = r#"{
    "id": "msg_unary_1",
    "type": "message",
    "role": "assistant",
    "model": "claude-opus-4-7",
    "content": [
        {"type":"text","text":"Hi there!"}
    ],
    "stop_reason": "end_turn",
    "stop_sequence": null,
    "usage": {"input_tokens": 4, "output_tokens": 3}
}"#;

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

/// Build a synthetic `CanonicalRequest` shaped as if it came from a
/// non-Anthropic frontend. The provider's canonical path (`complete()`) must
/// accept this regardless of `frontend.kind`.
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

async fn drain(mut stream: agent_shim_core::CanonicalStream) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(e) => events.push(e),
            Err(e) => panic!("canonical stream yielded error: {e:?}"),
        }
    }
    events
}

#[tokio::test]
async fn canonical_path_text_stream_yields_correct_events() {
    let mut server = mockito::Server::new_async().await;

    // Expect: POST /v1/messages with Anthropic auth/version headers and a JSON
    // body where `model` has been rewritten to the route's upstream model and
    // `stream=true` is set. The exact full body shape is covered by
    // `request::build` unit tests; here we only assert the cross-cutting
    // wire-shape invariants.
    let mock = server
        .mock("POST", "/v1/messages")
        .match_header("x-api-key", "test-key")
        .match_header("anthropic-version", "2023-06-01")
        .match_header("content-type", "application/json")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::PartialJsonString(r#"{"model":"claude-opus-4-7"}"#.to_string()),
            mockito::Matcher::PartialJsonString(r#"{"stream":true}"#.to_string()),
        ]))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(TEXT_SSE)
        .create_async()
        .await;

    let provider = make_provider(server.url());
    let stream = provider
        .complete(make_req(true, FrontendKind::OpenAiChat), make_target())
        .await
        .unwrap();

    let events = drain(stream).await;

    // ── Assert the canonical event sequence is well-formed ───────────────
    assert!(
        matches!(events.first(), Some(StreamEvent::ResponseStart { .. })),
        "first event must be ResponseStart, got {:?}",
        events.first()
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::MessageStart { .. })),
        "expected MessageStart in stream"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            StreamEvent::ContentBlockStart {
                kind: ContentBlockKind::Text,
                ..
            }
        )),
        "expected text ContentBlockStart"
    );

    // Text deltas must arrive in order, matching the upstream payload.
    let texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["Hello", " world"]);

    // MessageStop must carry the mapped stop_reason from `message_delta`.
    let stop = events
        .iter()
        .find(|e| matches!(e, StreamEvent::MessageStop { .. }))
        .expect("expected MessageStop");
    match stop {
        StreamEvent::MessageStop { stop_reason, .. } => {
            assert_eq!(*stop_reason, StopReason::EndTurn);
        }
        _ => unreachable!(),
    }

    // ResponseStop closes the stream and carries accumulated usage.
    match events.last() {
        Some(StreamEvent::ResponseStop { usage }) => {
            let u = usage.as_ref().expect("expected usage on ResponseStop");
            assert_eq!(u.input_tokens, Some(5));
            assert_eq!(u.output_tokens, Some(2));
        }
        other => panic!("last event must be ResponseStop, got {other:?}"),
    }

    mock.assert_async().await;
}

#[tokio::test]
async fn canonical_path_tool_call_stream_yields_tool_events() {
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("POST", "/v1/messages")
        .match_header("x-api-key", "test-key")
        .match_header("anthropic-version", "2023-06-01")
        .match_body(mockito::Matcher::PartialJsonString(
            r#"{"model":"claude-opus-4-7"}"#.to_string(),
        ))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(TOOL_SSE)
        .create_async()
        .await;

    let provider = make_provider(server.url());
    let stream = provider
        .complete(make_req(true, FrontendKind::OpenAiChat), make_target())
        .await
        .unwrap();

    let events = drain(stream).await;

    // ── Tool-use opens a ToolCall content block, not a Text block ─────────
    assert!(
        events.iter().any(|e| matches!(
            e,
            StreamEvent::ContentBlockStart {
                kind: ContentBlockKind::ToolCall,
                ..
            }
        )),
        "expected tool_call ContentBlockStart"
    );

    // ToolCallStart carries the upstream id + name verbatim.
    let tool_start = events
        .iter()
        .find(|e| matches!(e, StreamEvent::ToolCallStart { .. }))
        .expect("expected ToolCallStart");
    match tool_start {
        StreamEvent::ToolCallStart { id, name, .. } => {
            assert_eq!(id.0, "toolu_1");
            assert_eq!(name, "get_weather");
        }
        _ => unreachable!(),
    }

    // ── Argument fragments stream through and concatenate to valid JSON ───
    let fragments: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallArgumentsDelta { json_fragment, .. } => {
                Some(json_fragment.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(fragments, vec![r#"{"city":"#, r#""Paris"}"#]);
    let parsed: serde_json::Value = serde_json::from_str(&fragments.join(""))
        .expect("concatenated fragments must form valid JSON");
    assert_eq!(parsed["city"], "Paris");

    // MessageStop must reflect the tool-use stop reason.
    let stop = events
        .iter()
        .find(|e| matches!(e, StreamEvent::MessageStop { .. }))
        .expect("expected MessageStop");
    if let StreamEvent::MessageStop { stop_reason, .. } = stop {
        assert_eq!(*stop_reason, StopReason::ToolUse);
    }

    mock.assert_async().await;
}

#[tokio::test]
async fn canonical_path_unary_response_yields_correct_events() {
    let mut server = mockito::Server::new_async().await;

    // Non-streaming request: `stream=false` in the outbound body, upstream
    // returns Anthropic Messages JSON which `parse_unary` synthesises into a
    // canonical event sequence.
    let mock = server
        .mock("POST", "/v1/messages")
        .match_header("x-api-key", "test-key")
        .match_header("anthropic-version", "2023-06-01")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::PartialJsonString(r#"{"model":"claude-opus-4-7"}"#.to_string()),
            mockito::Matcher::PartialJsonString(r#"{"stream":false}"#.to_string()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(TEXT_UNARY)
        .create_async()
        .await;

    let provider = make_provider(server.url());
    let stream = provider
        .complete(make_req(false, FrontendKind::OpenAiChat), make_target())
        .await
        .unwrap();

    let events = drain(stream).await;

    // ResponseStart carries the upstream message id.
    match events.first() {
        Some(StreamEvent::ResponseStart { id, .. }) => {
            assert_eq!(id.0, "msg_unary_1");
        }
        other => panic!("first event must be ResponseStart, got {other:?}"),
    }

    let texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["Hi there!"]);

    let stop = events
        .iter()
        .find(|e| matches!(e, StreamEvent::MessageStop { .. }))
        .expect("expected MessageStop");
    if let StreamEvent::MessageStop { stop_reason, .. } = stop {
        assert_eq!(*stop_reason, StopReason::EndTurn);
    }

    match events.last() {
        Some(StreamEvent::ResponseStop { usage }) => {
            let u = usage.as_ref().expect("expected usage on ResponseStop");
            assert_eq!(u.input_tokens, Some(4));
            assert_eq!(u.output_tokens, Some(3));
        }
        other => panic!("last event must be ResponseStop, got {other:?}"),
    }

    mock.assert_async().await;
}

#[tokio::test]
async fn canonical_path_does_not_gate_on_frontend_kind() {
    // Smoke: the canonical path must not gate on `OpenAiChat` specifically;
    // any non-Anthropic frontend (here `OpenAiResponses`) routes through
    // `complete()` and produces canonical events. This guards against a
    // future regression where `complete()` accidentally inherits the
    // passthrough's frontend-kind check.
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("POST", "/v1/messages")
        .match_header("x-api-key", "test-key")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(TEXT_SSE)
        .create_async()
        .await;

    let provider = make_provider(server.url());
    let stream = provider
        .complete(make_req(true, FrontendKind::OpenAiResponses), make_target())
        .await
        .unwrap();

    let events = drain(stream).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { text, .. } if text == "Hello")),
        "OpenAiResponses frontend must route through canonical path and yield TextDelta"
    );
    mock.assert_async().await;
}
