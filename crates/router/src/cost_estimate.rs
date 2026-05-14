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
//! Text blocks contribute to the input estimate via the `cl100k_base`
//! tokenizer. Image blocks are accounted for via the per-frontend
//! `ImageTokenEstimator` injected by the caller — see
//! `agent_shim_core::cost::ImageTokenEstimator` (Plan v0.6.1 P04, M-8).
//! ToolCall/ToolResult blocks remain skipped: their wire form is small
//! enough that the conservative per-image worst-case dwarfs them.

use agent_shim_config::UpstreamCost;
use agent_shim_core::cost::{ImageSizeHint, ImageTokenEstimator};
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
///
/// `image_estimator` supplies the per-frontend image-token math. Plan
/// v0.6.1 P04 (M-8): this is dispatched by the gateway from the
/// inbound `FrontendKind` so the cost cap stays consistent with what
/// the upstream will actually bill.
pub fn estimate_request_cost(
    request: &CanonicalRequest,
    cost: Option<&UpstreamCost>,
    image_estimator: &dyn ImageTokenEstimator,
) -> Option<CostEstimate> {
    let cost = cost?;

    let text_tokens = estimate_text_tokens(request);
    let image_tokens = estimate_image_tokens(request, image_estimator);
    let input_tokens = text_tokens.saturating_add(image_tokens);
    let output_tokens = output_token_cap(request);

    let cost_usd = (input_tokens as f64 * cost.input_per_million_usd
        + output_tokens as f64 * cost.output_per_million_usd)
        / 1_000_000.0;

    Some(CostEstimate { cost_usd })
}

/// Returns the cl100k_base encoder. Delegates to
/// `agent_shim_tokens::cl100k_encoder` — single workspace-wide source
/// of truth, matching the pattern in
/// `crates/frontends/src/anthropic_messages/count_tokens.rs`. (P01 T4.)
fn cl100k() -> &'static tiktoken_rs::CoreBPE {
    agent_shim_tokens::cl100k_encoder()
}

/// Estimate input text tokens using `cl100k_base`. `encode_ordinary`
/// keeps `<|...|>` literals in user content from being interpreted as
/// special tokens.
fn estimate_text_tokens(request: &CanonicalRequest) -> u32 {
    let text = canonical_request_text(request);
    cl100k().encode_ordinary(&text).len() as u32
}

/// Estimate input image tokens by walking every message's content
/// blocks and asking the supplied `ImageTokenEstimator` to price each
/// `Image` block. The canonical `ImageBlock` (see
/// `agent_shim_core::content::ImageBlock`) does NOT carry width/height,
/// so the estimator always receives `ImageSizeHint::Unknown` here and
/// falls back to a vendor-published worst-case figure. Plan v0.6.1
/// P04 (M-8).
fn estimate_image_tokens(
    request: &CanonicalRequest,
    image_estimator: &dyn ImageTokenEstimator,
) -> u32 {
    let mut tokens: u32 = 0;
    for msg in &request.messages {
        for block in &msg.content {
            if matches!(block, ContentBlock::Image(_)) {
                tokens =
                    tokens.saturating_add(image_estimator.tokens_for_image(ImageSizeHint::Unknown));
            }
        }
    }
    tokens
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

    /// Test-only image-token estimator that returns zero tokens for every
    /// image. The pre-existing v0.6.0 unit tests don't include image
    /// blocks, so this is behaviourally identical to the old 2-arg
    /// signature for them — keeps the test bodies focused on the text
    /// path. Image-aware behaviour is covered by the integration tests
    /// in `crates/router/tests/cost_estimate_image.rs`.
    struct NoImagesEstimator;

    impl ImageTokenEstimator for NoImagesEstimator {
        fn tokens_for_image(&self, _hint: ImageSizeHint) -> u32 {
            0
        }
    }

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
        assert!(estimate_request_cost(&r, None, &NoImagesEstimator).is_none());
    }

    #[test]
    fn cost_computes_when_inputs_present() {
        let cost = UpstreamCost {
            input_per_million_usd: 1.0,
            output_per_million_usd: 2.0,
        };
        let r = req("hello world", Some(100));
        let est = estimate_request_cost(&r, Some(&cost), &NoImagesEstimator)
            .expect("cost present, request present");
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
