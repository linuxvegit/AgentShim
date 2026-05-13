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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageDetail {
    Low,
    High,
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
