//! Plan 04 T4 — vision cell: OpenAI Responses request → GitHub Copilot upstream.
//!
//! Pipeline exercised end-to-end:
//!
//! ```text
//! CanonicalRequest (frontend.kind = OpenAiResponses, image content)
//!   → CopilotProvider::complete()           [HTTP-mocked via mockito]
//!     ├─ token exchange  (GET /copilot_internal/v2/token)
//!     └─ Responses API   (POST /v1/responses with image part)
//!   → CanonicalStream
//!   → OpenAiResponses::encode_stream(stream)
//!   → SSE bytes
//! ```
//!
//! Why the Responses API path (not Chat Completions):
//!
//! `CopilotProvider::complete` (crates/providers/src/github_copilot/mod.rs:188)
//! routes through the Responses API when `req.frontend.kind == OpenAiResponses`,
//! and through Chat Completions for every other kind. With the Responses
//! frontend feeding it, this test exercises the Copilot Responses-API
//! encoder path — which differs from the Chat path in two visible ways
//! on the wire:
//!
//!   * The image part shape is `{"type":"input_image","image_url":"<URL>"}`
//!     (a STRING `image_url`, not the `{url:"..."}` object Chat uses).
//!     Plan 04 T4 also extends the `responses_api::encode_request`
//!     encoder to render `ContentBlock::Image` as `input_image` (it
//!     previously emitted text-only message content via `extract_text`,
//!     which silently dropped images).
//!   * The upstream URL is `/v1/responses` (not `/chat/completions`).
//!
//! Outbound assertion: the Responses-API body contains the
//! `input_image` part with our fixture URL as a bare string.
//!
//! Inbound assertion: the encoded SSE contains `event: response.completed`
//! with usage tokens populated from the upstream `response.completed`
//! event's `usage` block.

use agent_shim_core::request::{CanonicalRequest, RequestMetadata};
use agent_shim_core::{
    content::ImageBlock, media::BinarySource, ContentBlock, ExtensionMap, FrontendInfo,
    FrontendKind, FrontendModel, GenerationOptions, Message, RequestId, ResolvedPolicy,
};
use agent_shim_frontends::{openai_responses::OpenAiResponses, FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{
    collect_sse, make_copilot_provider, make_copilot_target, TEST_CLOCK,
};
use agent_shim_providers::BackendProvider;
use mockito::Matcher;

const IMAGE_URL: &str = "https://example.com/cat.png";

/// Upstream Responses-API SSE body — minimal lifecycle (`response.created`
/// → `output_text.delta` → `response.completed`) with a usage block on
/// the terminal event so the encoder's `response.completed` carries
/// tokens.
const RESPONSES_SSE: &str = concat!(
    "event: response.created\n",
    "data: {\"id\":\"resp_copilot_1\",\"object\":\"response\",\"created_at\":1700000000,\"model\":\"gpt-4o\",\"status\":\"in_progress\"}\n\n",
    "event: response.output_item.added\n",
    "data: {\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_0\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"item_id\":\"msg_0\",\"output_index\":0,\"content_index\":0,\"delta\":\"A cat.\"}\n\n",
    "event: response.output_item.done\n",
    "data: {\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_0\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"A cat.\",\"annotations\":[]}]}}\n\n",
    "event: response.completed\n",
    "data: {\"id\":\"resp_copilot_1\",\"object\":\"response\",\"created_at\":1700000000,\"model\":\"gpt-4o\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":17,\"output_tokens\":4,\"total_tokens\":21}}\n\n",
);

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
async fn responses_vision_copilot_round_trip() {
    let mut server = mockito::Server::new_async().await;

    // Token exchange — returns an api_base pointing back at this same
    // mockito server so the subsequent /v1/responses POST also lands here.
    let token_response = format!(
        r#"{{
            "token": "tid_visiontest",
            "expires_at": 9999999999,
            "refresh_in": 1500,
            "endpoints": {{ "api": "{}" }}
        }}"#,
        server.url()
    );
    let token_mock = server
        .mock("GET", "/copilot_internal/v2/token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(token_response)
        .create_async()
        .await;

    // Responses-API call — outbound assertion: the request body contains
    // the Copilot Responses-API `input_image` part shape, where
    // `image_url` is a STRING (not the `{url:"..."}` object the Chat
    // Completions API uses). The text and image are siblings in the
    // `content` parts array of the user `message` input item.
    let responses_mock = server
        .mock("POST", "/v1/responses")
        .match_header(
            "authorization",
            Matcher::Regex("Bearer tid_visiontest".into()),
        )
        .match_body(Matcher::PartialJsonString(
            r#"{"input":[{"type":"message","role":"user","content":[
                    {"type":"input_text","text":"describe this picture"},
                    {"type":"input_image","image_url":"https://example.com/cat.png"}
                ]}]}"#
                .into(),
        ))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(RESPONSES_SSE)
        .create_async()
        .await;

    let provider = make_copilot_provider(server.url());
    let canonical_stream = provider
        .complete(req_with_image(), make_copilot_target())
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

    token_mock.assert_async().await;
    responses_mock.assert_async().await;

    // INBOUND assertion: response.completed at the tail with usage
    // pulled from the upstream `response.completed` event.
    assert!(
        text.contains("event: response.completed"),
        "missing response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"input_tokens\":17"),
        "missing input_tokens=17 on response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"output_tokens\":4"),
        "missing output_tokens=4 on response.completed\n{}",
        text
    );
}
