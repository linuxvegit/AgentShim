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
    tool::{ToolChoice, ToolDefinition},
};
use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;

// Per-piece structural overhead. See spec §"Per-piece structural overhead".
const PER_MESSAGE: u32 = 4;
const PER_SYSTEM: u32 = 4;
#[allow(dead_code)]
const PER_TOOL_USE: u32 = 8;
#[allow(dead_code)]
const PER_TOOL_RESULT: u32 = 6;
#[allow(dead_code)]
const PER_REASONING: u32 = 4;
#[allow(dead_code)]
const PER_TOOL_DEF: u32 = 10;
#[allow(dead_code)]
const PER_TOOL_CHOICE: u32 = 6;
#[allow(dead_code)]
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
        _ => 0, // implemented in later tasks
    }
}

fn count_tool(_t: &ToolDefinition) -> u32 {
    0 // implemented in a later task
}

fn count_tool_choice(_c: &ToolChoice) -> u32 {
    0 // implemented in a later task
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
}
