//! `pii_scrubber` built-in plugin — H2 + H5 regex-based PII redaction.
//!
//! Spec: `docs/superpowers/specs/2026-05-22-phase-7-p06b1-pii-scrubber-design.md`.

#![cfg(feature = "pii_scrubber")]

use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashSet;

use agent_shim_core::{CanonicalRequest, StreamEvent};

use crate::context::PluginContext;
use crate::error::{PluginConfigError, PluginResult};
use crate::trait_def::{HookSet, Plugin, PluginFactory};

// ─── Config types ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PiiScrubberConfig {
    #[serde(default)]
    pub inbound: Vec<RuleConfig>,
    #[serde(default)]
    pub outbound: Vec<RuleConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    pub name: String,
    pub pattern: String,
    pub replacement: String,
    #[serde(default)]
    pub case_insensitive: bool,
    #[serde(default)]
    pub multiline: bool,
}

// ─── Compiled internal state ─────────────────────────────────────────────

struct CompiledRule {
    #[allow(dead_code)] // used by T8 (apply_rules)
    name: String,
    #[allow(dead_code)] // used by T8/T9
    regex: regex::Regex,
    #[allow(dead_code)] // used by T8/T9
    replacement: String,
}

pub struct PiiScrubber {
    #[allow(dead_code)] // used by T8
    inbound: Vec<CompiledRule>,
    #[allow(dead_code)] // used by T9
    outbound: Vec<CompiledRule>,
}

// ─── Constants for H5 buffering (used in T9) ─────────────────────────────

#[allow(dead_code)] // used by T9
const MAX_PATTERN_TAIL: usize = 64;
#[allow(dead_code)] // used by T9
const FLUSH_THRESHOLD: usize = 256;

// ─── Factory ────────────────────────────────────────────────────────────

pub struct PiiScrubberFactory;

impl PluginFactory for PiiScrubberFactory {
    fn kind_name(&self) -> &'static str {
        "pii_scrubber"
    }

    fn instantiate(
        &self,
        plugin_name: &str,
        config: serde_json::Value,
    ) -> Result<Box<dyn Plugin>, PluginConfigError> {
        let cfg: PiiScrubberConfig = serde_json::from_value(config)
            .map_err(|e| PluginConfigError::Deserialize(e, plugin_name.to_string()))?;

        let inbound = compile_rules(plugin_name, "inbound", &cfg.inbound)?;
        let outbound = compile_rules(plugin_name, "outbound", &cfg.outbound)?;

        if inbound.is_empty() && outbound.is_empty() {
            tracing::warn!(
                plugin = plugin_name,
                "pii_scrubber instantiated with empty inbound and outbound — no scrubbing will occur"
            );
        }

        Ok(Box::new(PiiScrubber { inbound, outbound }))
    }
}

fn compile_rules(
    plugin_name: &str,
    direction: &'static str,
    rules: &[RuleConfig],
) -> Result<Vec<CompiledRule>, PluginConfigError> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(rules.len());

    for (i, r) in rules.iter().enumerate() {
        if !is_valid_rule_name(&r.name) {
            return Err(PluginConfigError::InvalidValue {
                plugin: plugin_name.to_string(),
                field: format!("{direction}[{i}].name"),
                reason: format!(
                    "rule name must match ^[A-Za-z0-9_-]{{1,64}}$, got `{}`",
                    r.name
                ),
            });
        }
        if !seen.insert(r.name.clone()) {
            return Err(PluginConfigError::InvalidValue {
                plugin: plugin_name.to_string(),
                field: format!("{direction}[{i}].name"),
                reason: format!("duplicate rule name `{}`", r.name),
            });
        }
        let mut flag = String::new();
        if r.case_insensitive {
            flag.push('i');
        }
        if r.multiline {
            flag.push('m');
        }
        let final_pattern = if flag.is_empty() {
            r.pattern.clone()
        } else {
            format!("(?{flag}){}", r.pattern)
        };
        let regex = regex::Regex::new(&final_pattern).map_err(|e| {
            PluginConfigError::InvalidValue {
                plugin: plugin_name.to_string(),
                field: format!("{direction}[{i}].pattern"),
                reason: format!("invalid regex `{}`: {e}", r.pattern),
            }
        })?;
        out.push(CompiledRule {
            name: r.name.clone(),
            regex,
            replacement: r.replacement.clone(),
        });
    }
    Ok(out)
}

