//! Integration tests for the DeepSeek provider's `complete()` method.
//!
//! These tests run against a mockito-served fake `api.deepseek.com` and verify:
//!
//! * **Text streaming** (`deepseek-chat`-shape SSE) parses into the expected
//!   canonical event sequence, with the outbound body's `model` rewritten to
//!   the route's upstream model and an OpenAI-style `Authorization: Bearer ...`
//!   header.
//! * **Reasoning streaming** (`deepseek-reasoner`-shape SSE with
//!   `reasoning_content` deltas) interleaves canonical `Reasoning` and `Text`
//!   blocks via `ReasoningInterleaver`, with the reasoning block at index 0
//!   followed by the text block at index 1.
//! * **Cache hit unary** maps DeepSeek's `prompt_cache_hit_tokens` into the
//!   canonical `Usage.cache_read_input_tokens` slot, falls back to
//!   `prompt_cache_miss_tokens` for `input_tokens`, and preserves the original
//!   upstream JSON in `Usage.provider_raw`.
//! * **Cross-protocol**: a `CanonicalRequest` synthesised with
//!   `FrontendKind::AnthropicMessages` (i.e. an inbound Anthropic Messages
//!   request) routes through DeepSeek's `complete()` unchanged — the provider
//!   does not gate on inbound frontend kind. This is the provider-side cross-
//!   protocol verification; the gateway-level HTTP path (Anthropic frontend's
//!   `encode_stream` consuming the canonical events to produce
//!   `content_block_start type: thinking` SSE) is covered by the Anthropic
//!   frontend's own tests, mirroring Plan 01 T8's posture.
//!
//! Fixtures are inline `&str` constants rather than separate `.sse`/`.json`
//! files (Plan 01 T7 precedent), and they are hand-crafted from DeepSeek's
//! documented wire shape rather than captured from a live API key (Plan 02 T7
//! adjustment — no `DEEPSEEK_API_KEY` required to run the suite).

use agent_shim_core::{
    BackendTarget, CanonicalRequest, ContentBlock, ContentBlockKind, ExtensionMap, FrontendInfo,
    FrontendKind, FrontendModel, GenerationOptions, Message, RequestId, StopReason, StreamEvent,
};
use agent_shim_providers::{deepseek::DeepseekProvider, BackendProvider};
use futures::StreamExt;

/// DeepSeek `deepseek-chat` SSE for a "Hello world" text completion. Mirrors
/// the documented OpenAI-compatible chunk shape: each event is a
/// `chat.completion.chunk` with a `delta` carrying the role/content fragments.
/// The final chunk closes with `finish_reason: stop` and a usage block whose
/// fields are DeepSeek-specific (`prompt_cache_hit_tokens` /
/// `prompt_cache_miss_tokens`).
const TEXT_SSE: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"prompt_cache_hit_tokens\":0,\"prompt_cache_miss_tokens\":10}}\n\n",
    "data: [DONE]\n\n",
);

/// DeepSeek `deepseek-reasoner` SSE for a reasoning-then-text completion. The
/// stream begins with `reasoning_content` deltas (which the canonical parser
/// routes into a `Reasoning` block at index 0), then transitions to `content`
/// deltas (which open a new `Text` block at index 1 — the
/// `ReasoningInterleaver` closes the reasoning block on the kind switch).
const REASONING_SSE: &str = concat!(
    "data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"deepseek-reasoner\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"Let me think\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"deepseek-reasoner\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\" about this\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"deepseek-reasoner\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"42\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"deepseek-reasoner\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":15,\"completion_tokens\":8,\"prompt_cache_hit_tokens\":0,\"prompt_cache_miss_tokens\":15}}\n\n",
    "data: [DONE]\n\n",
);

/// DeepSeek `deepseek-chat` unary response with a prompt-cache hit
/// (`prompt_cache_hit_tokens: 80`, `prompt_cache_miss_tokens: 20`). The
/// canonical mapping in `super::usage::map_usage` should land 80 in
/// `cache_read_input_tokens`, 20 in `input_tokens`, and preserve the entire
/// upstream `usage` object verbatim in `provider_raw`.
const CACHE_HIT_UNARY: &str = r#"{
    "id": "chatcmpl-3",
    "object": "chat.completion",
    "created": 1700000000,
    "model": "deepseek-chat",
    "choices": [{
        "index": 0,
        "message": {"role": "assistant", "content": "Cached answer"},
        "finish_reason": "stop"
    }],
    "usage": {
        "prompt_tokens": 100,
        "completion_tokens": 12,
        "prompt_cache_hit_tokens": 80,
        "prompt_cache_miss_tokens": 20
    }
}"#;

