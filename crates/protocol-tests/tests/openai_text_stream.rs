use agent_shim_frontends::{openai_chat::OpenAiChat, FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{collect_sse, fixture, replay_jsonl};

#[tokio::test]
async fn openai_text_stream_chunks() {
    let frontend = OpenAiChat {
        keepalive: None,
        clock_override: Some(1700000000),
    };
    let stream = replay_jsonl(fixture("text_stream.jsonl"), None);

    let response = frontend.encode_stream(stream);
    let body = match response {
        FrontendResponse::Stream { stream, .. } => collect_sse(stream).await,
        FrontendResponse::Unary { .. } => panic!("expected stream"),
    };
    let text = std::str::from_utf8(&body).unwrap();

    // OpenAI streaming uses data-only SSE lines
    assert!(text.contains("data: "), "missing data: prefix\n{}", text);

    // Role chunk
    assert!(
        text.contains("\"assistant\""),
        "missing assistant role\n{}",
        text
    );

    // Text content
    assert!(text.contains("Hello"), "missing 'Hello'\n{}", text);
    assert!(text.contains(", world"), "missing ', world'\n{}", text);

    // finish_reason = "stop" for end_turn
    assert!(
        text.contains("\"stop\""),
        "missing finish_reason stop\n{}",
        text
    );

    // Usage chunk
    assert!(text.contains("prompt_tokens"), "missing usage\n{}", text);

    // DONE terminator
    assert!(text.contains("[DONE]"), "missing [DONE]\n{}", text);
}
