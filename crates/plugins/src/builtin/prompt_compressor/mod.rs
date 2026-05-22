#![cfg(feature = "prompt_compressor")]

pub mod config;
pub(super) mod drop_old_turns;
pub(super) mod groups;
pub(super) mod summarize;
pub(super) mod truncate;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use agent_shim_core::{BackendTarget, CanonicalRequest};
use agent_shim_providers::BackendProvider;

use crate::{
    FactoryDependencies, HookSet, Plugin, PluginConfigError, PluginFactory,
};
use crate::context::PluginContext;
use crate::error::PluginResult;
use config::{PromptCompressorConfig, Strategy};

/// Result of applying a single strategy. `action` becomes a metric label.
pub(super) struct CompressionResult {
    pub new_messages: Vec<agent_shim_core::Message>,
    pub dropped: usize,
    pub action: &'static str,
}

// ─── Compiled strategy ───────────────────────────────────────────────────────

pub(crate) enum CompiledStrategy {
    DropOldTurns {
        keep_last_n: usize,
    },
    TruncateToTokens {
        target_tokens: u32,
        keep_last_n: usize,
    },
    SummarizeOldTurns {
        keep_last_n: usize,
        provider: Arc<dyn BackendProvider>,
        target: BackendTarget,
        max_summary_tokens: u32,
        timeout: Duration,
    },
}

// ─── Plugin struct ───────────────────────────────────────────────────────────

pub struct PromptCompressor {
    strategy: CompiledStrategy,
}

#[async_trait]
impl Plugin for PromptCompressor {
    fn kind_name(&self) -> &'static str {
        "prompt_compressor"
    }

    fn hooks(&self) -> HookSet {
        HookSet::DECODED_REQUEST
    }

    async fn on_decoded_request(
        &self,
        _ctx: &PluginContext,
        mut req: CanonicalRequest,
    ) -> PluginResult<CanonicalRequest> {
        let (result, strategy_label) = match &self.strategy {
            CompiledStrategy::DropOldTurns { keep_last_n } => (
                drop_old_turns::apply(&req.messages, *keep_last_n),
                "drop_old_turns",
            ),
            CompiledStrategy::TruncateToTokens {
                target_tokens,
                keep_last_n,
            } => (
                truncate::apply(&req.messages, *target_tokens, *keep_last_n),
                "truncate_to_tokens",
            ),
            CompiledStrategy::SummarizeOldTurns {
                keep_last_n,
                provider,
                target,
                max_summary_tokens,
                timeout,
            } => (
                summarize::apply(
                    &req.messages,
                    *keep_last_n,
                    provider,
                    target,
                    *max_summary_tokens,
                    *timeout,
                )
                .await,
                "summarize_old_turns",
            ),
        };

        metrics::counter!(
            agent_shim_observability::metrics::catalog::PluginPromptCompressorActionsTotal::NAME,
            "strategy" => strategy_label,
            "action" => result.action,
        )
        .increment(1);

        if result.action != "skipped" {
            if result.dropped > 0 {
                metrics::counter!(
                    agent_shim_observability::metrics::catalog::PluginPromptCompressorMessagesDroppedTotal::NAME,
                    "strategy" => strategy_label,
                )
                .increment(result.dropped as u64);
            }
            req.messages = result.new_messages;
        }
        Ok(req)
    }
}

// ─── Factory ─────────────────────────────────────────────────────────────────

pub struct PromptCompressorFactory;

impl PluginFactory for PromptCompressorFactory {
    fn kind_name(&self) -> &'static str {
        "prompt_compressor"
    }

    fn instantiate(
        &self,
        plugin_name: &str,
        config: Value,
        deps: &FactoryDependencies,
    ) -> Result<Box<dyn Plugin>, PluginConfigError> {
        let cfg: PromptCompressorConfig = serde_json::from_value(config)
            .map_err(|e| PluginConfigError::Deserialize(e, plugin_name.to_string()))?;

        let strategy = compile_strategy(cfg.strategy, plugin_name, deps)?;
        Ok(Box::new(PromptCompressor { strategy }))
    }
}

// ─── Compile helper ──────────────────────────────────────────────────────────

fn compile_strategy(
    strategy: Strategy,
    plugin_name: &str,
    deps: &FactoryDependencies,
) -> Result<CompiledStrategy, PluginConfigError> {
    match strategy {
        Strategy::DropOldTurns(c) => {
            if c.keep_last_n == 0 {
                return Err(PluginConfigError::InvalidValue {
                    plugin: plugin_name.to_string(),
                    field: "keep_last_n".to_string(),
                    reason: "must be greater than 0".to_string(),
                });
            }
            Ok(CompiledStrategy::DropOldTurns { keep_last_n: c.keep_last_n })
        }

        Strategy::TruncateToTokens(c) => {
            if c.keep_last_n == 0 {
                return Err(PluginConfigError::InvalidValue {
                    plugin: plugin_name.to_string(),
                    field: "keep_last_n".to_string(),
                    reason: "must be greater than 0".to_string(),
                });
            }
            Ok(CompiledStrategy::TruncateToTokens {
                target_tokens: c.target_tokens,
                keep_last_n: c.keep_last_n,
            })
        }

        Strategy::SummarizeOldTurns(c) => {
            if c.keep_last_n == 0 {
                return Err(PluginConfigError::InvalidValue {
                    plugin: plugin_name.to_string(),
                    field: "keep_last_n".to_string(),
                    reason: "must be greater than 0".to_string(),
                });
            }
            let provider = deps
                .providers
                .get(&c.summarizer.upstream)
                .cloned()
                .ok_or_else(|| PluginConfigError::InvalidValue {
                    plugin: plugin_name.to_string(),
                    field: "summarizer.upstream".to_string(),
                    reason: format!("unknown upstream: {}", c.summarizer.upstream),
                })?;
            let target = BackendTarget {
                provider: c.summarizer.upstream.clone(),
                model: c.summarizer.model.clone(),
                policy: Default::default(),
            };
            Ok(CompiledStrategy::SummarizeOldTurns {
                keep_last_n: c.keep_last_n,
                provider,
                target,
                max_summary_tokens: c.summarizer.max_summary_tokens,
                timeout: Duration::from_millis(c.summarizer.timeout_ms),
            })
        }
    }
}

