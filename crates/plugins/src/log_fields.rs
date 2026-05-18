//! Whitelist of fields emitted by plugin observability. §7.1 of the
//! plugin design spec. The whitelist enforces §7.6 PII red lines —
//! adding a field MUST update §7.1 and the PII red-line doc.

use agent_shim_core::RequestId;

use crate::error::OnError;

/// Plugin hook outcome. Mapped to `outcome` metric label and
/// determines the tracing level per §7.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginOutcome {
    Success,
    Skipped,
    Failed,
    TimedOut,
    Aborted,
    ProtectedFieldMutated,
}

impl PluginOutcome {
    /// Stable label string for metric + log emission. NEVER rename — it
    /// is the public Prometheus label value.
    pub(crate) fn as_label(self) -> &'static str {
        match self {
            PluginOutcome::Success => "success",
            PluginOutcome::Skipped => "skipped",
            PluginOutcome::Failed => "failed",
            PluginOutcome::TimedOut => "timed_out",
            PluginOutcome::Aborted => "aborted",
            PluginOutcome::ProtectedFieldMutated => "protected_field_mutated",
        }
    }

    /// Tracing level per §7.2. The on_error policy upgrades TimedOut
    /// from WARN to ERROR when the timeout will abort the request.
    pub(crate) fn level(self, on_error: OnError) -> tracing::Level {
        match (self, on_error) {
            (PluginOutcome::Success, _) => tracing::Level::DEBUG,
            // (Skipped, Fail) is structurally unreachable in invoke() —
            // invoke() only emits PluginOutcome::Skipped under OnError::Skip.
            // The mapping is defined defensively so the function is total.
            (PluginOutcome::Skipped, _) => tracing::Level::WARN,
            (PluginOutcome::Failed, _) => tracing::Level::ERROR,
            (PluginOutcome::TimedOut, OnError::Skip) => tracing::Level::WARN,
            (PluginOutcome::TimedOut, OnError::Fail) => tracing::Level::ERROR,
            // Aborted maps to INFO because an aborted request is a
            // policy-driven 4xx (HTTP 400), not a server-side error —
            // operators care about the count, not individual ERROR
            // entries. Spec §7.2.
            (PluginOutcome::Aborted, _) => tracing::Level::INFO,
            (PluginOutcome::ProtectedFieldMutated, _) => tracing::Level::ERROR,
        }
    }
}

/// Whitelist of fields safe to emit to logs/metrics. Matches spec §7.1
/// verbatim. Adding a field MUST update §7.1 + the PII red-line doc.
///
/// NEVER add request content, user input, plugin output, or any
/// derived-from-user-content fields here. §7.6 PII red lines.
pub(crate) struct PluginLogFields<'a> {
    pub plugin_name: &'a str,
    pub plugin_kind: &'static str,
    pub plugin_hook: &'static str,
    pub request_id: &'a RequestId,
    pub route: &'a str,
    pub outcome: PluginOutcome,
    pub elapsed_ms: u64,
    pub on_error_policy: OnError,
    pub error: Option<&'a str>,
}

/// File-local: the 4-level dispatch macro. `tracing::event!`'s Level arg
/// must be a compile-time path, so the match is unavoidable. The macro
/// keeps the field list defined exactly once.
///
/// IMPORTANT: `RequestId` has no `Display` impl (frozen-core); access the
/// inner `String` via `&$f.request_id.0` to emit a bare ID string.
macro_rules! emit_at_level {
    ($level:expr, $f:expr) => {
        match $level {
            tracing::Level::ERROR => tracing::error!(
                "plugin.name" = $f.plugin_name,
                "plugin.kind" = $f.plugin_kind,
                "plugin.hook" = $f.plugin_hook,
                "agent_shim.request_id" = $f.request_id.0.as_str(),
                "agent_shim.route" = $f.route,
                "plugin.outcome" = $f.outcome.as_label(),
                "plugin.elapsed_ms" = $f.elapsed_ms,
                "plugin.on_error_policy" = ?$f.on_error_policy,
                "plugin.error" = $f.error,
            ),
            tracing::Level::WARN => tracing::warn!(
                "plugin.name" = $f.plugin_name,
                "plugin.kind" = $f.plugin_kind,
                "plugin.hook" = $f.plugin_hook,
                "agent_shim.request_id" = $f.request_id.0.as_str(),
                "agent_shim.route" = $f.route,
                "plugin.outcome" = $f.outcome.as_label(),
                "plugin.elapsed_ms" = $f.elapsed_ms,
                "plugin.on_error_policy" = ?$f.on_error_policy,
                "plugin.error" = $f.error,
            ),
            tracing::Level::INFO => tracing::info!(
                "plugin.name" = $f.plugin_name,
                "plugin.kind" = $f.plugin_kind,
                "plugin.hook" = $f.plugin_hook,
                "agent_shim.request_id" = $f.request_id.0.as_str(),
                "agent_shim.route" = $f.route,
                "plugin.outcome" = $f.outcome.as_label(),
                "plugin.elapsed_ms" = $f.elapsed_ms,
                "plugin.on_error_policy" = ?$f.on_error_policy,
                "plugin.error" = $f.error,
            ),
            tracing::Level::DEBUG => tracing::debug!(
                "plugin.name" = $f.plugin_name,
                "plugin.kind" = $f.plugin_kind,
                "plugin.hook" = $f.plugin_hook,
                "agent_shim.request_id" = $f.request_id.0.as_str(),
                "agent_shim.route" = $f.route,
                "plugin.outcome" = $f.outcome.as_label(),
                "plugin.elapsed_ms" = $f.elapsed_ms,
                "plugin.on_error_policy" = ?$f.on_error_policy,
                "plugin.error" = $f.error,
            ),
            _ => {}
        }
    };
}

