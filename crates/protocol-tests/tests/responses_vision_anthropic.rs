//! Plan 04 T4 — vision cell: OpenAI Responses request → Anthropic upstream.
//!
//! Pipeline exercised end-to-end:
//!
//! ```text
//! CanonicalRequest (frontend.kind = OpenAiResponses, image content)
//!   → AnthropicProvider::complete()         [HTTP-mocked via mockito]
//!   → CanonicalStream
//!   → OpenAiResponses::encode_stream(stream)
//!   → SSE bytes
//! ```
//!
//! What this exercises:
//!
//!   * The Plan 04 T4 Responses-decoder fix surfaces `input_image` parts
//!     as canonical `ContentBlock::Image`. Building the canonical image
//!     directly here gives the same end-state and verifies the Anthropic
//!     provider's outbound encoder lands the right wire shape.
//!   * The Anthropic provider's `image_block_to_outgoing` renders
//!     `BinarySource::Url` as `{"type":"image","source":{"type":"url",...}}`
//!     (Anthropic's URL-form image, distinct from the OpenAI `image_url`
//!     shape and the Gemini `fileData` shape).
//!
//! Outbound assertion: the request body landing on the Anthropic
//! `/v1/messages` endpoint contains the Anthropic-native image source.
//!
//! Inbound assertion: the encoded SSE contains `event: response.completed`
//! with usage tokens populated from the upstream `message_delta` /
//! `message_start` usage fields.

use agent_shim_core::request::{CanonicalRequest, RequestMetadata};
use agent_shim_core::{
    content::ImageBlock, media::BinarySource, ContentBlock, ExtensionMap, FrontendInfo,
    FrontendKind, FrontendModel, GenerationOptions, Message, RequestId, ResolvedPolicy,
};
use agent_shim_frontends::{openai_responses::OpenAiResponses, FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{
    collect_sse, make_anthropic_provider, make_anthropic_target, TEST_CLOCK,
};
use agent_shim_providers::BackendProvider;
use mockito::Matcher;

const IMAGE_URL: &str = "https://example.com/cat.png";

/// Anthropic Messages SSE response body — emits a single `"A cat."` text
/// delta and closes with `message_delta` carrying `output_tokens` and
/// `message_start`'s `input_tokens`. The Responses encoder must surface
/// both on `response.completed`.
const ANTHROPIC_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_vision_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-4-7\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":11,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"A cat.\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":3}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

fn req_with_image() -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::OpenAiResponses,
            requested_model: FrontendModel::from("claude-opus-4-7"),
        },
        model: FrontendModel::from("claude-opus-4-7"),
        system: vec![],
        messages: vec![Message::user(vec![
            ContentBlock::text("describe this picture"),
            ContentBlock::Image(ImageBlock {
                source: BinarySource::Url {
                    url: IMAGE_URL.into(),
                },
                extensions: ExtensionMap::new(),
            }),
        ])],
        tools: vec![],
        tool_choice: Default::default(),
        generation: GenerationOptions::default(),
        response_format: None,
        stream: true,
        metadata: RequestMetadata::default(),
        inbound_anthropic_headers: vec![],
        resolved_policy: ResolvedPolicy::default(),
        extensions: ExtensionMap::new(),
    }
}

#[tokio::test]
async fn responses_vision_anthropic_round_trip() {
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("POST", "/v1/messages")
        .match_header("x-api-key", "test-key")
        .match_header("anthropic-version", "2023-06-01")
        // OUTBOUND assertion: the Anthropic provider's
        // `image_block_to_outgoing` renders `BinarySource::Url` as the
        // Anthropic-native `{type:"image", source:{type:"url", url:...}}`.
        // This is intentionally NOT the OpenAI shape — a regression that
        // accidentally re-used the OpenAI encoder here would surface as
        // an unmatched mockito request.
        .match_body(Matcher::PartialJsonString(
            r#"{"messages":[{"role":"user","content":[
                    {"type":"text","text":"describe this picture"},
                    {"type":"image","source":{"type":"url","url":"https://example.com/cat.png"}}
                ]}]}"#
                .into(),
        ))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(ANTHROPIC_SSE)
        .create_async()
        .await;

    let provider = make_anthropic_provider(server.url());
    let canonical_stream = provider
        .complete(req_with_image(), make_anthropic_target())
        .await
        .expect("provider returned a canonical stream");

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

    // INBOUND assertion: response.completed with usage from the upstream
    // message_start.input_tokens + message_delta.output_tokens.
    assert!(
        text.contains("event: response.completed"),
        "missing response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"input_tokens\":11"),
        "missing input_tokens=11 on response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"output_tokens\":3"),
        "missing output_tokens=3 on response.completed\n{}",
        text
    );
}
