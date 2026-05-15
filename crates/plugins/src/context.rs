//! Request-level context handed to every plugin hook. Carries
//! request_id, frontend kind, and a route label. Does **not** carry
//! the plugin's own name — that lives in the `invoke()` template
//! locally and only enters logs / spans. (Plan 07 Q4.)

use agent_shim_core::{FrontendKind, RequestId, Usage};

/// Per-request metadata carried into every plugin hook.
///
/// `route_label` is the canonical string `<frontend_kind>/<model_alias>`
/// used by the rate-limit registry and metrics; plugins can read it for
/// logging/audit purposes but should not parse it.
#[derive(Debug, Clone)]
pub struct PluginContext {
    pub request_id: RequestId,
    pub frontend: FrontendKind,
    pub route_label: String,
    // Future-proofing: when this struct grows, add fields here. The
    // type is constructed only inside this crate's registry and the
    // pipeline; plugin code reads through `&PluginContext` so adding
    // fields is non-breaking.
}

/// Summary of a completed request. Handed to `Plugin::on_response_complete`
/// (H7). Carries only what is safe to read after the request finishes —
/// the underlying `CanonicalRequest` is dropped by then.
#[derive(Debug, Clone)]
pub struct ResponseSummary {
    pub usage: Option<Usage>,
    pub elapsed_ms: u64,
    pub upstream_status: UpstreamStatus,
}

/// Why the upstream call ended. Sufficient detail for observation
/// purposes; refined error categorisation is intentionally absent so
/// plugins don't drift into trying to retry or rebrand failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamStatus {
    Success,
    Error,
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{FrontendKind, RequestId};

    #[test]
    fn plugin_context_is_clone() {
        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "anthropic_messages/claude-sonnet".to_string(),
        };
        let _ = ctx.clone(); // compile-time check
    }

    #[test]
    fn response_summary_holds_optional_usage() {
        let summary = ResponseSummary {
            usage: None,
            elapsed_ms: 42,
            upstream_status: UpstreamStatus::Success,
        };
        assert_eq!(summary.elapsed_ms, 42);
        assert_eq!(summary.upstream_status, UpstreamStatus::Success);
        assert!(summary.usage.is_none());
    }
}
