//! Local token-count approximation for `/v1/messages/count_tokens`.
//!
//! Pure function over a `CanonicalRequest`. Uses `tiktoken-rs` cl100k_base
//! plus per-piece structural overhead constants. See
//! `docs/superpowers/specs/2026-05-06-count-tokens-design.md` for the
//! algorithm and the rationale behind each constant.

use agent_shim_core::{
    content::ContentBlock,
    message::Message,
    request::CanonicalRequest,
    tool::{ToolCallArguments, ToolChoice, ToolDefinition},
};
use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;

// Per-piece structural overhead. See spec §"Per-piece structural overhead".
const PER_MESSAGE: u32 = 4;
const PER_SYSTEM: u32 = 4;
const PER_TOOL_USE: u32 = 8;
const PER_TOOL_RESULT: u32 = 6;
const PER_REASONING: u32 = 4;
const PER_TOOL_DEF: u32 = 10;
const PER_TOOL_CHOICE: u32 = 6;
const PER_IMAGE: u32 = 200;

/// Count the approximate input tokens of a canonical request.
pub fn count(req: &CanonicalRequest) -> u32 {
    let mut total: u32 = 0;
    for sys in &req.system {
        total = total.saturating_add(PER_SYSTEM);
        for block in &sys.content {
            total = total.saturating_add(count_block(block));
        }
    }
    for msg in &req.messages {
        total = total.saturating_add(count_message(msg));
    }
    for tool in &req.tools {
        total = total.saturating_add(count_tool(tool));
    }
    total = total.saturating_add(count_tool_choice(&req.tool_choice));
    total
}

fn count_message(msg: &Message) -> u32 {
    let mut t = PER_MESSAGE;
    for block in &msg.content {
        t = t.saturating_add(count_block(block));
    }
    t
}

fn count_block(block: &ContentBlock) -> u32 {
    match block {
        ContentBlock::Text(b) => count_text(&b.text),
        ContentBlock::ToolCall(b) => {
            let args_text = match &b.arguments {
                ToolCallArguments::Complete { value } => {
                    serde_json::to_string(value).unwrap_or_default()
                }
                ToolCallArguments::Streaming { data } => data.clone(),
            };
            count_text(&b.name)
                .saturating_add(count_text(&args_text))
                .saturating_add(PER_TOOL_USE)
        }
        ContentBlock::ToolResult(b) => {
            let content_text = serde_json::to_string(&b.content).unwrap_or_default();
            count_text(&content_text).saturating_add(PER_TOOL_RESULT)
        }
        ContentBlock::Reasoning(b) => count_text(&b.text).saturating_add(PER_REASONING),
        ContentBlock::RedactedReasoning(b) => {
            let approx = (b.data.len() as u32) / 4;
            approx.saturating_add(PER_REASONING)
        }
        // Anthropic images decode into Unsupported(...) per
        // anthropic_messages::decode::inbound_block_to_canonical. Use the flat
        // image overhead — we don't have dimensions, and tokenizing the base64
        // blob would over-count by orders of magnitude.
        ContentBlock::Unsupported(_) => PER_IMAGE,
        // Native image/audio/file blocks aren't produced by the Anthropic
        // decoder today, but in case some path produces them, treat them as
        // image-equivalent rather than zero.
        ContentBlock::Image(_) | ContentBlock::Audio(_) | ContentBlock::File(_) => PER_IMAGE,
    }
}

fn count_tool(t: &ToolDefinition) -> u32 {
    let desc = t.description.as_deref().unwrap_or("");
    let schema = serde_json::to_string(&t.input_schema).unwrap_or_default();
    count_text(&t.name)
        .saturating_add(count_text(desc))
        .saturating_add(count_text(&schema))
        .saturating_add(PER_TOOL_DEF)
}

fn count_tool_choice(c: &ToolChoice) -> u32 {
    match c {
        ToolChoice::Auto => 0,
        ToolChoice::None | ToolChoice::Required => PER_TOOL_CHOICE,
        ToolChoice::Specific { name } => count_text(name).saturating_add(PER_TOOL_CHOICE),
    }
}

