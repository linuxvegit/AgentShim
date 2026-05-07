use agent_shim_frontends::{openai_responses::OpenAiResponses, FrontendProtocol, FrontendResponse};
use agent_shim_protocol_tests::{collect_sse, fixture, replay_jsonl};

#[tokio::test]
async fn responses_text_simple_emits_expected_sse_events() {
    let frontend = OpenAiResponses {
        keepalive: None,
        clock_override: Some(1700000000),
    };
    let stream = replay_jsonl(fixture("responses_text_simple.jsonl"), None);

    let response = frontend.encode_stream(stream);
    let body = match response {
        FrontendResponse::Stream { stream, .. } => collect_sse(stream).await,
        FrontendResponse::Unary { .. } => panic!("expected stream"),
    };
    let text = std::str::from_utf8(&body).unwrap();

    // response.created with the encoder-prefixed id.
    assert!(
        text.contains("event: response.created"),
        "missing response.created\n{}",
        text
    );
    assert!(
        text.contains("\"resp_resp_t1\""),
        "missing prefixed response id\n{}",
        text
    );

    // Output item lifecycle for the assistant message.
    assert!(
        text.contains("event: response.output_item.added"),
        "missing output_item.added\n{}",
        text
    );
    assert!(
        text.contains("\"type\":\"message\""),
        "missing message item type\n{}",
        text
    );
    assert!(
        text.contains("event: response.content_part.added"),
        "missing content_part.added\n{}",
        text
    );

    // Each text_delta lands as an output_text.delta event with the right delta payload.
    assert!(
        text.contains("event: response.output_text.delta"),
        "missing output_text.delta\n{}",
        text
    );
    assert!(
        text.contains("\"delta\":\"Hello, \""),
        "missing 'Hello, ' delta\n{}",
        text
    );
    assert!(
        text.contains("\"delta\":\"world!\""),
        "missing 'world!' delta\n{}",
        text
    );

    // output_text.done carries the accumulated text.
    assert!(
        text.contains("event: response.output_text.done"),
        "missing output_text.done\n{}",
        text
    );
    assert!(
        text.contains("\"text\":\"Hello, world!\""),
        "missing accumulated text\n{}",
        text
    );

    // content_part.done and output_item.done with completed status.
    assert!(
        text.contains("event: response.content_part.done"),
        "missing content_part.done\n{}",
        text
    );
    assert!(
        text.contains("event: response.output_item.done"),
        "missing output_item.done\n{}",
        text
    );
    assert!(
        text.contains("\"status\":\"completed\""),
        "missing completed status on output_item.done\n{}",
        text
    );

    // response.completed carries usage.
    assert!(
        text.contains("event: response.completed"),
        "missing response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"input_tokens\":8,\"output_tokens\":4"),
        "missing usage on response.completed\n{}",
        text
    );
}

#[tokio::test]
async fn responses_tools_parallel_emits_expected_sse_events() {
    let frontend = OpenAiResponses {
        keepalive: None,
        clock_override: Some(1700000000),
    };
    let stream = replay_jsonl(fixture("responses_tools_parallel.jsonl"), None);

    let response = frontend.encode_stream(stream);
    let body = match response {
        FrontendResponse::Stream { stream, .. } => collect_sse(stream).await,
        FrontendResponse::Unary { .. } => panic!("expected stream"),
    };
    let text = std::str::from_utf8(&body).unwrap();

    // response.created with the encoder-prefixed id.
    assert!(
        text.contains("event: response.created"),
        "missing response.created\n{}",
        text
    );
    assert!(
        text.contains("\"resp_resp_t2\""),
        "missing prefixed response id\n{}",
        text
    );

    // Two function_call output items added.
    let added_count = text.matches("event: response.output_item.added").count();
    assert_eq!(
        added_count, 2,
        "expected 2 output_item.added events, got {}\n{}",
        added_count, text
    );
    let function_call_added = text.matches("\"type\":\"function_call\"").count();
    assert!(
        function_call_added >= 2,
        "expected at least 2 function_call item types, got {}\n{}",
        function_call_added,
        text
    );

    // Function call args deltas appear with their fragments.
    assert!(
        text.contains("event: response.function_call_arguments.delta"),
        "missing function_call_arguments.delta\n{}",
        text
    );
    assert!(
        text.contains("\"delta\":\"{\\\"city\\\":\\\"\""),
        "missing first arguments delta fragment\n{}",
        text
    );
    assert!(
        text.contains("\"delta\":\"Tokyo\\\"}\""),
        "missing second arguments delta fragment\n{}",
        text
    );

    // Two function_call_arguments.done events with the completed argument strings.
    let done_count = text
        .matches("event: response.function_call_arguments.done")
        .count();
    assert_eq!(
        done_count, 2,
        "expected 2 function_call_arguments.done events, got {}\n{}",
        done_count, text
    );
    assert!(
        text.contains("\"arguments\":\"{\\\"city\\\":\\\"Tokyo\\\"}\""),
        "missing completed Tokyo arguments\n{}",
        text
    );
    assert!(
        text.contains("\"arguments\":\"{}\""),
        "missing completed empty arguments\n{}",
        text
    );

    // Two output_item.done events.
    let item_done_count = text.matches("event: response.output_item.done").count();
    assert_eq!(
        item_done_count, 2,
        "expected 2 output_item.done events, got {}\n{}",
        item_done_count, text
    );

    // response.completed with usage.
    assert!(
        text.contains("event: response.completed"),
        "missing response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"input_tokens\":20,\"output_tokens\":15"),
        "missing usage on response.completed\n{}",
        text
    );
}

