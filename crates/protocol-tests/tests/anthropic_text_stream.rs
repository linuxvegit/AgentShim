use agent_shim_frontends::{
    anthropic_messages::AnthropicMessages, FrontendProtocol, FrontendResponse,
};
use agent_shim_protocol_tests::{collect_sse, fixture, replay_jsonl};

#[tokio::test]
async fn anthropic_text_stream_sse_events() {
    let frontend = AnthropicMessages::new();
    let stream = replay_jsonl(fixture("text_stream.jsonl"), None);

    let response = frontend.encode_stream(stream);
    let body = match response {
        FrontendResponse::Stream { stream, .. } => collect_sse(stream).await,
        FrontendResponse::Unary { .. } => panic!("expected stream"),
    };
    let text = std::str::from_utf8(&body).unwrap();

    // Should contain the main Anthropic SSE event names
    assert!(
        text.contains("event: message_start"),
        "missing message_start\n{}",
        text
    );
    assert!(
        text.contains("event: content_block_start"),
        "missing content_block_start\n{}",
        text
    );
    assert!(
        text.contains("event: content_block_delta"),
        "missing content_block_delta\n{}",
        text
    );
    assert!(
        text.contains("event: content_block_stop"),
        "missing content_block_stop\n{}",
        text
    );
    assert!(
        text.contains("event: message_delta"),
        "missing message_delta\n{}",
        text
    );
    assert!(
        text.contains("event: message_stop"),
        "missing message_stop\n{}",
        text
    );

    // Text content should be present
    assert!(text.contains("Hello"), "missing text 'Hello'\n{}", text);
    assert!(text.contains(", world"), "missing text ', world'\n{}", text);

    // stop_reason
    assert!(
        text.contains("end_turn"),
        "missing stop_reason end_turn\n{}",
        text
    );
}
