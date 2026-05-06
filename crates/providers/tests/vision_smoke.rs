//! Plan 04 T7 + T8: vision Tier-1 cell tests across the (frontend × provider)
//! matrix.
//!
//! Each cell drives a `CanonicalRequest` carrying a `ContentBlock::Image`
//! through the provider's `complete()` method against a mockito-served
//! fake upstream. The mockito mock asserts on the OUTBOUND BODY (via
//! `Matcher::PartialJsonString`) so we know the provider's encoder
//! actually emitted the image part the upstream expects — not just that
//! the request didn't fail.
//!
//! Cells covered (T7 + T8 combined):
//!   T7  Anthropic frontend → OAI-compat       (cross-protocol)
//!   T7  Anthropic frontend → Anthropic native (canonical encoder path)
//!   T7  Anthropic frontend → Gemini           (cross-protocol)
//!   T8  OpenAI Chat frontend → OAI-compat     (same-protocol)
//!   T8  OpenAI Chat frontend → Gemini         (cross-protocol)
//!
//! Skipped:
//!   * Anthropic-passthrough cell — passthrough is byte-identity, no
//!     encoder to exercise. Covered by `anthropic_passthrough_smoke.rs`.
//!   * Copilot cells — Copilot routes through canonical_to_chat the same
//!     way OAI-compat does, so the encoder is already exercised by the
//!     OAI-compat cells; Copilot's distinguishing surface (token manager,
//!     headers) is covered separately in `copilot_headers.rs`.
//!
//! The fixture image is a `BinarySource::Url` pointing at a stable
//! example URL. Every cell uses the same image so any divergence in
//! how it lands on the wire is encoder-specific, not fixture-specific.

use agent_shim_core::{
    content::ImageBlock, media::BinarySource, BackendTarget, CanonicalRequest, ContentBlock,
    ExtensionMap, FrontendInfo, FrontendKind, FrontendModel, GenerationOptions, Message, RequestId,
};
use agent_shim_providers::{
    anthropic::AnthropicProvider, gemini::GeminiProvider,
    openai_compatible::OpenAiCompatibleProvider, BackendProvider,
};
use futures::StreamExt;
use mockito::Matcher;

const IMAGE_URL: &str = "https://example.com/cat.png";

// ── Common helpers ────────────────────────────────────────────────────

fn req_with_image(stream: bool, frontend: FrontendKind, model: &str) -> CanonicalRequest {
    use agent_shim_core::request::RequestMetadata;
    use agent_shim_core::ResolvedPolicy;
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: frontend,
            requested_model: FrontendModel::from(model),
        },
        model: FrontendModel::from(model),
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
        stream,
        metadata: RequestMetadata::default(),
        inbound_anthropic_headers: vec![],
        resolved_policy: ResolvedPolicy::default(),
        extensions: ExtensionMap::new(),
    }
}

fn target(provider: &str, model: &str) -> BackendTarget {
    BackendTarget {
        provider: provider.into(),
        model: model.into(),
        policy: Default::default(),
    }
}

/// Minimal OpenAI Chat unary response body. Used by every OAI-compat /
/// Copilot cell so the assertion focuses on the OUTBOUND encode, not the
/// inbound parse (which is exercised by `openai_compatible_smoke.rs`).
const OAI_TEXT_RESPONSE: &str = r#"{
    "id": "chatcmpl-vision-1",
    "object": "chat.completion",
    "created": 1700000000,
    "model": "gpt-4o",
    "choices": [{
        "index": 0,
        "message": {"role": "assistant", "content": "A cat."},
        "finish_reason": "stop"
    }],
    "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}
}"#;

/// Minimal Anthropic Messages unary response body.
const ANTHROPIC_TEXT_RESPONSE: &str = r#"{
    "id": "msg_vision_1",
    "type": "message",
    "role": "assistant",
    "model": "claude-opus-4-7",
    "content": [{"type":"text","text":"A cat."}],
    "stop_reason": "end_turn",
    "stop_sequence": null,
    "usage": {"input_tokens": 10, "output_tokens": 2}
}"#;

/// Minimal Gemini unary response body. Gemini-specific (uses `candidates`
/// + `usageMetadata`).
const GEMINI_TEXT_RESPONSE: &str = r#"{
    "candidates": [{
        "content": {"role":"model","parts":[{"text":"A cat."}]},
        "finishReason": "STOP"
    }],
    "usageMetadata": {"promptTokenCount":10,"candidatesTokenCount":2,"totalTokenCount":12}
}"#;

// ── T7: Anthropic frontend → OAI-compat (cross-protocol) ─────────────

/// Plan 04 T7 cell `vision_anthropic_to_openai_compat`.
///
/// An Anthropic-frontend request carrying a URL image is sent to an
/// OpenAI-compatible upstream. The encoder must convert the canonical
/// `ContentBlock::Image{Url}` into an OpenAI `image_url` part — anything
/// else (e.g. dropping the image, wrapping it in a Custom block) means
/// the cross-protocol seam regressed. The mockito match_body verifies
/// the emitted body contains an `image_url` part with our fixture URL.
#[tokio::test]
async fn vision_anthropic_to_openai_compat() {
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJsonString(
            r#"{"messages":[{"role":"user","content":[
                    {"type":"text","text":"describe this picture"},
                    {"type":"image_url","image_url":{"url":"https://example.com/cat.png"}}
                ]}]}"#
                .into(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(OAI_TEXT_RESPONSE)
        .create();

    let provider =
        OpenAiCompatibleProvider::new("openai", server.url(), "test-key", Default::default(), 30)
            .expect("provider build");

    let stream = provider
        .complete(
            req_with_image(false, FrontendKind::AnthropicMessages, "gpt-4o"),
            target("openai", "gpt-4o"),
        )
        .await
        .expect("complete ok");
    let _events: Vec<_> = stream.collect().await;
    // mockito assertion: body matched the expected image_url payload, or
    // the test would have failed with "unmatched request" before now.
}

