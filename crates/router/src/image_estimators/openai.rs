//! OpenAI image-token estimator. Plan v0.6.1 P04 (M-8).
//!
//! Source: OpenAI's documented Vision pricing for `image_url.detail`:
//! - `low`: flat 85 tokens regardless of dimensions
//! - `high`: 85 + 170 × ceil(width/512) × ceil(height/512), after the
//!   model resizes the image to fit a 2048x2048 box and rescales to
//!   768 px on the short side.
//! - `auto`: treated as `high` here (upper-bound semantics for the
//!   cost-cap filter; the live model may opt for `low` based on size).
//!
//! Unknown-dimension fallback returns the documented high-detail cost
//! of a 2048x2048 image (1105 tokens) — OpenAI's published upper bound
//! for a single image.

use agent_shim_core::cost::{ImageDetail, ImageSizeHint, ImageTokenEstimator};

/// Stateless dispatcher to OpenAI's image-token math.
pub struct OpenAiImageEstimator;

const OPENAI_LOW_DETAIL_TOKENS: u32 = 85;
const OPENAI_HIGH_BASE_TOKENS: u32 = 85;
const OPENAI_HIGH_PER_TILE_TOKENS: u32 = 170;
const OPENAI_TILE_PIXELS: u32 = 512;
const OPENAI_UNKNOWN_TOKENS: u32 = 1105;

/// Compute high-detail tile-math cost for given dimensions. Used by both
/// the High/Auto match arms and the future-variant wildcard fallback.
fn high_detail_tokens(width: u32, height: u32) -> u32 {
    let tiles_w = width.div_ceil(OPENAI_TILE_PIXELS);
    let tiles_h = height.div_ceil(OPENAI_TILE_PIXELS);
    let tiles = tiles_w.saturating_mul(tiles_h);
    OPENAI_HIGH_BASE_TOKENS.saturating_add(OPENAI_HIGH_PER_TILE_TOKENS.saturating_mul(tiles))
}

impl ImageTokenEstimator for OpenAiImageEstimator {
    fn tokens_for_image(&self, hint: ImageSizeHint) -> u32 {
        match hint {
            ImageSizeHint::Unknown => OPENAI_UNKNOWN_TOKENS,
            ImageSizeHint::Known {
                width,
                height,
                detail,
            } => match detail {
                ImageDetail::Low => OPENAI_LOW_DETAIL_TOKENS,
                ImageDetail::High | ImageDetail::Auto => high_detail_tokens(width, height),
                // Forward-compat: future ImageDetail variants use
                // high-detail math as the conservative upper bound.
                // `ImageDetail` is `#[non_exhaustive]` upstream, so
                // this arm is required for exhaustiveness.
                _ => high_detail_tokens(width, height),
            },
            // Forward-compat: future ImageSizeHint variants conservatively
            // map to the Unknown-fallback worst case. `ImageSizeHint` is
            // `#[non_exhaustive]` upstream, so this arm is required for
            // exhaustiveness; it is NOT dead code today.
            _ => OPENAI_UNKNOWN_TOKENS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_returns_documented_worst_case() {
        let est = OpenAiImageEstimator;
        assert_eq!(est.tokens_for_image(ImageSizeHint::Unknown), 1105);
    }

    #[test]
    fn low_detail_flat_regardless_of_size() {
        let est = OpenAiImageEstimator;
        let tokens = est.tokens_for_image(ImageSizeHint::Known {
            width: 4096,
            height: 4096,
            detail: ImageDetail::Low,
        });
        assert_eq!(tokens, 85);
    }

    #[test]
    fn high_detail_tile_math_matches_documented_examples() {
        let est = OpenAiImageEstimator;
        // 1024x1024 in high-detail mode: 2x2 tiles = 4 tiles
        // Expected: 85 + (170 * 4) = 765
        let tokens = est.tokens_for_image(ImageSizeHint::Known {
            width: 1024,
            height: 1024,
            detail: ImageDetail::High,
        });
        assert_eq!(tokens, 765);

        // 512x512 high-detail: 1x1 tile
        // Expected: 85 + 170 = 255
        let tokens = est.tokens_for_image(ImageSizeHint::Known {
            width: 512,
            height: 512,
            detail: ImageDetail::High,
        });
        assert_eq!(tokens, 255);

        // Auto is treated as high
        let tokens_auto = est.tokens_for_image(ImageSizeHint::Known {
            width: 1024,
            height: 1024,
            detail: ImageDetail::Auto,
        });
        assert_eq!(tokens_auto, 765);
    }
}
