//! Build an outbound GLM chat-completions request body.
//! Filled in by Task 5.

use agent_shim_core::{BackendTarget, CanonicalRequest};

use crate::ProviderError;

pub(crate) fn build(
    _req: &CanonicalRequest,
    _target: &BackendTarget,
    _accepts_xhigh: bool,
) -> Result<serde_json::Value, ProviderError> {
    unimplemented!("filled in Task 5")
}
