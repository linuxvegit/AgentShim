//! The `invoke()` template runs around every plugin call. It owns the
//! timeout, the on_error policy, the protected-field diff, and (in P05)
//! the logging + metrics. Plugin authors never see this code — they
//! just `Ok(...)` or `Err(...)` from their hook method, and the
//! template handles the rest.
//!
//! Spec §6.3 / §6.4 / §4.5.

use std::future::Future;
use std::time::{Duration, Instant};

use agent_shim_core::CanonicalRequest;

use crate::context::PluginContext;
use crate::error::{OnError, PluginError, PluginResult};

/// Outcome of a single invocation. Distinct from `PluginResult` because
/// the registry needs to know "swallowed by on_error: skip" (Ok(None))
/// vs "success" (Ok(Some(v))) vs "propagate" (Err).
#[allow(dead_code)] // used by P04 run_* methods
pub(crate) enum InvokeOutcome<T> {
    /// Plugin ran successfully and returned a value the caller should
    /// apply (e.g. the rewritten CanonicalRequest).
    Success(T),
    /// Plugin failed but on_error: skip swallowed it. Caller keeps
    /// prior state.
    Skipped,
    /// Plugin aborted the request (HTTP 400) or on_error: fail
    /// propagated a failure (HTTP 502). Caller short-circuits.
    Propagate(PluginError),
}

/// Run a single plugin hook with the standard policy envelope.
///
/// `plugin_name`: YAML name of the plugin instance — used only for
/// logging fields, never returned to plugin code.
/// `hook`: stable hook string (`Hook::as_str()`).
/// `timeout_ms`: the per-hook timeout for this plugin.
/// `on_error`: the per-plugin on_error policy.
/// `work`: a future producing `PluginResult<T>`. The factory will
/// typically build this via a closure capturing the plugin's hook
/// method call.
#[allow(dead_code)] // used by P04 run_* methods
pub(crate) async fn invoke<T, Fut>(
    plugin_name: &str,
    _ctx: &PluginContext,
    hook: &'static str,
    timeout_ms: u64,
    on_error: OnError,
    work: Fut,
) -> InvokeOutcome<T>
where
    Fut: Future<Output = PluginResult<T>>,
{
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_millis(timeout_ms), work).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(Ok(value)) => InvokeOutcome::Success(value),
        Ok(Err(PluginError::Aborted { plugin, reason })) => {
            // Aborted is NEVER subject to on_error — always propagate.
            InvokeOutcome::Propagate(PluginError::Aborted { plugin, reason })
        }
        Ok(Err(PluginError::ProtectedFieldMutated {
            plugin,
            hook: h,
            field,
        })) => {
            // Same — programming error, always propagate (subject to
            // on_error mapping by the registry).
            match on_error {
                OnError::Skip => InvokeOutcome::Skipped,
                OnError::Fail => InvokeOutcome::Propagate(PluginError::ProtectedFieldMutated {
                    plugin,
                    hook: h,
                    field,
                }),
            }
        }
        Ok(Err(err)) => match on_error {
            OnError::Skip => InvokeOutcome::Skipped,
            OnError::Fail => InvokeOutcome::Propagate(err),
        },
        Err(_) => {
            // Timeout elapsed before the future completed.
            let err = PluginError::Timeout {
                plugin: plugin_name.to_string(),
                hook,
                elapsed_ms,
            };
            match on_error {
                OnError::Skip => InvokeOutcome::Skipped,
                OnError::Fail => InvokeOutcome::Propagate(err),
            }
        }
    }
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

    #[tokio::test]
    async fn invoke_success_returns_value() {
        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        let out: InvokeOutcome<i32> = invoke(
            "test",
            &ctx,
            "on_decoded_request",
            50,
            OnError::Skip,
            async { Ok(42) },
        )
        .await;
        match out {
            InvokeOutcome::Success(v) => assert_eq!(v, 42),
            _ => panic!("expected Success"),
        }
    }

    #[tokio::test]
    async fn invoke_failed_with_skip_returns_skipped() {
        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        let out: InvokeOutcome<i32> = invoke(
            "test",
            &ctx,
            "on_decoded_request",
            50,
            OnError::Skip,
            async {
                Err(PluginError::Failed {
                    plugin: "test".to_string(),
                    hook: "on_decoded_request",
                    message: "boom".to_string(),
                })
            },
        )
        .await;
        assert!(matches!(out, InvokeOutcome::Skipped));
    }

    #[tokio::test]
    async fn invoke_failed_with_fail_returns_propagate() {
        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        let out: InvokeOutcome<i32> = invoke(
            "test",
            &ctx,
            "on_decoded_request",
            50,
            OnError::Fail,
            async {
                Err(PluginError::Failed {
                    plugin: "test".to_string(),
                    hook: "on_decoded_request",
                    message: "boom".to_string(),
                })
            },
        )
        .await;
        assert!(matches!(out, InvokeOutcome::Propagate(_)));
    }

    #[tokio::test]
    async fn invoke_aborted_always_propagates() {
        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        // Even with on_error: Skip, Aborted propagates.
        let out: InvokeOutcome<i32> = invoke(
            "test",
            &ctx,
            "on_decoded_request",
            50,
            OnError::Skip,
            async {
                Err(PluginError::Aborted {
                    plugin: "test".to_string(),
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
    async fn invoke_timeout_with_skip_returns_skipped() {
        let ctx = PluginContext {
            request_id: RequestId::new(),
            frontend: FrontendKind::AnthropicMessages,
            route_label: "x".to_string(),
        };
        let out: InvokeOutcome<i32> = invoke(
            "test",
            &ctx,
            "on_decoded_request",
            10, // ms
            OnError::Skip,
            async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(42)
            },
        )
        .await;
        assert!(matches!(out, InvokeOutcome::Skipped));
    }

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
        b.messages = vec![]; // any content change is fine
        b.system = vec![];
        // messages/system aren't protected → no error.
        assert!(check_protected_fields("p", "h", &a, &b).is_ok());
    }
}