/// Count the cl100k_base tokens in a string. Uses `encode_ordinary` so
/// `<|...|>` literals in user content do not blow up.
fn count_text(s: &str) -> u32 {
    static CELL: OnceLock<CoreBPE> = OnceLock::new();
    let bpe = CELL
        .get_or_init(|| tiktoken_rs::cl100k_base().expect("cl100k_base tokenizer must initialize"));
    bpe.encode_ordinary(s).len() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        content::TextBlock,
        extensions::ExtensionMap,
        ids::RequestId,
        message::{Message, MessageRole},
        request::{CanonicalRequest, GenerationOptions, RequestMetadata},
        target::{FrontendInfo, FrontendKind, FrontendModel},
    };

    fn empty_request() -> CanonicalRequest {
        let model = FrontendModel("m".into());
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: model.clone(),
            },
            model,
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            generation: GenerationOptions::default(),
            response_format: None,
            stream: false,
            metadata: RequestMetadata::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: Default::default(),
            extensions: ExtensionMap::new(),
        }
    }

    fn user_text(s: &str) -> Message {
        Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text(TextBlock {
                text: s.into(),
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        }
    }

    #[test]
    fn empty_request_counts_zero() {
        let req = empty_request();
        assert_eq!(count(&req), 0);
    }

    #[test]
    fn single_user_text_message_equals_tokenizer_plus_per_message() {
        let mut req = empty_request();
        req.messages.push(user_text("hello world"));
        let expected = count_text("hello world") + PER_MESSAGE;
        assert_eq!(count(&req), expected);
    }

    #[test]
    fn three_messages_sum_to_three_per_message_overheads_plus_text() {
        let mut req = empty_request();
        req.messages.push(user_text("a"));
        req.messages.push(user_text("b"));
        req.messages.push(user_text("c"));
        let expected = count_text("a") + count_text("b") + count_text("c") + 3 * PER_MESSAGE;
        assert_eq!(count(&req), expected);
    }

    #[test]
    fn deterministic_same_input_same_output() {
        let mut req = empty_request();
        req.messages.push(user_text("the quick brown fox"));
        assert_eq!(count(&req), count(&req));
    }

    #[test]
    fn system_instruction_adds_per_system_overhead_plus_content() {
        use agent_shim_core::message::{SystemInstruction, SystemSource};
        let mut req = empty_request();
        req.system.push(SystemInstruction {
            source: SystemSource::AnthropicSystem,
            content: vec![ContentBlock::Text(TextBlock {
                text: "you are helpful".into(),
                extensions: ExtensionMap::new(),
            })],
        });
        let expected = count_text("you are helpful") + PER_SYSTEM;
        assert_eq!(count(&req), expected);
    }

    #[test]
    fn tool_use_block_counts_name_arguments_plus_overhead() {
        use agent_shim_core::{ids::ToolCallId, tool::{ToolCallBlock, ToolCallArguments}};
        let mut req = empty_request();
        let args = serde_json::json!({"q": "rust"});
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolCall(ToolCallBlock {
                id: ToolCallId::from_provider("call_1".to_string()),
                name: "search".into(),
                arguments: ToolCallArguments::Complete { value: args.clone() },
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let serialized = serde_json::to_string(&args).unwrap();
        let expected = count_text("search") + count_text(&serialized) + PER_TOOL_USE + PER_MESSAGE;
        assert_eq!(count(&req), expected);
    }

    #[test]
    fn tool_result_block_counts_serialized_content_plus_overhead() {
        use agent_shim_core::{ids::ToolCallId, tool::ToolResultBlock};
        let mut req = empty_request();
        let content = serde_json::json!("the result text");
        req.messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult(ToolResultBlock {
                tool_call_id: ToolCallId::from_provider("call_1".to_string()),
                content: content.clone(),
                is_error: false,
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let serialized = serde_json::to_string(&content).unwrap();
        let expected = count_text(&serialized) + PER_TOOL_RESULT + PER_MESSAGE;
        assert_eq!(count(&req), expected);
    }

    #[test]
    fn reasoning_block_counts_text_plus_overhead() {
        use agent_shim_core::content::ReasoningBlock;
        let mut req = empty_request();
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Reasoning(ReasoningBlock {
                text: "thinking aloud".into(),
                extensions: ExtensionMap::new(), // signature lives here, must NOT be counted
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let expected = count_text("thinking aloud") + PER_REASONING + PER_MESSAGE;
        assert_eq!(count(&req), expected);
    }

    #[test]
    fn redacted_reasoning_counts_blob_length_quarter_plus_overhead() {
        use agent_shim_core::content::RedactedReasoningBlock;
        let mut req = empty_request();
        let blob = "x".repeat(40);
        req.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::RedactedReasoning(RedactedReasoningBlock {
                data: blob.clone(),
                extensions: ExtensionMap::new(),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let expected = (blob.len() as u32 / 4) + PER_REASONING + PER_MESSAGE;
        assert_eq!(count(&req), expected);
    }

    #[test]
    fn image_unsupported_block_uses_flat_overhead_only() {
        use agent_shim_core::content::UnsupportedBlock;
        let mut req = empty_request();
        req.messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Unsupported(UnsupportedBlock {
                origin: "anthropic_messages".into(),
                raw: serde_json::json!({"type":"image","source":{"type":"base64","data":"aGVsbG8="}}),
            })],
            name: None,
            extensions: ExtensionMap::new(),
        });
        let expected = PER_IMAGE + PER_MESSAGE;
        assert_eq!(count(&req), expected);
    }

    #[test]
    fn tool_definition_counts_name_description_schema_plus_overhead() {
        let mut req = empty_request();
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "q": { "type": "string" } },
            "required": ["q"]
        });
        req.tools.push(ToolDefinition {
            name: "search".into(),
            description: Some("find things".into()),
            input_schema: schema.clone(),
            extensions: ExtensionMap::new(),
        });
        let serialized = serde_json::to_string(&schema).unwrap();
        let expected = count_text("search")
            + count_text("find things")
            + count_text(&serialized)
            + PER_TOOL_DEF;
        assert_eq!(count(&req), expected);
    }

    #[test]
    fn tool_definition_with_no_description_does_not_panic() {
        let mut req = empty_request();
        req.tools.push(ToolDefinition {
            name: "ping".into(),
            description: None,
            input_schema: serde_json::json!({}),
            extensions: ExtensionMap::new(),
        });
        // We just care it doesn't panic and includes the tool overhead.
        let n = count(&req);
        assert!(n >= PER_TOOL_DEF);
    }

    #[test]
    fn tool_choice_auto_adds_zero() {
        let mut req = empty_request();
        req.tool_choice = ToolChoice::Auto;
        assert_eq!(count(&req), 0);
    }

    #[test]
    fn tool_choice_required_adds_overhead() {
        let mut req = empty_request();
        req.tool_choice = ToolChoice::Required;
        assert_eq!(count(&req), PER_TOOL_CHOICE);
    }

    #[test]
    fn tool_choice_specific_adds_overhead_plus_tool_name() {
        let mut req = empty_request();
        req.tool_choice = ToolChoice::Specific {
            name: "search".into(),
        };
        let expected = count_text("search") + PER_TOOL_CHOICE;
        assert_eq!(count(&req), expected);
    }
}