#[tokio::test]
async fn responses_reasoning_o1_emits_expected_sse_events() {
    let frontend = OpenAiResponses {
        keepalive: None,
        clock_override: Some(1700000000),
    };
    let stream = replay_jsonl(fixture("responses_reasoning_o1.jsonl"), None);

    let response = frontend.encode_stream(stream);
    let body = match response {
        FrontendResponse::Stream { stream, .. } => collect_sse(stream).await,
        FrontendResponse::Unary { .. } => panic!("expected stream"),
    };
    let text = std::str::from_utf8(&body).unwrap();

    // response.created with the encoder-prefixed id.
    assert!(
        text.contains("event: response.created"),
        "missing response.created\n{}",
        text
    );
    assert!(
        text.contains("\"resp_resp_t3\""),
        "missing prefixed response id\n{}",
        text
    );

    // First output_item.added is the reasoning item with rs_0 id.
    let reasoning_added_idx = text
        .find("event: response.output_item.added")
        .expect("reasoning output_item.added present");
    let reasoning_added_window = &text[reasoning_added_idx..];
    assert!(
        reasoning_added_window.contains("\"type\":\"reasoning\""),
        "reasoning output_item.added missing type=reasoning\n{}",
        text
    );
    assert!(
        reasoning_added_window.contains("\"id\":\"rs_0\""),
        "reasoning output_item.added missing id=rs_0\n{}",
        text
    );

    // Two reasoning.delta events with the right fragments.
    assert!(
        text.contains("event: response.reasoning.delta"),
        "missing reasoning.delta\n{}",
        text
    );
    assert!(
        text.contains("\"delta\":\"Let me think \""),
        "missing first reasoning delta\n{}",
        text
    );
    assert!(
        text.contains("\"delta\":\"step by step.\""),
        "missing second reasoning delta\n{}",
        text
    );

    // reasoning.done carries the accumulated text.
    assert!(
        text.contains("event: response.reasoning.done"),
        "missing reasoning.done\n{}",
        text
    );
    assert!(
        text.contains("\"text\":\"Let me think step by step.\""),
        "missing accumulated reasoning text\n{}",
        text
    );

    // First output_item.done is the completed reasoning item.
    let item_done_idx = text
        .find("event: response.output_item.done")
        .expect("first output_item.done present");
    let item_done_window = &text[item_done_idx..];
    assert!(
        item_done_window.contains("\"type\":\"reasoning\""),
        "first output_item.done missing type=reasoning\n{}",
        text
    );
    assert!(
        item_done_window.contains("\"status\":\"completed\""),
        "first output_item.done missing completed status\n{}",
        text
    );

    // After the reasoning completes, a message item is added with id msg_1.
    // Search past the reasoning item-added position to find the next output_item.added.
    let after_reasoning = &text[reasoning_added_idx + 1..];
    let message_added_relative = after_reasoning
        .find("event: response.output_item.added")
        .expect("second output_item.added (message) present");
    let message_added_window = &after_reasoning[message_added_relative..];
    assert!(
        message_added_window.contains("\"type\":\"message\""),
        "second output_item.added missing type=message\n{}",
        text
    );
    assert!(
        message_added_window.contains("\"id\":\"msg_1\""),
        "second output_item.added missing id=msg_1\n{}",
        text
    );

    // The message body produces an output_text.delta with the answer.
    assert!(
        text.contains("event: response.output_text.delta"),
        "missing output_text.delta\n{}",
        text
    );
    assert!(
        text.contains("\"delta\":\"The answer is 42.\""),
        "missing answer delta\n{}",
        text
    );

    // response.completed at the tail.
    assert!(
        text.contains("event: response.completed"),
        "missing response.completed\n{}",
        text
    );
    assert!(
        text.contains("\"input_tokens\":12,\"output_tokens\":20"),
        "missing usage on response.completed\n{}",
        text
    );
}
