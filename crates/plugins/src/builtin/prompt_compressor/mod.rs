#![cfg(feature = "prompt_compressor")]

pub mod config;
pub(super) mod drop_old_turns;
pub(super) mod groups;

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
#[allow(dead_code)] // filled in by T10 dispatcher
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
        req: CanonicalRequest,
    ) -> PluginResult<CanonicalRequest> {
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
}
