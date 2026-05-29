//! Per-`FrontendKind` `ImageTokenEstimator` selector. Plan v0.6.1 P04 (M-8).
//!
//! The gateway owns the inbound-protocol decision (it sees the route via
//! `FrontendKind`), so the per-frontend image-token math also lives here.
//! `select_image_estimator` returns a `&'static dyn ImageTokenEstimator`
//! of the matching shape; the admission layer threads it into the cost filter
//! so the cost-cap pre-pass charges image blocks at the same rate the
//! upstream will bill.
//!
//! `'static` is sound because every estimator is a unit struct (zero-sized,
//! trivially shareable across threads); we hand out references to module-
//! level constants and they never need lifetimes.
//!
//! # Coupling note
//!
//! Adding a new `FrontendKind` variant requires updating this selector —
//! the match below is intentionally exhaustive and not `_`-padded.
//! Documented in spec §7 risk table; this is the single point of truth
//! for "which estimator does this frontend use".

use agent_shim_core::cost::ImageTokenEstimator;
use agent_shim_core::target::FrontendKind;
use agent_shim_router::{AnthropicImageEstimator, OpenAiImageEstimator, ResponsesImageEstimator};

/// Unit-struct singletons live as `&'static` constants so the selector
/// can return `&'static dyn ImageTokenEstimator` without leaking
/// lifetimes into the rest of the gateway. Each is zero-sized.
const ANTHROPIC: &AnthropicImageEstimator = &AnthropicImageEstimator;
const OPENAI: &OpenAiImageEstimator = &OpenAiImageEstimator;
const RESPONSES: &ResponsesImageEstimator = &ResponsesImageEstimator;

/// Select the image-token estimator matching the inbound frontend.
///
/// The match is intentionally exhaustive (no `_` arm): adding a new
/// `FrontendKind` variant should force a compile error here so the
/// estimator dispatch is updated alongside the protocol surface.
pub fn select_image_estimator(frontend: FrontendKind) -> &'static dyn ImageTokenEstimator {
    match frontend {
        FrontendKind::AnthropicMessages => ANTHROPIC,
        FrontendKind::OpenAiChat => OPENAI,
        FrontendKind::OpenAiResponses => RESPONSES,
    }
}

#[cfg(test)]
mod tests {
    //! Pin the `FrontendKind` → estimator dispatch so a future refactor
    //! can't silently collapse two vendors to the same impl. Each test
    //! confirms the selector returns an estimator whose `Unknown`
    //! fallback matches the vendor's documented worst-case constant.

    use super::*;
    use agent_shim_core::cost::ImageSizeHint;

    #[test]
    fn anthropic_messages_selects_anthropic_estimator() {
        let est = select_image_estimator(FrontendKind::AnthropicMessages);
        // Anthropic worst-case from `image_estimators::anthropic`.
        assert_eq!(est.tokens_for_image(ImageSizeHint::Unknown), 1600);
    }

    #[test]
    fn openai_chat_selects_openai_estimator() {
        let est = select_image_estimator(FrontendKind::OpenAiChat);
        // OpenAI worst-case from `image_estimators::openai`.
        assert_eq!(est.tokens_for_image(ImageSizeHint::Unknown), 1105);
    }

    #[test]
    fn openai_responses_selects_responses_estimator() {
        // The Responses estimator currently delegates to OpenAI's
        // model-side math, so its Unknown fallback matches the
        // OpenAI single-image upper bound. The point of the test is
        // that the selector returns *something* — i.e. the match
        // arm exists and dispatch isn't `unreachable!()`.
        let est = select_image_estimator(FrontendKind::OpenAiResponses);
        let n = est.tokens_for_image(ImageSizeHint::Unknown);
        assert!(n > 0, "Responses Unknown fallback must be non-zero");
    }
}