// ── T7: Anthropic frontend → Anthropic native ────────────────────────

/// Plan 04 T7 cell `vision_anthropic_to_anthropic` (canonical path —
/// the passthrough cell is skipped per the file-level docs).
///
/// An Anthropic-style inbound is encoded by the AnthropicProvider's
/// outbound canonical encoder. The wire shape is the Anthropic-native
/// `{type:"image", source:{type:"url", url:"..."}}`.
#[tokio::test]
async fn vision_anthropic_to_anthropic_canonical() {
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock("POST", "/v1/messages")
        .match_body(Matcher::PartialJsonString(
            r#"{"messages":[{"role":"user","content":[
                    {"type":"text","text":"describe this picture"},
                    {"type":"image","source":{"type":"url","url":"https://example.com/cat.png"}}
                ]}]}"#
                .into(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(ANTHROPIC_TEXT_RESPONSE)
        .create();

    let provider = AnthropicProvider::new(
        "anthropic",
        server.url(),
        "test-key",
        "2023-06-01",
        Default::default(),
        30,
    )
    .expect("provider build");

    let stream = provider
        .complete(
            req_with_image(false, FrontendKind::OpenAiChat, "claude-opus-4-7"),
            target("anthropic", "claude-opus-4-7"),
        )
        .await
        .expect("complete ok");
    let _events: Vec<_> = stream.collect().await;
}

// ── T7: Anthropic frontend → Gemini (cross-protocol) ─────────────────

/// Plan 04 T7 cell `vision_anthropic_to_gemini`.
///
/// Anthropic-frontend image → Gemini's `fileData` part shape (URL form).
/// Gemini's wire renames media-type-bearing fields to camelCase and uses
/// `parts` instead of `content` arrays; `Matcher::PartialJsonString`
/// surfaces a mismatch loudly because the test JSON uses Gemini's exact
/// shape.
#[tokio::test]
async fn vision_anthropic_to_gemini() {
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock(
            "POST",
            Matcher::Regex(r"/models/gemini-2\.0-flash:generateContent.*key=test-key".into()),
        )
        .match_body(Matcher::PartialJsonString(
            r#"{"contents":[{"role":"user","parts":[
                    {"text":"describe this picture"},
                    {"fileData":{"fileUri":"https://example.com/cat.png"}}
                ]}]}"#
                .into(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(GEMINI_TEXT_RESPONSE)
        .create();

    let provider = GeminiProvider::new("gemini", server.url(), "test-key", Default::default(), 30)
        .expect("provider build");

    let stream = provider
        .complete(
            req_with_image(false, FrontendKind::AnthropicMessages, "gemini-2.0-flash"),
            target("gemini", "gemini-2.0-flash"),
        )
        .await
        .expect("complete ok");
    let _events: Vec<_> = stream.collect().await;
}

// ── T8: OpenAI Chat frontend → OAI-compat (same-protocol) ────────────

/// Plan 04 T8 cell `vision_openai_chat_to_openai_compat`.
///
/// Same-protocol image_url passthrough. This is the simplest cell —
/// inbound image_url shape matches outbound image_url shape — but
/// covers it explicitly in case a future encoder change accidentally
/// drops the image when the inbound and outbound dialects already
/// match.
#[tokio::test]
async fn vision_openai_chat_to_openai_compat() {
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJsonString(
            r#"{"messages":[{"role":"user","content":[
                    {"type":"text","text":"describe this picture"},
                    {"type":"image_url","image_url":{"url":"https://example.com/cat.png"}}
                ]}]}"#
                .into(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(OAI_TEXT_RESPONSE)
        .create();

    let provider =
        OpenAiCompatibleProvider::new("openai", server.url(), "test-key", Default::default(), 30)
            .expect("provider build");

    let stream = provider
        .complete(
            req_with_image(false, FrontendKind::OpenAiChat, "gpt-4o"),
            target("openai", "gpt-4o"),
        )
        .await
        .expect("complete ok");
    let _events: Vec<_> = stream.collect().await;
}

// ── T8: OpenAI Chat frontend → Gemini (cross-protocol) ───────────────

/// Plan 04 T8 cell `vision_openai_chat_to_gemini`.
///
/// OpenAI-frontend image_url → Gemini fileData. Same encoder as the
/// Anthropic→Gemini cell on the provider side; differs in the inbound
/// `frontend.kind` (which is informational only — Gemini's encoder
/// doesn't gate on it). Verifies the provider is frontend-agnostic for
/// vision the same way it is for text + tools (covered by
/// `gemini_smoke::cross_protocol_anthropic_request_routes_through_gemini_unchanged`).
#[tokio::test]
async fn vision_openai_chat_to_gemini() {
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock(
            "POST",
            Matcher::Regex(r"/models/gemini-2\.0-flash:generateContent.*key=test-key".into()),
        )
        .match_body(Matcher::PartialJsonString(
            r#"{"contents":[{"role":"user","parts":[
                    {"text":"describe this picture"},
                    {"fileData":{"fileUri":"https://example.com/cat.png"}}
                ]}]}"#
                .into(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(GEMINI_TEXT_RESPONSE)
        .create();

    let provider = GeminiProvider::new("gemini", server.url(), "test-key", Default::default(), 30)
        .expect("provider build");

    let stream = provider
        .complete(
            req_with_image(false, FrontendKind::OpenAiChat, "gemini-2.0-flash"),
            target("gemini", "gemini-2.0-flash"),
        )
        .await
        .expect("complete ok");
    let _events: Vec<_> = stream.collect().await;
}
