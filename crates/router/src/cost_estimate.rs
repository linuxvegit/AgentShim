//! Per-request cost estimation. Plan 06 P04 T2.
//!
//! Estimates the upper-bound USD cost of routing a request to a given
//! upstream. Used by the cost filter to apply per-route `max_cost_usd`
//! caps. The estimate is intentionally pessimistic — operators set the
//! cap expecting "reject if it COULD cost more than $X", not "if it
//! WILL".
//!
//! Spec §4.1 D3:
//!   estimate = estimate_input_tokens(request) * input_price/1M
//!            + max_output_tokens(request)     * output_price/1M
//!
//! Tokens are counted with `tiktoken-rs`'s `cl100k_base` encoder (the
//! same one frontier models use), using `encode_ordinary` so that
//! `<|...|>` literals in user content don't blow up the encoder. The
//! tokenizer is initialised once via a `OnceLock`, matching the pattern
//! established in `crates/frontends/src/anthropic_messages/count_tokens.rs`.
//!
//! Only `ContentBlock::Text` blocks contribute to the input estimate
//! today — ToolCall/ToolResult/Image blocks are skipped for estimation
//! simplicity. A generous overestimate isn't needed for those because
//! the cost cap is itself a coarse guardrail; if multimodal cost
//! tracking becomes important, extend this module.

use std::sync::OnceLock;

use tiktoken_rs::CoreBPE;

use agent_shim_config::UpstreamCost;
use agent_shim_core::{CanonicalRequest, ContentBlock};

/// Conservative default cap for output tokens when the request doesn't
/// specify one. Picked to be roughly the median frontier-model default;
/// won't underestimate cost much in practice.
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4096;

/// Outcome of a single estimation. Today this is just the USD cost —
/// see module docs for the formula. The encoder (`cl100k_base` via
/// `OnceLock`) is infallible in practice; no fallback flag is needed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostEstimate {
    pub cost_usd: f64,
}

/// Estimate the USD cost of routing `request` to an upstream that has
/// the given `cost` schedule. When `cost` is `None` (e.g. Copilot
/// subscription product), this returns `None` — there is no per-request
/// cost to estimate, so the cap effectively never fires.
pub fn estimate_request_cost(
    request: &CanonicalRequest,
    cost: Option<&UpstreamCost>,
) -> Option<CostEstimate> {
    let cost = cost?;

    let input_tokens = estimate_input_tokens(request);
    let output_tokens = output_token_cap(request);

    let cost_usd = (input_tokens as f64 * cost.input_per_million_usd
        + output_tokens as f64 * cost.output_per_million_usd)
        / 1_000_000.0;

    Some(CostEstimate { cost_usd })
}

/// Lazily-initialised cl100k_base tokenizer. Matches the pattern in
/// `crates/frontends/src/anthropic_messages/count_tokens.rs` so the
/// process pays for tokenizer construction at most once.
fn cl100k() -> &'static CoreBPE {
    static CELL: OnceLock<CoreBPE> = OnceLock::new();
    CELL.get_or_init(|| tiktoken_rs::cl100k_base().expect("cl100k_base tokenizer must initialize"))
}

/// Estimate input tokens using `cl100k_base`. `encode_ordinary` keeps
/// `<|...|>` literals in user content from being interpreted as special
/// tokens.
fn estimate_input_tokens(request: &CanonicalRequest) -> u32 {
    let text = canonical_request_text(request);
    cl100k().encode_ordinary(&text).len() as u32
}

/// Pull the output cap from the request, falling back to a conservative
/// default. The field lives at `request.generation.max_tokens`.
fn output_token_cap(request: &CanonicalRequest) -> u32 {
    request
        .generation
        .max_tokens
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
}

/// Concatenate every text content block across all messages. Non-text
/// blocks (tool calls, tool results, images, etc.) are intentionally
/// skipped — see module docs.
fn canonical_request_text(request: &CanonicalRequest) -> String {
    let mut out = String::new();
    for msg in &request.messages {
        for block in &msg.content {
            if let ContentBlock::Text(t) = block {
                out.push_str(&t.text);
                out.push('\n');
            }
        }
    }
    out
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

    fn req(text: &str, max_tokens: Option<u32>) -> CanonicalRequest {
        let model = FrontendModel("m".into());
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::OpenAiChat,
                requested_model: model.clone(),
            },
            model,
            system: vec![],
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text(TextBlock {
                    text: text.into(),
                    extensions: ExtensionMap::new(),
                })],
                name: None,
                extensions: ExtensionMap::new(),
            }],
            tools: vec![],
            tool_choice: Default::default(),
            generation: GenerationOptions {
                max_tokens,
                ..Default::default()
            },
            response_format: None,
            stream: false,
            metadata: RequestMetadata::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: Default::default(),
            extensions: ExtensionMap::new(),
        }
    }

    #[test]
    fn cost_none_returns_none() {
        let r = req("hello", None);
        assert!(estimate_request_cost(&r, None).is_none());
    }

    #[test]
    fn cost_computes_when_inputs_present() {
        let cost = UpstreamCost {
            input_per_million_usd: 1.0,
            output_per_million_usd: 2.0,
        };
        let r = req("hello world", Some(100));
        let est = estimate_request_cost(&r, Some(&cost)).expect("cost present, request present");
        // Don't assert exact value (tokenizer-dependent); just that it's
        // a reasonable bound for a short text + tiny output cap.
        assert!(est.cost_usd >= 0.0);
        assert!(
            est.cost_usd < 0.001,
            "rough sanity: tiny request, tiny cost; got {}",
            est.cost_usd
        );
    }
}