fn is_valid_rule_name(s: &str) -> bool {
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

// ─── Plugin trait impl (stubs filled in by T8/T9) ────────────────────────

#[async_trait]
impl Plugin for PiiScrubber {
    fn kind_name(&self) -> &'static str {
        "pii_scrubber"
    }

    fn hooks(&self) -> HookSet {
        let mut h = HookSet::empty();
        if !self.inbound.is_empty() {
            h |= HookSet::DECODED_REQUEST;
        }
        if !self.outbound.is_empty() {
            h |= HookSet::STREAM_EVENT;
        }
        h
    }

    async fn on_decoded_request(
        &self,
        _ctx: &PluginContext,
        req: CanonicalRequest,
    ) -> PluginResult<CanonicalRequest> {
        // Filled in by T8.
        Ok(req)
    }

    async fn on_stream_event(
        &self,
        _ctx: &PluginContext,
        event: StreamEvent,
    ) -> PluginResult<Vec<StreamEvent>> {
        // Filled in by T9.
        Ok(vec![event])
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Config parsing ─────────────────────────────────────────────────

    #[test]
    fn config_defaults() {
        let cfg: PiiScrubberConfig = serde_json::from_value(json!({})).unwrap();
        assert!(cfg.inbound.is_empty());
        assert!(cfg.outbound.is_empty());
    }

    #[test]
    fn config_deny_unknown_field() {
        let result: Result<PiiScrubberConfig, _> = serde_json::from_value(json!({"extra": 1}));
        assert!(result.is_err(), "deny_unknown_fields must reject unknown keys");
    }

    // ── Factory ────────────────────────────────────────────────────────

    #[test]
    fn factory_kind_name() {
        assert_eq!(PiiScrubberFactory.kind_name(), "pii_scrubber");
    }

    #[test]
    fn factory_compiles_valid_regex() {
        let cfg = json!({
            "inbound": [
                { "name": "email", "pattern": r"[\w.]+@[\w.]+", "replacement": "[X]" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).expect("should compile");
        assert_eq!(plugin.kind_name(), "pii_scrubber");
        assert!(plugin.hooks().contains(crate::Hook::DecodedRequest));
        assert!(!plugin.hooks().contains(crate::Hook::StreamEvent));
    }

    #[test]
    fn factory_rejects_invalid_regex() {
        let cfg = json!({
            "inbound": [
                { "name": "bad", "pattern": "[unclosed", "replacement": "X" }
            ]
        });
        let err = match PiiScrubberFactory.instantiate("p", cfg) {
            Ok(_) => panic!("expected InvalidValue error"),
            Err(e) => e,
        };
        match err {
            PluginConfigError::InvalidValue { field, reason, .. } => {
                assert_eq!(field, "inbound[0].pattern");
                assert!(reason.contains("invalid regex"), "reason was: {reason}");
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    #[test]
    fn factory_rejects_duplicate_rule_name() {
        let cfg = json!({
            "inbound": [
                { "name": "email", "pattern": "a", "replacement": "x" },
                { "name": "email", "pattern": "b", "replacement": "y" }
            ]
        });
        let err = match PiiScrubberFactory.instantiate("p", cfg) {
            Ok(_) => panic!("expected InvalidValue error"),
            Err(e) => e,
        };
        match err {
            PluginConfigError::InvalidValue { field, reason, .. } => {
                assert_eq!(field, "inbound[1].name");
                assert!(reason.contains("duplicate"), "reason was: {reason}");
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    #[test]
    fn factory_rejects_invalid_name_char() {
        let cfg = json!({
            "inbound": [
                { "name": "my rule", "pattern": "a", "replacement": "x" }
            ]
        });
        let err = match PiiScrubberFactory.instantiate("p", cfg) {
            Ok(_) => panic!("expected InvalidValue error"),
            Err(e) => e,
        };
        assert!(matches!(err, PluginConfigError::InvalidValue { .. }));
    }

    #[test]
    fn factory_rejects_overlong_name() {
        let long_name = "a".repeat(65);
        let cfg = json!({
            "inbound": [
                { "name": long_name, "pattern": "a", "replacement": "x" }
            ]
        });
        let err = match PiiScrubberFactory.instantiate("p", cfg) {
            Ok(_) => panic!("expected InvalidValue error"),
            Err(e) => e,
        };
        assert!(matches!(err, PluginConfigError::InvalidValue { .. }));
    }

    #[test]
    fn hooks_minimal_subscription_outbound_only() {
        let cfg = json!({
            "outbound": [
                { "name": "x", "pattern": "a", "replacement": "y" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).unwrap();
        let h = plugin.hooks();
        assert!(!h.contains(crate::Hook::DecodedRequest));
        assert!(h.contains(crate::Hook::StreamEvent));
    }

    #[test]
    fn hooks_empty_subscription_on_empty_config() {
        let plugin = PiiScrubberFactory.instantiate("p", json!({})).unwrap();
        assert_eq!(plugin.hooks(), HookSet::empty());
    }
}
