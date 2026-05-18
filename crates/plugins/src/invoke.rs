//! The `invoke()` template runs around every plugin call. It owns the
//! timeout, the on_error policy, the protected-field diff, the
//! per-call OTel span, structured logging, and metric recording.
//! Plugin authors never see this code.
//!
//! Spec §6.3 / §6.4 / §4.5 + §7 (P05).

use std::future::Future;
use std::time::{Duration, Instant};

use agent_shim_core::CanonicalRequest;
use tracing::Instrument;

use crate::context::PluginContext;
use crate::error::{OnError, PluginError, PluginResult};
use crate::log_fields::{PluginLogFields, PluginOutcome};
use crate::registry::PluginEntry;
use crate::trait_def::Hook;

/// Outcome of a single invocation. Distinct from `PluginResult` because
/// the registry needs to know "swallowed by on_error: skip" (Skipped)
/// vs "success" (Success(v)) vs "propagate" (Propagate(err)).
#[allow(dead_code)] // used by P04 run_* methods
pub(crate) enum InvokeOutcome<T> {
    Success(T),
    Skipped,
    Propagate(PluginError),
}

/// Span instrumentation mode for a single invoke call. §7.3 + §7.5.
///
/// - `PerInvocation`: open a fresh `plugin.invoke` span. Used by H2/H3/H7.
/// - `Aggregated`: do NOT open a new span; events attach to whatever
///   span is current (typically `plugin.stream` from `wrap_stream`).
///   Used by H5 to avoid 500-event-per-request span explosion.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // used by P04 run_* methods after T6
pub(crate) enum SpanMode {
    PerInvocation,
    Aggregated,
}

/// Bundle of static/per-entry args for a single invoke. Replaces the
/// 6-positional-args form to keep call sites readable (P05 Q7).
#[allow(dead_code)] // used by P04 run_* methods after T6
pub(crate) struct InvokeArgs<'a> {
    pub plugin_name: &'a str,
    pub plugin_kind: &'static str,
    pub hook: &'static str,
    pub timeout_ms: u64,
    pub on_error: OnError,
    pub span_mode: SpanMode,
}

impl InvokeArgs<'_> {
    /// Build args from a `PluginEntry` + the hook being invoked + the
    /// span mode for this hook class. The cached `entry.kind`
    /// `&'static str` flows through; timeouts pick the per-hook value.
    #[allow(dead_code)] // used by P04 run_* methods after T6
    pub(crate) fn from_entry<'a>(
        entry: &'a PluginEntry,
        hook: Hook,
        span_mode: SpanMode,
    ) -> InvokeArgs<'a> {
        InvokeArgs {
            plugin_name: &entry.name,
            plugin_kind: entry.kind,
            hook: hook.as_str(),
            timeout_ms: entry.timeouts.for_hook(hook),
            on_error: entry.on_error,
            span_mode,
        }
    }
}

