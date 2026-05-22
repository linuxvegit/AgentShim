//! `usage_recorder` built-in plugin — H7 structured log emission.
//!
//! Spec §5.4 / P06a T5–T6. Subscribes to `ResponseComplete` only.
//! Emits one tracing event per completed request at the configured level,
//! tagged `target: "agent_shim::usage_recorder"`.

use serde::{Deserialize, Serialize};

use async_trait::async_trait;

use crate::context::{PluginContext, ResponseSummary};
use crate::error::{PluginConfigError, PluginResult};
use crate::trait_def::{HookSet, Plugin, PluginFactory};

// ─── Config types ────────────────────────────────────────────────────────────

/// Where to send the usage log. Currently only `log` is supported;
/// future variants may include `file`, `kafka`, etc.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Sink {
    /// Emit via `tracing` (structured log).
    #[default]
    Log,
}

/// Tracing level for the emitted event.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    #[default]
    Info,
    Warn,
    Debug,
}

/// Optional extra fields to include in the log event.
/// Validated at construction; runtime path reads from `ResponseSummary`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    RequestId,
    Route,
    InputTokens,
    OutputTokens,
    ElapsedMs,
    UpstreamStatus,
}

/// Per-plugin YAML `config:` block for `usage_recorder`.
///
/// ```yaml
/// plugins:
///   usage_log:
///     type: usage_recorder
///     on_error: skip
///     config:
///       sink: log
///       level: info
///       fields: [request_id, route]
/// ```
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageRecorderConfig {
    /// Destination sink (default: log).
    #[serde(default)]
    pub sink: Sink,

    /// Log level (default: info).
    #[serde(default)]
    pub level: LogLevel,

    /// Extra fields to include (default: empty). Validation-only; the
    /// runtime always emits the full standard field set.
    #[allow(dead_code)]
    #[serde(default)]
    pub fields: Vec<Field>,
}

// ─── Factory ─────────────────────────────────────────────────────────────────

/// Factory for `usage_recorder`. Registered via `builtin_plugins()`.
pub struct UsageRecorderFactory;

impl PluginFactory for UsageRecorderFactory {
    fn kind_name(&self) -> &'static str {
        "usage_recorder"
    }

    fn instantiate(
        &self,
        plugin_name: &str,
        config: serde_json::Value,
    ) -> Result<Box<dyn Plugin>, PluginConfigError> {
        let cfg: UsageRecorderConfig = serde_json::from_value(config)
            .map_err(|e| PluginConfigError::Deserialize(e, plugin_name.to_string()))?;

        // Validate: only Log sink is supported right now.
        if cfg.sink != Sink::Log {
            return Err(PluginConfigError::InvalidValue {
                plugin: plugin_name.to_string(),
                field: "sink".to_string(),
                reason: "only `log` sink is supported".to_string(),
            });
        }

        Ok(Box::new(UsageRecorder { level: cfg.level }))
    }
}

// ─── Plugin ──────────────────────────────────────────────────────────────────

/// Runtime state for a single `usage_recorder` instance.
pub struct UsageRecorder {
    level: LogLevel,
}

