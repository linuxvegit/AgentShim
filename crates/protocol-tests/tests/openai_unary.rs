use agent_shim_core::{
    content::ContentBlock,
    ids::ResponseId,
    response::CanonicalResponse,
    usage::{StopReason, Usage},
};
use agent_shim_frontends::{openai_chat::OpenAiChat, FrontendProtocol, FrontendResponse};

#[test]
fn openai_unary_text_response() {
    let frontend = OpenAiChat { keepalive: None, clock_override: Some(1700000000) };
    let response = CanonicalResponse {
        id: ResponseId(String::from("chatcmpl-test")),
        model: "gpt-4o".into(),
        content: vec![ContentBlock::text("Hello there!")],
        stop_reason: StopReason::EndTurn,
        stop_sequence: None,
        usage: Some(Usage {
            input_tokens: Some(8),
            output_tokens: Some(3),
            ..Default::default()
        }),
    };

    let result = frontend.encode_unary(response).unwrap();
    let body = match result {
        FrontendResponse::Unary { body, .. } => body,
        FrontendResponse::Stream { .. } => panic!("expected unary"),
    };

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["object"], "chat.completion");
    assert_eq!(json["choices"][0]["message"]["role"], "assistant");
    assert_eq!(json["choices"][0]["message"]["content"], "Hello there!");
    assert_eq!(json["choices"][0]["finish_reason"], "stop");
    assert_eq!(json["usage"]["prompt_tokens"], 8);
    assert_eq!(json["usage"]["completion_tokens"], 3);
    assert_eq!(json["usage"]["total_tokens"], 11);
    assert_eq!(json["created"], 1700000000_u64);
}

#[test]
fn openai_unary_tool_call_response() {
    use agent_shim_core::{
        extensions::ExtensionMap,
        ids::ToolCallId,
        tool::{ToolCallArguments, ToolCallBlock},
    };

    let frontend = OpenAiChat { keepalive: None, clock_override: Some(1700000000) };
    let response = CanonicalResponse {
        id: ResponseId(String::from("chatcmpl-tool")),
        model: "gpt-4o".into(),
        content: vec![ContentBlock::ToolCall(ToolCallBlock {
            id: ToolCallId::from_provider("call_abc"),
            name: "get_weather".into(),
            arguments: ToolCallArguments::Complete {
                value: serde_json::json!({"city": "Tokyo"}),
            },
            extensions: ExtensionMap::new(),
        })],
        stop_reason: StopReason::ToolUse,
        stop_sequence: None,
        usage: None,
    };

    let result = frontend.encode_unary(response).unwrap();
    let body = match result {
        FrontendResponse::Unary { body, .. } => body,
        FrontendResponse::Stream { .. } => panic!("expected unary"),
    };

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(json["choices"][0]["message"]["role"], "assistant");
    let tc = &json["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(tc["id"], "call_abc");
    assert_eq!(tc["type"], "function");
    assert_eq!(tc["function"]["name"], "get_weather");
    // arguments should be valid JSON string
    let args_str = tc["function"]["arguments"].as_str().unwrap();
    let args: serde_json::Value = serde_json::from_str(args_str).unwrap();
    assert_eq!(args["city"], "Tokyo");
}