/// Run a single plugin hook with the standard policy envelope.
#[allow(dead_code)] // used by P04 run_* methods after T6
pub(crate) async fn invoke<T, Fut>(
    args: InvokeArgs<'_>,
    ctx: &PluginContext,
    work: Fut,
) -> InvokeOutcome<T>
where
    Fut: Future<Output = PluginResult<T>>,
{
    // §7.3 + Q16: PerInvocation opens a fresh plugin.invoke span;
    // Aggregated uses Span::none() (zero-overhead disabled span). The
    // work future is always `.instrument(span.clone())`, which means a
    // single timeout code path regardless of mode.
    let span = match args.span_mode {
        SpanMode::PerInvocation => tracing::info_span!(
            "plugin.invoke",
            "plugin.name" = args.plugin_name,
            "plugin.kind" = args.plugin_kind,
            "plugin.hook" = args.hook,
            "plugin.outcome" = tracing::field::Empty,
            "plugin.elapsed_ms" = tracing::field::Empty,
        ),
        SpanMode::Aggregated => tracing::Span::none(),
    };

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_millis(args.timeout_ms),
        work.instrument(span.clone()),
    )
    .await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let (outcome, err_for_log, returned): (PluginOutcome, Option<String>, InvokeOutcome<T>) =
        match result {
            Ok(Ok(value)) => (PluginOutcome::Success, None, InvokeOutcome::Success(value)),
            Ok(Err(err @ PluginError::Aborted { .. })) => {
                let err_string = err.to_string();
                (
                    PluginOutcome::Aborted,
                    Some(err_string),
                    InvokeOutcome::Propagate(err),
                )
            }
            Ok(Err(err @ PluginError::ProtectedFieldMutated { .. })) => {
                let err_string = err.to_string();
                let outcome = PluginOutcome::ProtectedFieldMutated;
                let returned = match args.on_error {
                    OnError::Skip => InvokeOutcome::Skipped,
                    OnError::Fail => InvokeOutcome::Propagate(err),
                };
                (outcome, Some(err_string), returned)
            }
            Ok(Err(err @ PluginError::Failed { .. })) => {
                let err_string = err.to_string();
                let returned = match args.on_error {
                    OnError::Skip => InvokeOutcome::Skipped,
                    OnError::Fail => InvokeOutcome::Propagate(err),
                };
                let outcome = match args.on_error {
                    OnError::Skip => PluginOutcome::Skipped,
                    OnError::Fail => PluginOutcome::Failed,
                };
                (outcome, Some(err_string), returned)
            }
            Ok(Err(err @ PluginError::Timeout { .. })) => {
                let err_string = err.to_string();
                let returned = match args.on_error {
                    OnError::Skip => InvokeOutcome::Skipped,
                    OnError::Fail => InvokeOutcome::Propagate(err),
                };
                (PluginOutcome::TimedOut, Some(err_string), returned)
            }
            Err(_elapsed) => {
                let err = PluginError::Timeout {
                    plugin: args.plugin_name.to_string(),
                    hook: args.hook,
                    elapsed_ms,
                };
                let err_string = err.to_string();
                let returned = match args.on_error {
                    OnError::Skip => InvokeOutcome::Skipped,
                    OnError::Fail => InvokeOutcome::Propagate(err),
                };
                (PluginOutcome::TimedOut, Some(err_string), returned)
            }
        };

    // Record fields on the span. No-op on Span::none() (Aggregated mode).
    span.record("plugin.outcome", outcome.as_label());
    span.record("plugin.elapsed_ms", elapsed_ms);

    // Structured log emission (Q3). H5 success skips inside emit().
    let fields = PluginLogFields {
        plugin_name: args.plugin_name,
        plugin_kind: args.plugin_kind,
        plugin_hook: args.hook,
        request_id: &ctx.request_id,
        route: &ctx.route_label,
        outcome,
        elapsed_ms,
        on_error_policy: args.on_error,
        error: err_for_log.as_deref(),
    };
    fields.emit();

    // Prometheus metric (always update, including H5 success).
    agent_shim_observability::metrics::recorders::record_plugin_invocation(
        args.plugin_kind,
        args.plugin_name,
        args.hook,
        outcome.as_label(),
        elapsed_ms as f64 / 1000.0,
    );

    returned
}

