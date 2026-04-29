use agent_shim_core::{
    content::ContentBlock,
    ids::ResponseId,
    response::CanonicalResponse,
    usage::{StopReason, Usage},
};
use agent_shim_frontends::{
    anthropic_messages::AnthropicMessages, FrontendProtocol, FrontendResponse,
};

#[test]
fn anthropic_unary_text_response() {
    let frontend = AnthropicMessages::new();
    let response = CanonicalResponse {
        id: ResponseId(String::from("resp_unary_test")),
        model: "claude-3-5-sonnet-20241022".into(),
        content: vec![ContentBlock::text("The answer is 42.")],
        stop_reason: StopReason::EndTurn,
        stop_sequence: None,
        usage: Some(Usage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            ..Default::default()
        }),
    };

    let result = frontend.encode_unary(response).unwrap();
    let body = match result {
        FrontendResponse::Unary { body, .. } => body,
        FrontendResponse::Stream { .. } => panic!("expected unary"),
    };

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["type"], "message");
    assert_eq!(json["role"], "assistant");
    assert_eq!(json["stop_reason"], "end_turn");
    assert_eq!(json["content"][0]["type"], "text");
    assert_eq!(json["content"][0]["text"], "The answer is 42.");
    assert_eq!(json["usage"]["input_tokens"], 10);
    assert_eq!(json["usage"]["output_tokens"], 5);
}

#[test]
fn anthropic_unary_tool_call_response() {
    use agent_shim_core::{
        extensions::ExtensionMap,
        ids::ToolCallId,
        tool::{ToolCallArguments, ToolCallBlock},
    };

    let frontend = AnthropicMessages::new();
    let response = CanonicalResponse {
        id: ResponseId(String::from("resp_tool_test")),
        model: "claude-3-5-sonnet-20241022".into(),
        content: vec![ContentBlock::ToolCall(ToolCallBlock {
            id: ToolCallId::from_provider("call_xyz"),
            name: "search".into(),
            arguments: ToolCallArguments::Complete {
                value: serde_json::json!({"q": "rust"}),
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
    assert_eq!(json["stop_reason"], "tool_use");
    assert_eq!(json["content"][0]["type"], "tool_use");
    assert_eq!(json["content"][0]["id"], "call_xyz");
    assert_eq!(json["content"][0]["name"], "search");
}
