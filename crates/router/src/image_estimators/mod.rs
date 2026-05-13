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