fn make_provider(base_url: String) -> DeepseekProvider {
    DeepseekProvider::new("deepseek", base_url, "test-key", Default::default(), 30)
        .expect("DeepseekProvider construction failed")
}

fn make_target(model: &str) -> BackendTarget {
    BackendTarget {
        provider: "deepseek".to_string(),
        model: model.to_string(),
        policy: Default::default(),
    }
}

/// Build a synthetic `CanonicalRequest`. The DeepSeek provider's `complete()`
/// path does not gate on `frontend.kind`, so any inbound frontend (OpenAiChat,
/// OpenAiResponses, AnthropicMessages) routes through the same code.
fn make_req(frontend_kind: FrontendKind, stream: bool) -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: frontend_kind,
            requested_model: FrontendModel::from("alias-model"),
        },
        model: FrontendModel::from("alias-model"),
        system: vec![],
        messages: vec![Message::user(vec![ContentBlock::text("hi")])],
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

// ─────────────────────────────────────────────────────────────────────────────
// T8 bullet 1 — text stream against deepseek-chat
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn text_stream_yields_canonical_events() {
    let mut server = mockito::Server::new_async().await;

    // Verify the cross-cutting wire-shape invariants: POST /chat/completions
    // (no /v1 — DeepSeek's base_url already includes it), Bearer auth, and a
    // body where the `model` field has been rewritten to the route's upstream
    // model name with `stream=true` set.
    let mock = server
        .mock("POST", "/chat/completions")
        .match_header("authorization", "Bearer test-key")
        .match_header("content-type", "application/json")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::PartialJsonString(r#"{"model":"deepseek-chat"}"#.to_string()),
            mockito::Matcher::PartialJsonString(r#"{"stream":true}"#.to_string()),
        ]))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(TEXT_SSE)
        .create_async()
        .await;

    let provider = make_provider(server.url());
    let stream = provider
        .complete(
            make_req(FrontendKind::OpenAiChat, true),
            make_target("deepseek-chat"),
        )
        .await
        .unwrap();

    let events = drain(stream).await;

    // ── Stream opens with ResponseStart + MessageStart ───────────────────
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

    // ── A single Text content block opens, accepts deltas, then closes ────
    let text_block_starts: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                StreamEvent::ContentBlockStart {
                    kind: ContentBlockKind::Text,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(
        text_block_starts.len(),
        1,
        "expected exactly one Text ContentBlockStart, got {text_block_starts:?}"
    );

    // Text deltas arrive in order, matching the upstream payload.
    let texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["Hello", " world"]);

    // ── MessageStop carries the mapped finish_reason ──────────────────────
    let stop = events
        .iter()
        .find(|e| matches!(e, StreamEvent::MessageStop { .. }))
        .expect("expected MessageStop");
    if let StreamEvent::MessageStop { stop_reason, .. } = stop {
        assert_eq!(*stop_reason, StopReason::EndTurn);
    }

    // ── Usage delta arrives near the end with mapped DeepSeek fields ──────
    let usage_delta = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::UsageDelta { usage } => Some(usage.clone()),
            _ => None,
        })
        .expect("expected UsageDelta from final usage block");
    // miss=10 lands in input_tokens; hit=0 is preserved as Some(0).
    assert_eq!(usage_delta.input_tokens, Some(10));
    assert_eq!(usage_delta.output_tokens, Some(5));
    assert_eq!(usage_delta.cache_read_input_tokens, Some(0));
    assert_eq!(usage_delta.cache_creation_input_tokens, None);

    // ── Stream closes with ResponseStop ───────────────────────────────────
    assert!(
        matches!(events.last(), Some(StreamEvent::ResponseStop { .. })),
        "last event must be ResponseStop, got {:?}",
        events.last()
    );

    mock.assert_async().await;
}

