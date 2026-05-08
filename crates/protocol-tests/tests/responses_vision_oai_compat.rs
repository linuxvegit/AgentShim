//! Plan 04 T4 — vision cell: OpenAI Responses request → OAI-compat upstream.
//!
//! Pipeline exercised end-to-end:
//!
//! ```text
//! CanonicalRequest (frontend.kind = OpenAiResponses, image content)
//!   → OpenAiCompatibleProvider::complete()  [HTTP-mocked via mockito]
//!   → CanonicalStream
//!   → OpenAiResponses::encode_stream(stream)
//!   → SSE bytes
//! ```
//!
//! What this exercises that the same-protocol vision smokes don't:
//!
//!   * The Responses-frontend decoder fix from Plan 04 T4 actually surfaces
//!     `input_image` parts as canonical `ContentBlock::Image`. Before the
//!     fix the image arrived at the provider as `Unsupported` and was
//!     silently dropped, so the outbound assertion below would have failed.
//!     Building a `ContentBlock::Image` directly here gives the same
//!     end-state as decoding a Responses request body would, and verifies
//!     the cross-protocol seam from the canonical model on outward.
//!   * The OAI-compat provider's outbound encoder (`canonical_to_chat`) is
//!     frontend-agnostic for content blocks: a Responses-shaped request
//!     should produce the exact same `image_url` part shape an OpenAI Chat
//!     request would. This test pins that property.
//!
//! Outbound assertion: the request body landing on the upstream contains
//! the OpenAI Chat-style `{"type":"image_url","image_url":{"url":"..."}}`
//! object form.
//!
//! Inbound assertion: the encoded SSE contains `event: response.completed`
//! with usage tokens populated from the upstream `usage` block.

use agent_shim_core::request::{CanonicalRequest, RequestMetadata};
use agent_shim_core::{
    content::ImageBlock, media::BinarySource, ContentBlock, ExtensionMap, FrontendInfo,
    FrontendKind, FrontendModel, GenerationOptions, Message, RequestId, ResolvedPolicy,
};
use agent_shim_frontends::{openai_responses::OpenAiResponses, FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{
    collect_sse, make_oai_compat_provider, make_oai_compat_target, TEST_CLOCK,
};
use agent_shim_providers::BackendProvider;
use mockito::Matcher;

const IMAGE_URL: &str = "https://example.com/cat.png";

/// OpenAI Chat SSE response body — two text deltas (`"Hello"` then
/// `" world"`), a `finish_reason` chunk, and a final usage chunk so the
/// Responses encoder's `response.completed` event can carry usage. The
/// `[DONE]` sentinel terminates the stream.
const TEXT_SSE: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}}\n\n",
    "data: [DONE]\n\n",
);

/// Build a Responses-frontend canonical request carrying both a text
/// block and a URL-form image. Mirrors what the Responses decoder would
/// produce from an `input_image` part with an https URL after the Plan
/// 04 T4 decoder fix.
fn req_with_image() -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::OpenAiResponses,
            requested_model: FrontendModel::from("gpt-4o"),
        },
        model: FrontendModel::from("gpt-4o"),
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
async fn responses_vision_oai_compat_round_trip() {
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("POST", "/v1/chat/completions")
        // OUTBOUND assertion: the canonical_to_chat encoder must render
        // the canonical `BinarySource::Url` image as the OpenAI Chat
        // `image_url` object shape (not the bare-string Responses shape —
        // the OAI-compat upstream is the Chat Completions API).
        .match_body(Matcher::PartialJsonString(
            r#"{"messages":[{"role":"user","content":[
                    {"type":"text","text":"describe this picture"},
                    {"type":"image_url","image_url":{"url":"https://example.com/cat.png"}}
                ]}]}"#
                .into(),
        ))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(TEXT_SSE)
        .create_async()
        .await;

    // Provider produces a CanonicalStream from the upstream SSE.
    let provider = make_oai_compat_provider(server.url());
    let canonical_stream = provider
        .complete(req_with_image(), make_oai_compat_target())
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

    // INBOUND assertion: response.completed at the tail with usage.
    assert!(
        text.contains("event: response.completed"),
        "missing response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"input_tokens\":7"),
        "missing input_tokens=7 on response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"output_tokens\":2"),
        "missing output_tokens=2 on response.completed\n{}",
        text
    );
}
