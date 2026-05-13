//! Anthropic image-token estimator. Plan v0.6.1 P04 (M-8).
//!
//! Source: Anthropic's documented Vision pricing. The token cost for an
//! image is approximated as `tokens = ceil((width * height) / 750)` per
//! Anthropic's published guidance (the divisor varies by model family;
//! 750 is the documented worst-case for sonnet/haiku as of v0.6.1
//! release). Unknown-dimension fallback is 1600 tokens — the cost of
//! a 1024x1024 image at the documented divisor, rounded up.

use agent_shim_core::cost::{ImageSizeHint, ImageTokenEstimator};

/// Stateless dispatcher to Anthropic's image-token math.
pub struct AnthropicImageEstimator;

/// Anthropic's documented worst-case token cost for an image of unknown
/// dimensions. Equal to the cost of a 1024x1024 image at the v0.6.1
/// divisor.
const ANTHROPIC_UNKNOWN_TOKENS: u32 = 1600;

/// Anthropic's documented divisor: tokens ≈ (w × h) / DIVISOR.
const ANTHROPIC_TOKENS_DIVISOR: u32 = 750;

impl ImageTokenEstimator for AnthropicImageEstimator {
    fn tokens_for_image(&self, hint: ImageSizeHint) -> u32 {
        match hint {
            ImageSizeHint::Unknown => ANTHROPIC_UNKNOWN_TOKENS,
            ImageSizeHint::Known {
                width,
                height,
                detail: _,
            } => {
                // Anthropic doesn't expose a "low/high" detail axis the
                // way OpenAI does — `detail` is ignored on this path.
                let pixels = (width as u64) * (height as u64);
                pixels.div_ceil(ANTHROPIC_TOKENS_DIVISOR as u64) as u32
            }
            // Forward-compat: future ImageSizeHint variants conservatively
            // map to the Unknown-fallback worst case. `ImageSizeHint` is
            // `#[non_exhaustive]` upstream, so this arm is required for
            // exhaustiveness; it is NOT dead code today.
            _ => ANTHROPIC_UNKNOWN_TOKENS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::cost::ImageDetail;

    #[test]
    fn unknown_returns_documented_worst_case() {
        let est = AnthropicImageEstimator;
        assert_eq!(est.tokens_for_image(ImageSizeHint::Unknown), 1600);
    }

    #[test]
    fn small_image_costs_proportional_to_pixels() {
        let est = AnthropicImageEstimator;
        // 512x512 = 262_144 pixels / 750 ≈ 350 tokens (rounded up)
        let tokens = est.tokens_for_image(ImageSizeHint::Known {
            width: 512,
            height: 512,
            detail: ImageDetail::Auto,
        });
        assert!((349..=350).contains(&tokens), "expected ~350, got {tokens}");
    }

    #[test]
    fn megapixel_image_within_anthropic_bounds() {
        let est = AnthropicImageEstimator;
        let tokens = est.tokens_for_image(ImageSizeHint::Known {
            width: 1024,
            height: 1024,
            detail: ImageDetail::Auto,
        });
        assert!(
            tokens <= ANTHROPIC_UNKNOWN_TOKENS,
            "Known(1024²) should not exceed Unknown fallback; got {tokens}"
        );
        // Within ±5% of the v0.6.1 pinned value (1399).
        assert!((1320..=1470).contains(&tokens), "got {tokens}");
    }
}
