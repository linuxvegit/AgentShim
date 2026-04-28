// Decode an Anthropic request, then encode its response via the OpenAI frontend.
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
fn decode_anthropic_encode_openai_unary() {
    let anthropic = AnthropicMessages::new();
    let body = br#"{
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "What is Rust?"}]
    }"#;

    let canonical_req = anthropic.decode_request(body).unwrap();
    assert_eq!(canonical_req.messages.len(), 1);

    // Simulate a backend response
    let canonical_resp = CanonicalResponse {
        id: ResponseId(String::from("resp_cross_1")),
        model: canonical_req.model.0.clone(),
        content: vec![ContentBlock::text("Rust is a systems language.")],
        stop_reason: StopReason::EndTurn,
        stop_sequence: None,
        usage: None,
    };

    let openai = OpenAiChat { keepalive: None, clock_override: Some(1700000000) };
    let result = openai.encode_unary(canonical_resp).unwrap();
    let body = match result {
        FrontendResponse::Unary { body, .. } => body,
        FrontendResponse::Stream { .. } => panic!("expected unary"),
    };

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["object"], "chat.completion");
    assert_eq!(json["choices"][0]["message"]["content"], "Rust is a systems language.");
    assert_eq!(json["choices"][0]["finish_reason"], "stop");
}
