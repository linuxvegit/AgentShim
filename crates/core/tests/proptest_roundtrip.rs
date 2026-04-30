use agent_shim_core::ids::RequestId;
use agent_shim_core::{
    CanonicalRequest, ContentBlock, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel,
    GenerationOptions, Message, MessageRole, RequestMetadata, SystemInstruction, SystemSource,
    TextBlock,
};
use proptest::prelude::*;

fn arb_text_block() -> impl Strategy<Value = TextBlock> {
    ".*".prop_map(|text| TextBlock {
        text,
        extensions: ExtensionMap::new(),
    })
}

fn arb_content_block() -> impl Strategy<Value = ContentBlock> {
    arb_text_block().prop_map(ContentBlock::Text)
}

fn arb_message() -> impl Strategy<Value = Message> {
    let role = prop_oneof![Just(MessageRole::User), Just(MessageRole::Assistant)];
    let content = prop::collection::vec(arb_content_block(), 0..4);
    (role, content).prop_map(|(role, content)| Message {
        role,
        content,
        name: None,
        extensions: ExtensionMap::new(),
    })
}

fn arb_system_instruction() -> impl Strategy<Value = SystemInstruction> {
    prop::collection::vec(arb_content_block(), 0..2).prop_map(|content| SystemInstruction {
        source: SystemSource::AnthropicSystem,
        content,
    })
}

fn arb_request() -> impl Strategy<Value = CanonicalRequest> {
    let messages = prop::collection::vec(arb_message(), 1..5);
    let system = prop::collection::vec(arb_system_instruction(), 0..2);
    (messages, system).prop_map(|(messages, system)| CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::AnthropicMessages,
            requested_model: FrontendModel::from("claude-3-5-sonnet-20241022"),
        },
        model: FrontendModel::from("claude-3-5-sonnet-20241022"),
        system,
        messages,
        tools: vec![],
        tool_choice: Default::default(),
        generation: GenerationOptions::default(),
        response_format: None,
        stream: false,
        metadata: RequestMetadata::default(),
        inbound_anthropic_headers: vec![],
        resolved_policy: Default::default(),
        extensions: ExtensionMap::new(),
    })
}

proptest! {
    #[test]
    fn canonical_request_json_round_trip(req in arb_request()) {
        let json = serde_json::to_string(&req).expect("serialize");
        let back: CanonicalRequest = serde_json::from_str(&json).expect("deserialize");
        let json2 = serde_json::to_string(&back).expect("re-serialize");
        prop_assert_eq!(json, json2);
    }
}