#[async_trait]
impl Plugin for UsageRecorder {
    fn kind_name(&self) -> &'static str {
        "usage_recorder"
    }

    fn hooks(&self) -> HookSet {
        HookSet::RESPONSE_COMPLETE
    }

    async fn on_response_complete(
        &self,
        ctx: &PluginContext,
        summary: &ResponseSummary,
    ) -> PluginResult<()> {
        let input_tokens = summary.usage.as_ref().map(|u| u.input_tokens);
        let output_tokens = summary.usage.as_ref().map(|u| u.output_tokens);
        let status_str: &str = match summary.upstream_status {
            crate::context::UpstreamStatus::Success => "success",
            crate::context::UpstreamStatus::Error => "error",
            crate::context::UpstreamStatus::Cancelled => "cancelled",
        };
        match self.level {
            LogLevel::Info => tracing::info!(
                target: "agent_shim::usage_recorder",
                {
                    "plugin.kind" = "usage_recorder",
                    "agent_shim.request_id" = ctx.request_id.0.as_str(),
                    "agent_shim.route" = ctx.route_label.as_str(),
                    "usage.input_tokens" = ?input_tokens,
                    "usage.output_tokens" = ?output_tokens,
                    "usage.elapsed_ms" = summary.elapsed_ms,
                    "usage.upstream_status" = status_str,
                },
                "usage recorded",
            ),
            LogLevel::Warn => tracing::warn!(
                target: "agent_shim::usage_recorder",
                {
                    "plugin.kind" = "usage_recorder",
                    "agent_shim.request_id" = ctx.request_id.0.as_str(),
                    "agent_shim.route" = ctx.route_label.as_str(),
                    "usage.input_tokens" = ?input_tokens,
                    "usage.output_tokens" = ?output_tokens,
                    "usage.elapsed_ms" = summary.elapsed_ms,
                    "usage.upstream_status" = status_str,
                },
                "usage recorded",
            ),
            LogLevel::Debug => tracing::debug!(
                target: "agent_shim::usage_recorder",
                {
                    "plugin.kind" = "usage_recorder",
                    "agent_shim.request_id" = ctx.request_id.0.as_str(),
                    "agent_shim.route" = ctx.route_label.as_str(),
                    "usage.input_tokens" = ?input_tokens,
                    "usage.output_tokens" = ?output_tokens,
                    "usage.elapsed_ms" = summary.elapsed_ms,
                    "usage.upstream_status" = status_str,
                },
                "usage recorded",
            ),
        }
        Ok(())
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_factory() -> UsageRecorderFactory {
        UsageRecorderFactory
    }

    // ── Config deserialization ──────────────────────────────────────────────

    #[test]
    fn config_defaults_applied_when_empty_object() {
        let cfg: UsageRecorderConfig = serde_json::from_value(json!({})).unwrap();
        assert_eq!(cfg.sink, Sink::Log);
        assert_eq!(cfg.level, LogLevel::Info);
        assert!(cfg.fields.is_empty());
    }

    #[test]
    fn config_explicit_level_warn() {
        let cfg: UsageRecorderConfig =
            serde_json::from_value(json!({"sink": "log", "level": "warn"})).unwrap();
        assert_eq!(cfg.level, LogLevel::Warn);
    }

    #[test]
    fn config_explicit_level_debug() {
        let cfg: UsageRecorderConfig = serde_json::from_value(json!({"level": "debug"})).unwrap();
        assert_eq!(cfg.level, LogLevel::Debug);
    }

    #[test]
    fn config_unknown_field_rejected() {
        let result: Result<UsageRecorderConfig, _> =
            serde_json::from_value(json!({"unknown_key": true}));
        assert!(
            result.is_err(),
            "deny_unknown_fields should reject unknown keys"
        );
    }

    #[test]
    fn config_fields_list_parsed() {
        let cfg: UsageRecorderConfig =
            serde_json::from_value(json!({"fields": ["request_id", "route"]})).unwrap();
        assert_eq!(cfg.fields, vec![Field::RequestId, Field::Route]);
    }

    // ── Factory ────────────────────────────────────────────────────────────

    #[test]
    fn factory_kind_name() {
        assert_eq!(make_factory().kind_name(), "usage_recorder");
    }

    #[test]
    fn factory_instantiate_ok() {
        let plugin = make_factory()
            .instantiate("my_usage_log", json!({"level": "info"}))
            .expect("should instantiate");
        assert_eq!(plugin.kind_name(), "usage_recorder");
        assert!(plugin.hooks().contains(crate::Hook::ResponseComplete));
    }

    #[test]
    fn factory_instantiate_bad_json_errors() {
        let result = make_factory().instantiate("bad", json!({"level": 42}));
        assert!(result.is_err());
    }

    // ── Runtime emission tests ─────────────────────────────────────────────

    use crate::context::{PluginContext, ResponseSummary, UpstreamStatus};
    use agent_shim_core::{FrontendKind, RequestId, Usage};

    fn make_ctx() -> PluginContext {
        PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/test-model".to_string(),
        )
    }

    fn make_summary_success() -> ResponseSummary {
        ResponseSummary {
            usage: Some(Usage {
                input_tokens: Some(17),
                output_tokens: Some(42),
                ..Default::default()
            }),
            elapsed_ms: 250,
            upstream_status: UpstreamStatus::Success,
        }
    }

    fn make_summary_no_usage() -> ResponseSummary {
        ResponseSummary {
            usage: None,
            elapsed_ms: 100,
            upstream_status: UpstreamStatus::Error,
        }
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn on_response_complete_emits_at_info_level_with_all_fields() {
        let plugin = UsageRecorder {
            level: LogLevel::Info,
        };
        let ctx = make_ctx();
        let summary = make_summary_success();
        plugin.on_response_complete(&ctx, &summary).await.unwrap();
        assert!(logs_contain("usage recorded"));
        assert!(logs_contain("usage_recorder"));
        assert!(logs_contain("success"));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn on_response_complete_with_none_usage_emits_none_via_debug() {
        let plugin = UsageRecorder {
            level: LogLevel::Info,
        };
        let ctx = make_ctx();
        let summary = make_summary_no_usage();
        plugin.on_response_complete(&ctx, &summary).await.unwrap();
        assert!(logs_contain("usage recorded"));
        assert!(logs_contain("None"));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn on_response_complete_warn_level_emits_at_warn() {
        let plugin = UsageRecorder {
            level: LogLevel::Warn,
        };
        let ctx = make_ctx();
        let summary = make_summary_success();
        plugin.on_response_complete(&ctx, &summary).await.unwrap();
        assert!(logs_contain("usage recorded"));
    }
}
