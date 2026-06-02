//! C — Cross-dialect lifecycle harness.
//!
//! Asserts that **every Provider** in the workspace emits a canonical-
//! lifecycle-conformant `CanonicalStream` on a minimal happy-path SSE
//! fixture. The point isn't to re-cover what each provider's own
//! `*_smoke.rs` already does — it's to guarantee that the **shared
//! contract** (CONTEXT.md "Canonical lifecycle") holds uniformly across
//! every wire dialect, anchored against the single `assert_canonical_lifecycle`
//! oracle.
//!
//! When a new provider lands, add one test here following the existing
//! shape. If it diverges from the lifecycle, `assert_canonical_lifecycle`
//! fails with an exact rule + event index.
//!
//! Each test:
//! 1. Stands up a mockito server with a minimal happy-path SSE body in
//!    the provider's own wire dialect.
//! 2. Calls `BackendProvider::complete` with a synthetic streaming
//!    canonical request.
//! 3. Drains the resulting `CanonicalStream` into `Vec<StreamEvent>`.
//! 4. Asserts content survives (so a silently-empty stream can't pass).
//! 5. Calls `assert_canonical_lifecycle` to enforce the shared contract.

use agent_shim_core::{FrontendKind, StreamEvent};
use agent_shim_protocol_tests::lifecycle::assert_canonical_lifecycle;
use agent_shim_protocol_tests::{
    make_anthropic_provider, make_anthropic_target, make_canonical_request, make_deepseek_provider,
    make_deepseek_target, make_gemini_provider, make_gemini_target, make_oai_compat_provider,
    make_oai_compat_target,
};
use agent_shim_providers::BackendProvider;
use futures::StreamExt;

async fn drain_and_validate(stream: agent_shim_core::CanonicalStream) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    let mut stream = stream;
    while let Some(item) = stream.next().await {
        match item {
            Ok(e) => events.push(e),
            Err(e) => panic!("canonical stream yielded error: {e:?}"),
        }
    }
    // The whole point of this harness: every provider's happy path must
    // satisfy the shared contract. Failures here name the exact rule +
    // event index — see `crates/protocol-tests/src/lifecycle.rs`.
    assert_canonical_lifecycle(&events);
    events
}

fn assert_has_text(events: &[StreamEvent], expected: &str) {
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text, expected,
        "concatenated TextDelta text mismatch (events did not survive the parser)"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// OpenAI-Compatible (Chat Completions wire shape)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn openai_compatible_chat_emits_valid_lifecycle() {
    let mut server = mockito::Server::new_async().await;
    let body = concat!(
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let _mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;

    let provider = make_oai_compat_provider(server.url());
    let stream = provider
        .complete(
            make_canonical_request(true, FrontendKind::OpenAiChat),
            make_oai_compat_target(),
        )
        .await
        .expect("complete ok");
    let events = drain_and_validate(stream).await;
    assert_has_text(&events, "hi");
}

// ─────────────────────────────────────────────────────────────────────────────
// DeepSeek (OpenAI-Chat-shape with reasoning_content extensions)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn deepseek_emits_valid_lifecycle() {
    let mut server = mockito::Server::new_async().await;
    let body = concat!(
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let _mock = server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;

    let provider = make_deepseek_provider(server.url());
    let stream = provider
        .complete(
            make_canonical_request(true, FrontendKind::OpenAiChat),
            make_deepseek_target(),
        )
        .await
        .expect("complete ok");
    let events = drain_and_validate(stream).await;
    assert_has_text(&events, "hi");
}

// ─────────────────────────────────────────────────────────────────────────────
// Anthropic (Messages API SSE)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn anthropic_emits_valid_lifecycle() {
    let mut server = mockito::Server::new_async().await;
    // Minimal Anthropic SSE: message_start with a usage block, one text
    // content_block_start/delta/stop, then message_delta + message_stop.
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-opus-4-7\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let _mock = server
        .mock("POST", "/v1/messages")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;

    let provider = make_anthropic_provider(server.url());
    let stream = provider
        .complete(
            make_canonical_request(true, FrontendKind::AnthropicMessages),
            make_anthropic_target(),
        )
        .await
        .expect("complete ok");
    let events = drain_and_validate(stream).await;
    assert_has_text(&events, "hi");
}

// ─────────────────────────────────────────────────────────────────────────────
// Gemini (streamGenerateContent JSON-array wire shape)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn gemini_emits_valid_lifecycle() {
    let mut server = mockito::Server::new_async().await;
    // Gemini streams a JSON array of GenerateContentResponse objects;
    // a single chunk with `finishReason: STOP` terminates the stream.
    let body = "[\n{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}],\"role\":\"model\"},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":1,\"totalTokenCount\":4},\"modelVersion\":\"gemini-2.0-flash\"}\n]";
    let _mock = server
        .mock(
            "POST",
            mockito::Matcher::Regex(
                r"/models/gemini-2\.0-flash:streamGenerateContent.*".to_string(),
            ),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body)
        .create_async()
        .await;

    let provider = make_gemini_provider(server.url());
    let stream = provider
        .complete(
            make_canonical_request(true, FrontendKind::OpenAiChat),
            make_gemini_target(),
        )
        .await
        .expect("complete ok");
    let events = drain_and_validate(stream).await;
    assert_has_text(&events, "hi");
}