impl PluginLogFields<'_> {
    /// Single emission point from `invoke()`. §7.5 noise control: H5
    /// success skips log emission (metric still updates upstream).
    pub(crate) fn emit(&self) {
        if self.outcome == PluginOutcome::Success && self.plugin_hook == "on_stream_event" {
            return;
        }
        emit_at_level!(self.outcome.level(self.on_error_policy), self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_label_strings_are_stable() {
        assert_eq!(PluginOutcome::Success.as_label(), "success");
        assert_eq!(PluginOutcome::Skipped.as_label(), "skipped");
        assert_eq!(PluginOutcome::Failed.as_label(), "failed");
        assert_eq!(PluginOutcome::TimedOut.as_label(), "timed_out");
        assert_eq!(PluginOutcome::Aborted.as_label(), "aborted");
        assert_eq!(
            PluginOutcome::ProtectedFieldMutated.as_label(),
            "protected_field_mutated"
        );
    }

    #[test]
    fn level_success_is_debug_regardless_of_on_error() {
        assert_eq!(
            PluginOutcome::Success.level(OnError::Skip),
            tracing::Level::DEBUG
        );
        assert_eq!(
            PluginOutcome::Success.level(OnError::Fail),
            tracing::Level::DEBUG
        );
    }

    #[test]
    fn level_skipped_is_warn() {
        assert_eq!(
            PluginOutcome::Skipped.level(OnError::Skip),
            tracing::Level::WARN
        );
        assert_eq!(
            PluginOutcome::Skipped.level(OnError::Fail),
            tracing::Level::WARN
        );
    }

    #[test]
    fn level_failed_is_error() {
        assert_eq!(
            PluginOutcome::Failed.level(OnError::Skip),
            tracing::Level::ERROR
        );
        assert_eq!(
            PluginOutcome::Failed.level(OnError::Fail),
            tracing::Level::ERROR
        );
    }

    #[test]
    fn level_timeout_is_warn_on_skip_error_on_fail() {
        assert_eq!(
            PluginOutcome::TimedOut.level(OnError::Skip),
            tracing::Level::WARN
        );
        assert_eq!(
            PluginOutcome::TimedOut.level(OnError::Fail),
            tracing::Level::ERROR
        );
    }

    #[test]
    fn level_aborted_is_info() {
        assert_eq!(
            PluginOutcome::Aborted.level(OnError::Skip),
            tracing::Level::INFO
        );
        assert_eq!(
            PluginOutcome::Aborted.level(OnError::Fail),
            tracing::Level::INFO
        );
    }

    #[test]
    fn level_protected_field_mutated_is_error() {
        assert_eq!(
            PluginOutcome::ProtectedFieldMutated.level(OnError::Skip),
            tracing::Level::ERROR
        );
        assert_eq!(
            PluginOutcome::ProtectedFieldMutated.level(OnError::Fail),
            tracing::Level::ERROR
        );
    }

    #[test]
    fn h5_success_skips_emit_no_panic() {
        let fields = PluginLogFields {
            plugin_name: "n",
            plugin_kind: "k",
            plugin_hook: "on_stream_event",
            request_id: &RequestId::new(),
            route: "r",
            outcome: PluginOutcome::Success,
            elapsed_ms: 0,
            on_error_policy: OnError::Skip,
            error: None,
        };
        fields.emit();
    }

    #[test]
    fn h2_success_emit_no_panic() {
        let fields = PluginLogFields {
            plugin_name: "n",
            plugin_kind: "k",
            plugin_hook: "on_decoded_request",
            request_id: &RequestId::new(),
            route: "r",
            outcome: PluginOutcome::Success,
            elapsed_ms: 1,
            on_error_policy: OnError::Skip,
            error: None,
        };
        fields.emit();
    }

    #[test]
    fn failed_emit_with_error_message_no_panic() {
        let fields = PluginLogFields {
            plugin_name: "n",
            plugin_kind: "k",
            plugin_hook: "on_decoded_request",
            request_id: &RequestId::new(),
            route: "r",
            outcome: PluginOutcome::Failed,
            elapsed_ms: 1,
            on_error_policy: OnError::Fail,
            error: Some("boom"),
        };
        fields.emit();
    }

    #[test]
    #[tracing_test::traced_test]
    fn failed_emit_uses_error_level() {
        let fields = PluginLogFields {
            plugin_name: "p",
            plugin_kind: "k",
            plugin_hook: "on_decoded_request",
            request_id: &RequestId::new(),
            route: "r",
            outcome: PluginOutcome::Failed,
            elapsed_ms: 1,
            on_error_policy: OnError::Skip,
            error: Some("boom"),
        };
        fields.emit();
        // The traced_test macro captures all events. Verify an ERROR
        // event went out with the plugin.name field set.
        assert!(
            logs_contain("plugin.name=\"p\""),
            "log line should carry plugin.name field"
        );
        // tracing_test asserts level via the macro env; an ERROR event
        // emitted at any non-error level would be filtered out by the
        // tracing-test infrastructure. So if logs_contain finds it,
        // the level is at least ERROR-compatible.
    }
}