// ─── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_kind_name() {
        assert_eq!(PromptCompressorFactory.kind_name(), "prompt_compressor");
    }

    #[test]
    fn compiles_drop_old_turns() {
        let raw = serde_json::json!({
            "strategy": { "type": "drop_old_turns", "keep_last_n": 5 }
        });
        let deps = FactoryDependencies::empty();
        let plugin = PromptCompressorFactory.instantiate("test", raw, &deps).unwrap();
        assert_eq!(plugin.kind_name(), "prompt_compressor");
    }

    #[test]
    fn rejects_keep_last_n_zero() {
        let raw = serde_json::json!({
            "strategy": { "type": "drop_old_turns", "keep_last_n": 0 }
        });
        let deps = FactoryDependencies::empty();
        let err = PromptCompressorFactory.instantiate("test", raw, &deps)
            .err().expect("expected error");
        assert!(
            matches!(err, PluginConfigError::InvalidValue { .. }),
            "expected InvalidValue, got {err:?}"
        );
    }

    #[test]
    fn rejects_unknown_upstream() {
        let raw = serde_json::json!({
            "strategy": {
                "type": "summarize_old_turns",
                "keep_last_n": 4,
                "summarizer": {
                    "upstream": "nonexistent",
                    "model": "gpt-4o"
                }
            }
        });
        let deps = FactoryDependencies::empty();
        let err = PromptCompressorFactory.instantiate("test", raw, &deps)
            .err().expect("expected error");
        assert!(
            matches!(err, PluginConfigError::InvalidValue { .. }),
            "expected InvalidValue, got {err:?}"
        );
    }

    // ─── H2 dispatch integration tests ───────────────────────────────────

    use agent_shim_core::{
        ContentBlock, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel,
        GenerationOptions, Message, RequestId, ResolvedPolicy, TextBlock,
    };
    use agent_shim_core::request::RequestMetadata;
    use serde_json::json;

    fn req_with(messages: Vec<Message>) -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: FrontendModel::from("test-model"),
            },
            model: FrontendModel::from("test-model"),
            system: vec![],
            messages,
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

    fn user_t(s: &str) -> Message {
        Message::user(vec![ContentBlock::Text(TextBlock {
            text: s.to_string(),
            extensions: Default::default(),
        })])
    }
    fn assistant_t(s: &str) -> Message {
        Message::assistant(vec![ContentBlock::Text(TextBlock {
            text: s.to_string(),
            extensions: Default::default(),
        })])
    }

    fn ctx() -> PluginContext {
        PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/test-model".to_string(),
        )
    }

    #[tokio::test]
    async fn h2_skipped_no_metric_emission() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let cfg = json!({
            "strategy": { "type": "drop_old_turns", "keep_last_n": 5 }
        });
        let deps = FactoryDependencies::empty();
        let plugin = PromptCompressorFactory
            .instantiate("p", cfg, &deps)
            .unwrap();
        let req = req_with(vec![user_t("a"), assistant_t("b")]);
        let out = plugin.on_decoded_request(&ctx(), req).await.unwrap();
        // Skipped: returned unchanged.
        assert_eq!(out.messages.len(), 2);

        // Metric was emitted (action=skipped is still counted), but messages_dropped_total
        // must NOT have been incremented.
        let snapshot = snapshotter.snapshot().into_vec();
        let dropped_count: u64 = snapshot
            .iter()
            .filter_map(|(key, _, _, value)| {
                if key.key().name() == "agent_shim_plugin_prompt_compressor_messages_dropped_total"
                {
                    match value {
                        DebugValue::Counter(v) => Some(*v),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .sum();
        assert_eq!(dropped_count, 0);
    }

    #[tokio::test]
    async fn h2_compressed_emits_metrics() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let cfg = json!({
            "strategy": { "type": "drop_old_turns", "keep_last_n": 1 }
        });
        let deps = FactoryDependencies::empty();
        let plugin = PromptCompressorFactory
            .instantiate("p", cfg, &deps)
            .unwrap();
        let messages = vec![
            user_t("old-u"), assistant_t("old-a"),
            user_t("kept-u"), assistant_t("kept-a"),
        ];
        let req = req_with(messages);
        let out = plugin.on_decoded_request(&ctx(), req).await.unwrap();
        assert_eq!(out.messages.len(), 2);

        let snapshot = snapshotter.snapshot().into_vec();
        let actions: u64 = snapshot
            .iter()
            .filter_map(|(key, _, _, value)| {
                if key.key().name() == "agent_shim_plugin_prompt_compressor_actions_total" {
                    match value {
                        DebugValue::Counter(v) => Some(*v),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .sum();
        assert_eq!(actions, 1, "one action total");

        let dropped: u64 = snapshot
            .iter()
            .filter_map(|(key, _, _, value)| {
                if key.key().name() == "agent_shim_plugin_prompt_compressor_messages_dropped_total"
                {
                    match value {
                        DebugValue::Counter(v) => Some(*v),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .sum();
        assert_eq!(dropped, 2, "two messages dropped (1 group × 2 msgs)");
    }
}
