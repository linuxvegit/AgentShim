// Decode an OpenAI request, then encode its response via the Anthropic frontend.
use agent_shim_core::{
    content::ContentBlock,
    ids::ResponseId,
    response::CanonicalResponse,
    usage::StopReason,
};
use agent_shim_frontends::{
    anthropic_messages::AnthropicMessages,
    openai_chat::OpenAiChat,
    FrontendProtocol, FrontendResponse,
};

#[test]
fn decode_openai_encode_anthropic_unary() {
    let openai = OpenAiChat::new();
    let body = br#"{
        "model": "gpt-4o",
        "messages": [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "Hello"}
        ]
    }"#;

    let canonical_req = openai.decode_request(body).unwrap();
    assert_eq!(canonical_req.system.len(), 1);
    assert_eq!(canonical_req.messages.len(), 1);

    let canonical_resp = CanonicalResponse {
        id: ResponseId(String::from("resp_cross_2")),
        model: "claude-3-5-sonnet-20241022".into(),
        content: vec![ContentBlock::text("Hi! How can I help?")],
        stop_reason: StopReason::EndTurn,
        stop_sequence: None,
        usage: None,
    };

    let anthropic = AnthropicMessages::new();
    let result = anthropic.encode_unary(canonical_resp).unwrap();
    let body = match result {
        FrontendResponse::Unary { body, .. } => body,
        FrontendResponse::Stream { .. } => panic!("expected unary"),
    };

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["type"], "message");
    assert_eq!(json["role"], "assistant");
    assert_eq!(json["stop_reason"], "end_turn");
    assert_eq!(json["content"][0]["type"], "text");
    assert_eq!(json["content"][0]["text"], "Hi! How can I help?");
}
