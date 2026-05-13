//! Cost-related traits and types. Plan v0.6.1 P04 (M-8).
//!
//! `ImageTokenEstimator` lets the router crate dispatch per-frontend
//! image-token math without depending on the frontends crate (which
//! remains frozen against the v0.5.0 baseline). The gateway selects
//! the concrete impl per inbound `FrontendKind` and passes it through
//! `agent_shim_router::CostFilterInputs`.
//!
//! `ImageSizeHint::Unknown` exists because the canonical `ImageBlock`
//! (see `crate::content::ImageBlock`) does NOT record width/height —
//! they aren't always available on the wire. Implementations must
//! return a vendor-published worst-case count for `Unknown` so the
//! cost-cap filter stays conservative ("reject if it COULD cost more
//! than $X").

/// Per-frontend image-token cost estimator. Implementations live in the
/// router crate (e.g. `agent_shim_router::image_estimators`). The trait
/// itself lives here so router/gateway can refer to a stable abstraction
/// without dragging the frontends crate's dependency tree.
pub trait ImageTokenEstimator: Send + Sync {
    /// Estimate the number of input tokens that the upstream model will
    /// bill for this image. Conservative (upper-bound) when dimensions
    /// are unknown — see `ImageSizeHint::Unknown`.
    fn tokens_for_image(&self, hint: ImageSizeHint) -> u32;
}

/// Per-block size information available to the estimator.
///
/// The canonical `ImageBlock` does NOT record width/height (only the
/// `BinarySource`), so the estimator may receive `Unknown` and is
/// required to fall back to a vendor-published worst-case figure.
///
/// Marked `#[non_exhaustive]` so that future additions (e.g. a
/// `PartiallyKnown { content_type }` variant) don't break every
/// match arm in the workspace. Implementations must include a
/// wildcard arm that conservatively maps to `Unknown` semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImageSizeHint {
    /// Dimensions and detail level are both known (e.g. a future frontend
    /// adapter cached them at decode time and stashed in `extensions`).
    Known {
        width: u32,
        height: u32,
        detail: ImageDetail,
    },
    /// Dimensions unknown — return the vendor's worst-case per-image
    /// token count.
    Unknown,
}

/// Vendor "detail" level. Maps to OpenAI's `image_url.detail` field
/// and to Anthropic's documented low-/high-detail breakpoints. `Auto`
/// means the impl chooses (typically by treating it as `High` for
/// upper-bound estimation).
///
/// Marked `#[non_exhaustive]` so that future vendor pricing tiers
/// (e.g. an `Ultra` mode) can be added without breaking every match
/// arm in the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImageDetail {
    /// Vendor's lowest-cost detail mode. For OpenAI: a flat 85 tokens
    /// regardless of dimensions. For Anthropic: not separately priced;
    /// impls may treat as equivalent to `High` since the divisor is
    /// the same.
    Low,
    /// Vendor's full-detail / tile-math mode. For OpenAI: 85 base +
    /// 170 per 512×512 tile after rescaling. For Anthropic: the
    /// documented `(width × height) / divisor` math.
    High,
    /// Vendor chooses based on size. Estimators conservatively treat
    /// `Auto` as `High` for upper-bound cost-cap purposes; the live
    /// model may opt for `Low` at request time.
    Auto,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal in-crate impl used to confirm the trait shape.
    /// Production impls live in `agent_shim_router::image_estimators`.
    struct DummyEstimator;

    impl ImageTokenEstimator for DummyEstimator {
        fn tokens_for_image(&self, hint: ImageSizeHint) -> u32 {
            match hint {
                ImageSizeHint::Known { width, height, .. } => (width * height) / 1000,
                ImageSizeHint::Unknown => 1024, // sentinel worst-case
            }
        }
    }

    #[test]
    fn unknown_hint_uses_fallback() {
        let est = DummyEstimator;
        assert_eq!(est.tokens_for_image(ImageSizeHint::Unknown), 1024);
    }

    #[test]
    fn known_hint_uses_dimensions() {
        let est = DummyEstimator;
        let n = est.tokens_for_image(ImageSizeHint::Known {
            width: 1024,
            height: 1024,
            detail: ImageDetail::High,
        });
        assert_eq!(n, 1048); // (1024 * 1024) / 1000 = 1048
    }
}
