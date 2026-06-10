//! Cross-protocol verification for the 2026-06-10 positional system
//! messages spec.
//!
//! Matrix coverage (per spec §5.4):
//!   * Anthropic-in → Anthropic-out: positional preserved
//!   * Anthropic-in → OpenAI Chat-out: positional preserved
//!   * Anthropic-in → OpenAI Responses-out: downgrade to instructions
//!   * Anthropic-in → Gemini-out: downgrade to systemInstruction
//!   * OpenAI Chat-in → Anthropic-out: positional preserved

use agent_shim_core::{
    ids::RequestId,
    message::{Message, SystemSource},
    request::{CanonicalRequest, GenerationOptions, RequestMetadata},
    target::{BackendTarget, FrontendInfo, FrontendKind, FrontendModel},
    tool::ToolChoice,
    ContentBlock, ExtensionMap,
};

/// Build a canonical request with a single `User → System → User` sequence,
/// suitable for verifying that each outbound encoder either preserves or
/// downgrades the positional system block as required by spec §4.
///
/// `frontend` flags what produced the request, and `source` tags the System
/// message with its original wire shape (Anthropic `system` block,
/// OpenAI `system` role, or OpenAI `developer` role) — necessary so encoders
/// can pick the right downgrade vs preserve path.
fn canonical_user_system_user(frontend: FrontendKind, source: SystemSource) -> CanonicalRequest {
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: frontend,
            requested_model: FrontendModel::from("test-model"),
        },
        model: FrontendModel::from("test-model"),
        system: vec![],
        messages: vec![
            Message::user(vec![ContentBlock::text("turn one")]),
            Message::system(source, vec![ContentBlock::text("mid system")]),
            Message::user(vec![ContentBlock::text("turn two")]),
        ],
        tools: vec![],
        tool_choice: ToolChoice::default(),
        generation: GenerationOptions::default(),
        response_format: None,
        stream: false,
        metadata: RequestMetadata::default(),
        inbound_anthropic_headers: vec![],
        resolved_policy: Default::default(),
        extensions: ExtensionMap::new(),
    }
}

fn anthropic_target() -> BackendTarget {
    BackendTarget {
        provider: "anthropic".into(),
        model: "claude-opus-4-8".into(),
        policy: Default::default(),
    }
}

fn openai_target() -> BackendTarget {
    BackendTarget {
        provider: "openai".into(),
        model: "gpt-5".into(),
        policy: Default::default(),
    }
}

fn gemini_target() -> BackendTarget {
    BackendTarget {
        provider: "gemini".into(),
        model: "gemini-2.0-flash".into(),
        policy: Default::default(),
    }
}

// ───────────────────────── Matrix tests (spec §5.4) ─────────────────────────

#[test]
fn anthropic_in_anthropic_out_preserves_positional_system() {
    let req = canonical_user_system_user(
        FrontendKind::AnthropicMessages,
        SystemSource::AnthropicSystem,
    );
    let body = agent_shim_providers::anthropic::request::build(&req, &anthropic_target());
    let msgs = body["messages"].as_array().expect("messages array present");
    assert_eq!(msgs.len(), 3, "all three turns preserved: {body}");
    assert_eq!(
        msgs[1]["role"], "system",
        "mid-position system stays at role:system"
    );
    assert_eq!(
        msgs[1]["content"][0]["text"], "mid system",
        "mid-position system content preserved verbatim"
    );
}

#[test]
fn anthropic_in_openai_chat_out_preserves_positional_system() {
    let req = canonical_user_system_user(
        FrontendKind::AnthropicMessages,
        SystemSource::AnthropicSystem,
    );
    let body = agent_shim_providers::oai_chat_wire::canonical_to_chat::build_json(
        &req,
        &openai_target(),
        false,
    );
    let msgs = body["messages"].as_array().expect("messages array present");
    assert_eq!(msgs.len(), 3, "all three turns preserved: {body}");
    assert_eq!(
        msgs[1]["role"], "system",
        "anthropic_system source maps to role:system on OpenAI Chat wire"
    );
    assert_eq!(
        msgs[1]["content"], "mid system",
        "mid-position system content preserved verbatim"
    );
}

#[test]
fn anthropic_in_openai_responses_out_downgrades_to_instructions() {
    let req = canonical_user_system_user(
        FrontendKind::AnthropicMessages,
        SystemSource::AnthropicSystem,
    );
    let body = agent_shim_providers::openai_compatible::responses_api::encode_request::build(
        &req,
        &openai_target(),
    );
    assert_eq!(
        body["instructions"], "mid system",
        "mid-position system folded into top-level instructions: {body}"
    );
    let input = body["input"].as_array().expect("input array present");
    for item in input {
        let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            role != "system" && role != "developer",
            "input must not contain system or developer roles after downgrade: {item}"
        );
    }
}

#[test]
fn anthropic_in_gemini_out_downgrades_to_system_instruction() {
    let req = canonical_user_system_user(
        FrontendKind::AnthropicMessages,
        SystemSource::AnthropicSystem,
    );
    let body = agent_shim_providers::gemini::request::build_json(&req, &gemini_target());
    let parts = body["systemInstruction"]["parts"]
        .as_array()
        .expect("systemInstruction.parts present");
    assert!(
        parts.iter().any(|p| p["text"] == "mid system"),
        "mid-position system folded into top-level systemInstruction: {body}"
    );
    let contents = body["contents"].as_array().expect("contents array present");
    for item in contents {
        let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
        assert_ne!(
            role, "system",
            "Gemini contents must not contain a 'system' role after downgrade: {item}"
        );
    }
}

#[test]
fn openai_chat_in_anthropic_out_preserves_positional_system() {
    let req = canonical_user_system_user(FrontendKind::OpenAiChat, SystemSource::OpenAiSystem);
    let body = agent_shim_providers::anthropic::request::build(&req, &anthropic_target());
    let msgs = body["messages"].as_array().expect("messages array present");
    assert_eq!(msgs.len(), 3, "all three turns preserved: {body}");
    assert_eq!(
        msgs[1]["role"], "system",
        "openai_system source maps to role:system on Anthropic wire"
    );
    assert_eq!(
        msgs[1]["content"][0]["text"], "mid system",
        "mid-position system content preserved verbatim"
    );
}
