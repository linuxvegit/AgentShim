//! Per-frontend implementations of `agent_shim_core::cost::ImageTokenEstimator`.
//! Plan v0.6.1 P04 (M-8).
//!
//! Each unit struct is a stateless dispatcher to vendor-specific token math.
//! See each impl's doc-comment for the source-of-truth URL. The token tables
//! are pinned at v0.6.1 release; later drift is tracked in the impl's
//! `#[cfg(test)] mod tests` fixture rows.

mod anthropic;
mod openai;
mod responses;

pub use anthropic::AnthropicImageEstimator;
pub use openai::OpenAiImageEstimator;
pub use responses::ResponsesImageEstimator;

#[cfg(test)]
mod cross_vendor_tests {
    //! Cross-vendor invariants: locks in that the gateway's
    //! `FrontendKind`-keyed dispatch is non-trivial. If a future
    //! refactor accidentally collapses two vendors to the same
    //! Unknown-fallback constant, this test breaks.

    use super::*;
    use agent_shim_core::cost::{ImageSizeHint, ImageTokenEstimator};

    #[test]
    fn unknown_fallback_differs_across_vendors() {
        let anth = AnthropicImageEstimator.tokens_for_image(ImageSizeHint::Unknown);
        let oai = OpenAiImageEstimator.tokens_for_image(ImageSizeHint::Unknown);
        assert_ne!(
            anth, oai,
            "Anthropic ({anth}) and OpenAI ({oai}) Unknown fallbacks must differ — \
             selector dispatch is meaningful only if the impls disagree on at least one shape"
        );
    }
}
