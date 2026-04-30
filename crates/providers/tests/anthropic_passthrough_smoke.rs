//! Integration tests for the Anthropic provider's passthrough path
//! (`proxy_raw` with `FrontendKind::AnthropicMessages`).
//!
//! These tests run against a mockito-served fake `api.anthropic.com` and
//! verify the wire-shape gate, byte-for-byte response forwarding, model
//! field rewriting, and per-route `default_anthropic_beta` propagation.
//!
//! The canonical path (T8) and cross-protocol round-trips are out of scope.

use agent_shim_core::{BackendTarget, FrontendKind, RoutePolicy};
use agent_shim_providers::{anthropic::AnthropicProvider, BackendProvider};
use bytes::Bytes;
use futures::StreamExt;

/// Hand-crafted Anthropic SSE response body for a tiny "Hello" completion.
/// Used as the upstream response so passthrough tests can assert byte-equality.
const UPSTREAM_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-4-7\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

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

fn make_target(policy: RoutePolicy) -> BackendTarget {
    BackendTarget {
        provider: "anthropic".to_string(),
        model: "claude-opus-4-7".to_string(),
        policy,
    }
}

fn inbound_body() -> Bytes {
    Bytes::from_static(
        br#"{"model":"alias","messages":[{"role":"user","content":"hi"}],"max_tokens":1024}"#,
    )
}

async fn collect_stream(mut stream: agent_shim_providers::RawByteStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    out
}

#[tokio::test]
async fn passthrough_text_stream_forwards_bytes_and_rewrites_model() {
    let mut server = mockito::Server::new_async().await;

    // Expect: POST /v1/messages with Anthropic auth/version headers, JSON body
    // where `model` has been rewritten to the route's upstream model.
    let mock = server
        .mock("POST", "/v1/messages")
        .match_header("x-api-key", "test-key")
        .match_header("anthropic-version", "2023-06-01")
        .match_header("content-type", "application/json")
        .match_body(mockito::Matcher::PartialJsonString(
            r#"{"model":"claude-opus-4-7"}"#.to_string(),
        ))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(UPSTREAM_SSE)
        .create_async()
        .await;

    let provider = make_provider(server.url());
    let target = make_target(RoutePolicy::default());

    let Some((content_type, stream)) = provider
        .proxy_raw(inbound_body(), target, FrontendKind::AnthropicMessages)
        .await
        .unwrap()
    else {
        panic!("expected passthrough to be applicable for AnthropicMessages frontend");
    };

    assert_eq!(content_type, "text/event-stream");

    let body = collect_stream(stream).await;
    assert_eq!(
        std::str::from_utf8(&body).unwrap(),
        UPSTREAM_SSE,
        "passthrough must forward upstream bytes verbatim"
    );

    mock.assert_async().await;
}

#[tokio::test]
async fn passthrough_returns_none_for_non_anthropic_frontend() {
    // No mockito expectations registered — if the wire-shape gate is broken
    // and the provider tries to call the upstream, mockito returns 501 and
    // the test fails loudly.
    let server = mockito::Server::new_async().await;
    let provider = make_provider(server.url());

    // OpenAI Chat inbound must not be passed through — the canonical path
    // owns those requests.
    let result_chat = provider
        .proxy_raw(
            inbound_body(),
            make_target(RoutePolicy::default()),
            FrontendKind::OpenAiChat,
        )
        .await
        .unwrap();
    assert!(
        result_chat.is_none(),
        "OpenAiChat frontend must fall through to canonical path"
    );

    // Same for OpenAI Responses.
    let result_responses = provider
        .proxy_raw(
            inbound_body(),
            make_target(RoutePolicy::default()),
            FrontendKind::OpenAiResponses,
        )
        .await
        .unwrap();
    assert!(
        result_responses.is_none(),
        "OpenAiResponses frontend must fall through to canonical path"
    );
}

#[tokio::test]
async fn passthrough_propagates_anthropic_beta_from_route_policy() {
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("POST", "/v1/messages")
        .match_header("x-api-key", "test-key")
        .match_header("anthropic-version", "2023-06-01")
        .match_header("anthropic-beta", "context-1m-2025-08-07")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(UPSTREAM_SSE)
        .create_async()
        .await;

    let provider = make_provider(server.url());
    let target = make_target(RoutePolicy {
        default_anthropic_beta: Some("context-1m-2025-08-07".to_string()),
        ..Default::default()
    });

    let Some((_content_type, stream)) = provider
        .proxy_raw(inbound_body(), target, FrontendKind::AnthropicMessages)
        .await
        .unwrap()
    else {
        panic!("expected passthrough to be applicable for AnthropicMessages frontend");
    };

    // Drain so the request completes before assert_async checks the mock.
    let _ = collect_stream(stream).await;

    mock.assert_async().await;
}