// ─────────────────────────────────────────────────────────────────────────────
// T8 bullet 2 — reasoning stream against deepseek-reasoner
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn reasoning_stream_yields_interleaved_reasoning_and_text() {
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("POST", "/chat/completions")
        .match_header("authorization", "Bearer test-key")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::PartialJsonString(r#"{"model":"deepseek-reasoner"}"#.to_string()),
            mockito::Matcher::PartialJsonString(r#"{"stream":true}"#.to_string()),
        ]))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(REASONING_SSE)
        .create_async()
        .await;

    let provider = make_provider(server.url());
    let stream = provider
        .complete(
            make_req(FrontendKind::OpenAiChat, true),
            make_target("deepseek-reasoner"),
        )
        .await
        .unwrap();

    let events = drain(stream).await;

    // ── A Reasoning block at index 0 opens first ──────────────────────────
    let reasoning_start_pos = events
        .iter()
        .position(|e| {
            matches!(
                e,
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Reasoning,
                }
            )
        })
        .expect("expected Reasoning ContentBlockStart at index 0");

    // ── A Text block at index 1 opens after the reasoning block ──────────
    let text_start_pos = events
        .iter()
        .position(|e| {
            matches!(
                e,
                StreamEvent::ContentBlockStart {
                    index: 1,
                    kind: ContentBlockKind::Text,
                }
            )
        })
        .expect("expected Text ContentBlockStart at index 1");

    assert!(
        reasoning_start_pos < text_start_pos,
        "Reasoning block must open before Text block (reasoning_pos={reasoning_start_pos}, text_pos={text_start_pos})"
    );

    // The reasoning block must close before the text block opens (the
    // interleaver flushes on kind switch).
    let reasoning_stop_pos = events
        .iter()
        .position(|e| matches!(e, StreamEvent::ContentBlockStop { index: 0 }))
        .expect("expected ContentBlockStop for reasoning block at index 0");
    assert!(
        reasoning_stop_pos < text_start_pos,
        "Reasoning ContentBlockStop must precede Text ContentBlockStart"
    );

    // ── Reasoning deltas (index 0) are concatenated, in order ─────────────
    let reasoning_texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ReasoningDelta { index: 0, text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning_texts, vec!["Let me think", " about this"]);

    // ── Text deltas (index 1) carry the final answer ──────────────────────
    let text_texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { index: 1, text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text_texts, vec!["42"]);

    // ── MessageStop carries EndTurn ───────────────────────────────────────
    let stop = events
        .iter()
        .find(|e| matches!(e, StreamEvent::MessageStop { .. }))
        .expect("expected MessageStop");
    if let StreamEvent::MessageStop { stop_reason, .. } = stop {
        assert_eq!(*stop_reason, StopReason::EndTurn);
    }

    mock.assert_async().await;
}

