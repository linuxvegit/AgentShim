//! Plan 04 T4 — vision cell: OpenAI Responses request → Gemini upstream.
//!
//! Pipeline exercised end-to-end:
//!
//! ```text
//! CanonicalRequest (frontend.kind = OpenAiResponses, image content)
//!   → GeminiProvider::complete()            [HTTP-mocked via mockito]
//!   → CanonicalStream
//!   → OpenAiResponses::encode_stream(stream)
//!   → SSE bytes
//! ```
//!
//! What this exercises:
//!
//!   * The Plan 04 T4 Responses-decoder fix surfaces `input_image` parts
//!     as canonical `ContentBlock::Image`, which the Gemini provider's
//!     `binary_to_part` then renders as Gemini's native `fileData` part
//!     (URL-form) — distinct from the OpenAI `image_url` and Anthropic
//!     `image.source` shapes.
//!   * Gemini's streaming path uses `:streamGenerateContent` which
//!     returns a JSON-array body, not SSE — the parser splits the array
//!     into per-element `CanonicalStream` events. Usage lands on the
//!     last element's `usageMetadata`.
//!
//! Outbound assertion: the body contains `{"fileData":{"fileUri":"..."}}`.
//!
//! Inbound assertion: the encoded SSE contains `event: response.completed`
//! with usage tokens from the upstream `usageMetadata`.

use agent_shim_core::request::{CanonicalRequest, RequestMetadata};
use agent_shim_core::{
    content::ImageBlock, media::BinarySource, ContentBlock, ExtensionMap, FrontendInfo,
    FrontendKind, FrontendModel, GenerationOptions, Message, RequestId, ResolvedPolicy,
};
use agent_shim_frontends::{openai_responses::OpenAiResponses, FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{
    collect_sse, make_gemini_provider, make_gemini_target, TEST_CLOCK,
};
use agent_shim_providers::BackendProvider;
use mockito::Matcher;

const IMAGE_URL: &str = "https://example.com/cat.png";

/// Gemini `:streamGenerateContent` JSON-array body — single element with
/// `usageMetadata` so the Responses encoder's `response.completed` event
/// can carry usage.
const GEMINI_JSON: &str = r#"[
{"candidates":[{"content":{"role":"model","parts":[{"text":"A cat."}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":13,"candidatesTokenCount":3,"totalTokenCount":16}}
]"#;

fn req_with_image() -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::OpenAiResponses,
            requested_model: FrontendModel::from("gemini-2.0-flash"),
        },
        model: FrontendModel::from("gemini-2.0-flash"),
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
async fn responses_vision_gemini_round_trip() {
    let mut server = mockito::Server::new_async().await;

    // Gemini auth is a `?key=<api_key>` query string, not a header — match
    // on the streaming URL pattern (regex) plus the api key in the query.
    let mock = server
        .mock(
            "POST",
            Matcher::Regex(r"/models/gemini-2\.0-flash:streamGenerateContent.*key=test-key".into()),
        )
        // OUTBOUND assertion: Gemini's `binary_to_part` renders
        // `BinarySource::Url` as `{"fileData":{"fileUri":"..."}}` (URL-form
        // image — Gemini's `inlineData` is base64-only). Inserted as one of
        // the `parts` in the `contents` array, alongside the `text` part.
        .match_body(Matcher::PartialJsonString(
            r#"{"contents":[{"role":"user","parts":[
                    {"text":"describe this picture"},
                    {"fileData":{"fileUri":"https://example.com/cat.png"}}
                ]}]}"#
                .into(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(GEMINI_JSON)
        .create_async()
        .await;

    let provider = make_gemini_provider(server.url());
    let canonical_stream = provider
        .complete(req_with_image(), make_gemini_target())
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

    // INBOUND assertion: response.completed at the tail with usage from
    // the upstream `usageMetadata` (promptTokenCount / candidatesTokenCount).
    assert!(
        text.contains("event: response.completed"),
        "missing response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"input_tokens\":13"),
        "missing input_tokens=13 on response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"output_tokens\":3"),
        "missing output_tokens=3 on response.completed\n{}",
        text
    );
}
