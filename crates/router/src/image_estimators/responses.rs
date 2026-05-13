//! Responses-frontend image-token estimator. Plan v0.6.1 P04 (M-8).
//!
//! The Responses API is OpenAI-shaped; its image-token math mirrors
//! OpenAI's Vision pricing exactly. We define this as its own type
//! rather than reusing `OpenAiImageEstimator` so the gateway's
//! `FrontendKind`-keyed dispatch is explicit and pricing can diverge
//! later without surprising the caller.

use agent_shim_core::cost::{ImageSizeHint, ImageTokenEstimator};

use super::openai::OpenAiImageEstimator;

/// Stateless dispatcher; delegates to `OpenAiImageEstimator`.
pub struct ResponsesImageEstimator;

impl ImageTokenEstimator for ResponsesImageEstimator {
    fn tokens_for_image(&self, hint: ImageSizeHint) -> u32 {
        OpenAiImageEstimator.tokens_for_image(hint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::cost::ImageDetail;

    /// Parity: every input shape produces the same output as
    /// OpenAiImageEstimator. If pricing diverges in the future,
    /// remove the parity test and replace with Responses-specific
    /// fixture rows.
    #[test]
    fn parity_with_openai() {
        let resp = ResponsesImageEstimator;
        let openai = OpenAiImageEstimator;

        for hint in [
            ImageSizeHint::Unknown,
            ImageSizeHint::Known {
                width: 512,
                height: 512,
                detail: ImageDetail::Low,
            },
            ImageSizeHint::Known {
                width: 512,
                height: 512,
                detail: ImageDetail::High,
            },
            ImageSizeHint::Known {
                width: 1024,
                height: 1024,
                detail: ImageDetail::Auto,
            },
            ImageSizeHint::Known {
                width: 4096,
                height: 4096,
                detail: ImageDetail::High,
            },
        ] {
            assert_eq!(
                resp.tokens_for_image(hint),
                openai.tokens_for_image(hint),
                "Responses estimator must match OpenAI for hint {hint:?}"
            );
        }
    }
}