// ─────────────────────────────────────────────────────────────────────────────
// T8 bullet 3 — cache-hit unary
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cache_hit_unary_populates_cache_read_input_tokens() {
    let mut server = mockito::Server::new_async().await;

    // Non-streaming: outbound body must have `stream=false`, response is JSON.
    let mock = server
        .mock("POST", "/chat/completions")
        .match_header("authorization", "Bearer test-key")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::PartialJsonString(r#"{"model":"deepseek-chat"}"#.to_string()),
            mockito::Matcher::PartialJsonString(r#"{"stream":false}"#.to_string()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(CACHE_HIT_UNARY)
        .create_async()
        .await;

    let provider = make_provider(server.url());
    let stream = provider
        .complete(
            make_req(FrontendKind::OpenAiChat, false),
            make_target("deepseek-chat"),
        )
        .await
        .unwrap();

    let events = drain(stream).await;

    // Sanity: ResponseStart carries the upstream message id and the body is
    // unwrapped into the canonical assistant text.
    match events.first() {
        Some(StreamEvent::ResponseStart { id, .. }) => {
            assert_eq!(id.0, "chatcmpl-3");
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
    assert_eq!(texts, vec!["Cached answer"]);

    // ── ResponseStop carries the cache-mapped Usage ──────────────────────
    let usage = match events.last() {
        Some(StreamEvent::ResponseStop { usage }) => usage
            .as_ref()
            .expect("ResponseStop must carry usage for unary cache-hit response"),
        other => panic!("last event must be ResponseStop, got {other:?}"),
    };
    // Cache miss (20) lands in input_tokens (the *new* prompt portion).
    assert_eq!(usage.input_tokens, Some(20));
    // Cache hit (80) lands in cache_read_input_tokens.
    assert_eq!(usage.cache_read_input_tokens, Some(80));
    // DeepSeek doesn't report cache creation.
    assert_eq!(usage.cache_creation_input_tokens, None);
    // Output tokens flow through unchanged.
    assert_eq!(usage.output_tokens, Some(12));

    // ── provider_raw preserves DeepSeek-specific fields verbatim ─────────
    let raw = usage
        .provider_raw
        .as_ref()
        .expect("provider_raw must be populated");
    assert_eq!(
        raw.get("prompt_cache_hit_tokens").and_then(|x| x.as_u64()),
        Some(80),
        "provider_raw must preserve prompt_cache_hit_tokens verbatim"
    );
    assert_eq!(
        raw.get("prompt_cache_miss_tokens").and_then(|x| x.as_u64()),
        Some(20),
        "provider_raw must preserve prompt_cache_miss_tokens verbatim"
    );
    assert_eq!(
        raw.get("prompt_tokens").and_then(|x| x.as_u64()),
        Some(100),
        "provider_raw must preserve original prompt_tokens (not the post-mapping value)"
    );

    mock.assert_async().await;
}

// ─────────────────────────────────────────────────────────────────────────────
// T9 — cross-protocol: Anthropic frontend kind routes through DeepSeek
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn canonical_path_with_anthropic_frontend_renders_reasoning_then_text() {
    // T9 cross-protocol verification at the provider boundary: a
    // `CanonicalRequest` carrying `FrontendKind::AnthropicMessages` (i.e. an
    // inbound Anthropic Messages request that the gateway has decoded) routes
    // through the same DeepSeek `complete()` code path as an OpenAI Chat
    // inbound request. The canonical event stream is identical regardless of
    // the inbound frontend kind.
    //
    // The frontend-side encoding (canonical → Anthropic SSE with
    // `content_block_start type: thinking` followed by
    // `content_block_start type: text`) is covered by the Anthropic frontend's
    // own `encode_stream` tests; this test verifies the provider half of the
    // cross-protocol round-trip works, mirroring Plan 01 T8's posture.
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("POST", "/chat/completions")
        .match_header("authorization", "Bearer test-key")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::PartialJsonString(r#"{"model":"deepseek-reasoner"}"#.to_string()),
            mockito::Matcher::PartialJsonString(r#"{"stream":true}"#.to_string()),
        ]))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(REASONING_SSE)
        .create_async()
        .await;

    let provider = make_provider(server.url());
    let stream = provider
        .complete(
            make_req(FrontendKind::AnthropicMessages, true),
            make_target("deepseek-reasoner"),
        )
        .await
        .unwrap();

    let events = drain(stream).await;

    // Same invariants as the OpenAiChat reasoning test: Reasoning block at
    // index 0 must open and close before the Text block at index 1 opens.
    let reasoning_start_pos = events
        .iter()
        .position(|e| {
            matches!(
                e,
                StreamEvent::ContentBlockStart {
                    index: 0,
                    kind: ContentBlockKind::Reasoning,
                }
            )
        })
        .expect("expected Reasoning ContentBlockStart at index 0");
    let reasoning_stop_pos = events
        .iter()
        .position(|e| matches!(e, StreamEvent::ContentBlockStop { index: 0 }))
        .expect("expected ContentBlockStop for reasoning block at index 0");
    let text_start_pos = events
        .iter()
        .position(|e| {
            matches!(
                e,
                StreamEvent::ContentBlockStart {
                    index: 1,
                    kind: ContentBlockKind::Text,
                }
            )
        })
        .expect("expected Text ContentBlockStart at index 1");
    assert!(reasoning_start_pos < reasoning_stop_pos);
    assert!(reasoning_stop_pos < text_start_pos);

    // Reasoning deltas (index 0) are concatenated, in order.
    let reasoning_texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ReasoningDelta { index: 0, text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning_texts, vec!["Let me think", " about this"]);

    // Text deltas (index 1) carry the final answer.
    let text_texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { index: 1, text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text_texts, vec!["42"]);

    mock.assert_async().await;
}