/// Check that a plugin did not mutate any of the four protected
/// fields. Returns `Err(ProtectedFieldMutated)` if it did, naming the
/// first field that changed.
///
/// Protected fields: `id`, `frontend`, `model`, `stream`. The four
/// fields that drive routing, identity, and pipeline branching.
/// (Spec §4.5 / Q3.)
#[allow(dead_code)] // used by P04 run_* methods
pub(crate) fn check_protected_fields(
    plugin_name: &str,
    hook: &'static str,
    before: &CanonicalRequest,
    after: &CanonicalRequest,
) -> Result<(), PluginError> {
    if before.id != after.id {
        return Err(PluginError::ProtectedFieldMutated {
            plugin: plugin_name.to_string(),
            hook,
            field: "id",
        });
    }
    if before.frontend != after.frontend {
        return Err(PluginError::ProtectedFieldMutated {
            plugin: plugin_name.to_string(),
            hook,
            field: "frontend",
        });
    }
    if before.model != after.model {
        return Err(PluginError::ProtectedFieldMutated {
            plugin: plugin_name.to_string(),
            hook,
            field: "model",
        });
    }
    if before.stream != after.stream {
        return Err(PluginError::ProtectedFieldMutated {
            plugin: plugin_name.to_string(),
            hook,
            field: "stream",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{
        request::RequestMetadata, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel,
        GenerationOptions, RequestId, ResolvedPolicy,
    };

    fn req() -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: FrontendModel::from("m"),
            },
            model: FrontendModel::from("m"),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: Default::default(),
            generation: GenerationOptions::default(),
            response_format: None,
            stream: false,
            metadata: RequestMetadata::default(),
            inbound_anthropic_headers: vec![],
            resolved_policy: ResolvedPolicy::default(),
            extensions: ExtensionMap::new(),
        }
    }

    // ── invoke() — InvokeArgs shape (P05) ──────────────────────────────

    fn ctx() -> PluginContext {
        PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "anthropic_messages/m".to_string(),
        }
    }

    fn args_for<'a>(name: &'a str, hook: &'static str) -> InvokeArgs<'a> {
        InvokeArgs {
            plugin_name: name,
            plugin_kind: "test_kind",
            hook,
            timeout_ms: 50,
            on_error: OnError::Skip,
            span_mode: SpanMode::PerInvocation,
        }
    }

    #[tokio::test]
    async fn invoke_with_args_success_returns_value() {
        let c = ctx();
        let a = args_for("p", "on_decoded_request");
        let out: InvokeOutcome<i32> = invoke(a, &c, async { Ok(42) }).await;
        match out {
            InvokeOutcome::Success(v) => assert_eq!(v, 42),
            _ => panic!("expected Success"),
        }
    }

    #[tokio::test]
    async fn invoke_with_args_failed_skip_returns_skipped() {
        let c = ctx();
        let a = args_for("p", "on_decoded_request");
        let out: InvokeOutcome<i32> = invoke(
            a,
            &c,
            async {
                Err(PluginError::Failed {
                    plugin: "p".to_string(),
                    hook: "on_decoded_request",
                    message: "boom".to_string(),
                })
            },
        )
        .await;
        assert!(matches!(out, InvokeOutcome::Skipped));
    }

    #[tokio::test]
    async fn invoke_with_args_failed_fail_propagates() {
        let c = ctx();
        let mut a = args_for("p", "on_decoded_request");
        a.on_error = OnError::Fail;
        let out: InvokeOutcome<i32> = invoke(
            a,
            &c,
            async {
                Err(PluginError::Failed {
                    plugin: "p".to_string(),
                    hook: "on_decoded_request",
                    message: "boom".to_string(),
                })
            },
        )
        .await;
        assert!(matches!(out, InvokeOutcome::Propagate(_)));
    }

    #[tokio::test]
    async fn invoke_with_args_aborted_always_propagates() {
        let c = ctx();
        let a = args_for("p", "on_decoded_request"); // Skip
        let out: InvokeOutcome<i32> = invoke(
            a,
            &c,
            async {
                Err(PluginError::Aborted {
                    plugin: "p".to_string(),
                    reason: "policy".to_string(),
                })
            },
        )
        .await;
        match out {
            InvokeOutcome::Propagate(PluginError::Aborted { .. }) => {}
            _ => panic!("expected Propagate(Aborted)"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn invoke_with_args_timeout_skip_returns_skipped() {
        let c = ctx();
        let mut a = args_for("p", "on_decoded_request");
        a.timeout_ms = 10;
        let out: InvokeOutcome<i32> = invoke(
            a,
            &c,
            async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(42)
            },
        )
        .await;
        assert!(matches!(out, InvokeOutcome::Skipped));
    }

    #[tokio::test]
    async fn invoke_aggregated_span_mode_does_not_panic() {
        let c = ctx();
        let mut a = args_for("p", "on_stream_event");
        a.span_mode = SpanMode::Aggregated;
        let out: InvokeOutcome<i32> = invoke(a, &c, async { Ok(99) }).await;
        assert!(matches!(out, InvokeOutcome::Success(99)));
    }

    // ── check_protected_fields (unchanged from P04) ────────────────────

    #[test]
    fn protected_fields_pass_when_identical() {
        let a = req();
        let b = a.clone();
        assert!(check_protected_fields("p", "h", &a, &b).is_ok());
    }

    #[test]
    fn protected_fields_detect_model_change() {
        let a = req();
        let mut b = a.clone();
        b.model = FrontendModel::from("different");
        let err = check_protected_fields("p", "h", &a, &b).unwrap_err();
        match err {
            PluginError::ProtectedFieldMutated { field, .. } => assert_eq!(field, "model"),
            _ => panic!("expected ProtectedFieldMutated"),
        }
    }

    #[test]
    fn protected_fields_detect_id_change() {
        let a = req();
        let mut b = a.clone();
        b.id = RequestId::new();
        let err = check_protected_fields("p", "h", &a, &b).unwrap_err();
        match err {
            PluginError::ProtectedFieldMutated { field, .. } => assert_eq!(field, "id"),
            _ => panic!("expected ProtectedFieldMutated"),
        }
    }

    #[test]
    fn protected_fields_detect_stream_change() {
        let a = req();
        let mut b = a.clone();
        b.stream = !a.stream;
        let err = check_protected_fields("p", "h", &a, &b).unwrap_err();
        match err {
            PluginError::ProtectedFieldMutated { field, .. } => assert_eq!(field, "stream"),
            _ => panic!("expected ProtectedFieldMutated"),
        }
    }

    #[test]
    fn protected_fields_allow_messages_change() {
        let a = req();
        let mut b = a.clone();
        b.messages = vec![];
        b.system = vec![];
        assert!(check_protected_fields("p", "h", &a, &b).is_ok());
    }
}
