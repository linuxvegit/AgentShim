//! Integration test for image-aware cost estimation. Plan v0.6.1 P04 (M-8).
//!
//! Builds a `CanonicalRequest` with mixed text + image content blocks and
//! verifies the estimate accounts for both contributions. Also pins the
//! cross-vendor invariant that swapping the estimator changes the result —
//! making the gateway's `FrontendKind`-keyed selector observable end-to-end.

use agent_shim_config::UpstreamCost;
use agent_shim_core::content::{ImageBlock, TextBlock};
use agent_shim_core::extensions::ExtensionMap;
use agent_shim_core::ids::RequestId;
use agent_shim_core::media::BinarySource;
use agent_shim_core::message::{Message, MessageRole};
use agent_shim_core::request::{CanonicalRequest, GenerationOptions, RequestMetadata};
use agent_shim_core::target::{FrontendInfo, FrontendKind, FrontendModel};
use agent_shim_core::ContentBlock;
use agent_shim_router::{estimate_request_cost, AnthropicImageEstimator, OpenAiImageEstimator};

/// Build a request with a single user message containing the supplied text
/// followed by one image block (URL-sourced; no width/height — the estimator
/// must fall back to its `Unknown` worst-case figure).
fn req_with_text_and_image(text: &str) -> CanonicalRequest {
    let model = FrontendModel("m".into());
    CanonicalRequest {
        id: RequestId::new(),
        frontend: FrontendInfo {
            kind: FrontendKind::AnthropicMessages,
            requested_model: model.clone(),
        },
        model,
        system: vec![],
        messages: vec![Message {
            role: MessageRole::User,
            content: vec![
                ContentBlock::Text(TextBlock {
                    text: text.into(),
                    extensions: ExtensionMap::new(),
                }),
                ContentBlock::Image(ImageBlock {
                    source: BinarySource::Url {
                        url: "https://example.com/img.png".into(),
                    },
                    extensions: ExtensionMap::new(),
                }),
            ],
            name: None,
            extensions: ExtensionMap::new(),
        }],
        tools: vec![],
        tool_choice: Default::default(),
        generation: GenerationOptions {
            max_tokens: Some(100),
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
fn mixed_text_and_image_estimate_includes_both_contributions() {
    let cost = UpstreamCost {
        input_per_million_usd: 3.0,
        output_per_million_usd: 15.0,
    };

    // Text-only baseline: drop the image block from the same request shape.
    let text_only_req = {
        let mut r = req_with_text_and_image("hello world");
        r.messages[0].content.pop();
        r
    };
    let text_only = estimate_request_cost(&text_only_req, Some(&cost), &AnthropicImageEstimator)
        .expect("text-only cost present");

    // Mixed text + image — same text content, one extra image block.
    let mixed_req = req_with_text_and_image("hello world");
    let mixed = estimate_request_cost(&mixed_req, Some(&cost), &AnthropicImageEstimator)
        .expect("mixed cost present");

    assert!(
        mixed.cost_usd > text_only.cost_usd,
        "image block must increase the estimate: text_only={}, mixed={}",
        text_only.cost_usd,
        mixed.cost_usd
    );

    // The increase must equal the AnthropicImageEstimator's Unknown
    // fallback (1600 tokens) × input_per_million_usd / 1M.
    let expected_delta = 1600.0 * 3.0 / 1_000_000.0;
    let observed_delta = mixed.cost_usd - text_only.cost_usd;
    let tolerance = 0.000_001;
    assert!(
        (observed_delta - expected_delta).abs() < tolerance,
        "delta should be {expected_delta} but was {observed_delta}"
    );
}

#[test]
fn openai_estimator_uses_different_unknown_baseline() {
    let cost = UpstreamCost {
        input_per_million_usd: 3.0,
        output_per_million_usd: 15.0,
    };
    let req = req_with_text_and_image("hello world");
    let anth = estimate_request_cost(&req, Some(&cost), &AnthropicImageEstimator)
        .expect("anthropic cost present");
    let oai = estimate_request_cost(&req, Some(&cost), &OpenAiImageEstimator)
        .expect("openai cost present");
    // Anthropic Unknown = 1600 tokens; OpenAI Unknown = 1105 tokens.
    // OpenAI estimate must be lower because the only differing factor
    // between the two evaluations is the image-token contribution.
    assert!(
        oai.cost_usd < anth.cost_usd,
        "OpenAI Unknown ({}) should be cheaper than Anthropic Unknown ({}); \
         got openai={} anthropic={}",
        1105,
        1600,
        oai.cost_usd,
        anth.cost_usd,
    );
}
