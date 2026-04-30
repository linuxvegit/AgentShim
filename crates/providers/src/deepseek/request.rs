//! Build an outbound DeepSeek chat-completions request body.
//!
//! Today DeepSeek is wire-compatible with the OAI-Chat shape, so this module
//! is a thin wrapper around `oai_chat_wire::canonical_to_chat::build`. It lives
//! in its own file because Plan 02 T5 will introduce a `cache_control` strip
//! step (DeepSeek rejects Anthropic-style cache_control on messages); keeping
//! the seam visible avoids a future churn-y refactor.
//!
//! The wrapper returns an owned `serde_json::Value` so future post-processing
//! steps (T5) can mutate the body via `as_object_mut()` before it is sent.

use agent_shim_core::{BackendTarget, CanonicalRequest};

/// Build the outbound JSON body for a DeepSeek `/chat/completions` request.
///
/// Delegates to the shared OAI-Chat encoder. Future quirks (Plan 02 T5:
/// cache-control strip) will be applied here.
pub(crate) fn build(req: &CanonicalRequest, target: &BackendTarget) -> serde_json::Value {
    let body = crate::oai_chat_wire::canonical_to_chat::build(req, target);
    serde_json::to_value(&body).unwrap_or_default()
}
