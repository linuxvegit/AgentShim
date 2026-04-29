use agent_shim_frontends::{openai_chat::OpenAiChat, FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{collect_sse, fixture, replay_jsonl};

#[tokio::test]
async fn openai_tool_call_stream_chunks() {
    let frontend = OpenAiChat {
        keepalive: None,
        clock_override: Some(1700000001),
    };
    let stream = replay_jsonl(fixture("tool_call_stream.jsonl"), None);

    let response = frontend.encode_stream(stream);
    let body = match response {
        FrontendResponse::Stream { stream, .. } => collect_sse(stream).await,
        FrontendResponse::Unary { .. } => panic!("expected stream"),
    };
    let text = std::str::from_utf8(&body).unwrap();

    // Tool call id and name
    assert!(text.contains("call_abc"), "missing tool call id\n{}", text);
    assert!(text.contains("get_weather"), "missing tool name\n{}", text);

    // Argument fragments (they appear JSON-encoded inside the SSE data)
    assert!(
        text.contains("city"),
        "missing 'city' key in arg fragment\n{}",
        text
    );
    assert!(
        text.contains("Tokyo"),
        "missing 'Tokyo' value in arg fragment\n{}",
        text
    );

    // finish_reason = "tool_calls"
    assert!(
        text.contains("\"tool_calls\""),
        "missing finish_reason tool_calls\n{}",
        text
    );

    // DONE terminator
    assert!(text.contains("[DONE]"), "missing [DONE]\n{}", text);
}
