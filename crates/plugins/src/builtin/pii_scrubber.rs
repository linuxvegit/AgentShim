//! `pii_scrubber` built-in plugin — H2 + H5 regex-based PII redaction.
//!
//! Spec: `docs/superpowers/specs/2026-05-22-phase-7-p06b1-pii-scrubber-design.md`.

#![cfg(feature = "pii_scrubber")]

use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashSet;

use agent_shim_core::{CanonicalRequest, ContentBlock, StreamEvent};
use agent_shim_observability::metrics::catalog::PluginPiiScrubberMatchesTotal;

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
    name: String,
    regex: regex::Regex,
    replacement: String,
}

pub struct PiiScrubber {
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

/// Apply each rule in order to `text`, returning the scrubbed string and
/// emitting the per-rule match counter. `direction` is the static label
/// used for the metric ("inbound" | "outbound").
///
/// Rules are applied sequentially over the running result so that earlier
/// replacements feed into later ones. Counter increments use the
/// pre-replacement match count, which is the natural metric semantics:
/// "how many times did rule R fire?". A rule that produces no matches
/// emits no counter sample.
fn apply_rules(
    rules: &[CompiledRule],
    text: &str,
    _ctx: &PluginContext,
    direction: &'static str,
) -> String {
    let mut current = std::borrow::Cow::Borrowed(text);
    for rule in rules {
        let match_count = rule.regex.find_iter(&current).count() as u64;
        if match_count > 0 {
            metrics::counter!(
                PluginPiiScrubberMatchesTotal::NAME,
                "rule" => rule.name.clone(),
                "direction" => direction,
            )
            .increment(match_count);
            let replaced = rule
                .regex
                .replace_all(&current, rule.replacement.as_str())
                .into_owned();
            current = std::borrow::Cow::Owned(replaced);
        }
    }
    current.into_owned()
}

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
        ctx: &PluginContext,
        mut req: CanonicalRequest,
    ) -> PluginResult<CanonicalRequest> {
        for msg in &mut req.messages {
            for block in &mut msg.content {
                match block {
                    ContentBlock::Text(t) => {
                        t.text = apply_rules(&self.inbound, &t.text, ctx, "inbound");
                    }
                    ContentBlock::Reasoning(r) => {
                        r.text = apply_rules(&self.inbound, &r.text, ctx, "inbound");
                    }
                    _ => {}
                }
            }
        }
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

    // ── H2 hook ────────────────────────────────────────────────────────

    use agent_shim_core::message::Message;
    use agent_shim_core::request::RequestMetadata;
    use agent_shim_core::{
        BinarySource, ExtensionMap, FrontendInfo, FrontendKind, FrontendModel, GenerationOptions,
        ImageBlock, MessageRole, ReasoningBlock, RequestId, ResolvedPolicy, TextBlock,
    };

    fn make_ctx() -> PluginContext {
        PluginContext::new(
            RequestId::new(),
            FrontendKind::AnthropicMessages,
            "anthropic_messages/test".to_string(),
        )
    }

    fn make_base_request() -> CanonicalRequest {
        CanonicalRequest {
            id: RequestId::new(),
            frontend: FrontendInfo {
                kind: FrontendKind::AnthropicMessages,
                requested_model: FrontendModel::from("test-model"),
            },
            model: FrontendModel::from("test-model"),
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

    fn make_request_with_text(text: &str) -> CanonicalRequest {
        let block = ContentBlock::Text(TextBlock {
            text: text.to_string(),
            extensions: ExtensionMap::new(),
        });
        let msg = Message {
            role: MessageRole::User,
            content: vec![block],
            name: None,
            extensions: ExtensionMap::new(),
        };
        let mut req = make_base_request();
        req.messages = vec![msg];
        req
    }

    fn make_request_with_reasoning(text: &str) -> CanonicalRequest {
        let block = ContentBlock::Reasoning(ReasoningBlock {
            text: text.to_string(),
            extensions: ExtensionMap::new(),
        });
        let msg = Message {
            role: MessageRole::Assistant,
            content: vec![block],
            name: None,
            extensions: ExtensionMap::new(),
        };
        let mut req = make_base_request();
        req.messages = vec![msg];
        req
    }

    #[tokio::test]
    async fn h2_scrubs_text_blocks() {
        let cfg = json!({
            "inbound": [
                { "name": "email", "pattern": r"\w+@\w+\.\w+", "replacement": "[EMAIL]" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).unwrap();
        let req = make_request_with_text("Contact me at alice@example.com please.");
        let out = plugin.on_decoded_request(&make_ctx(), req).await.unwrap();
        let text = match &out.messages[0].content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected Text block"),
        };
        assert_eq!(text, "Contact me at [EMAIL] please.");
    }

    #[tokio::test]
    async fn h2_scrubs_reasoning_blocks() {
        let cfg = json!({
            "inbound": [
                { "name": "ssn", "pattern": r"\b\d{3}-\d{2}-\d{4}\b", "replacement": "[SSN]" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).unwrap();
        let req = make_request_with_reasoning("User mentioned SSN 123-45-6789 earlier.");
        let out = plugin.on_decoded_request(&make_ctx(), req).await.unwrap();
        let text = match &out.messages[0].content[0] {
            ContentBlock::Reasoning(r) => r.text.clone(),
            _ => panic!("expected Reasoning block"),
        };
        assert_eq!(text, "User mentioned SSN [SSN] earlier.");
    }

    #[tokio::test]
    async fn h2_skips_other_blocks() {
        let cfg = json!({
            "inbound": [
                { "name": "email", "pattern": r"\w+@\w+", "replacement": "[X]" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).unwrap();
        let image_block = ContentBlock::Image(ImageBlock {
            source: BinarySource::Url {
                url: "https://example.com/cat@home.png".to_string(),
            },
            extensions: ExtensionMap::new(),
        });
        let mut req = make_base_request();
        req.messages = vec![Message {
            role: MessageRole::User,
            content: vec![image_block.clone()],
            name: None,
            extensions: ExtensionMap::new(),
        }];

        let out = plugin.on_decoded_request(&make_ctx(), req).await.unwrap();
        match &out.messages[0].content[0] {
            ContentBlock::Image(img) => {
                // Source URL must be unchanged even though it contains '@'.
                match &img.source {
                    BinarySource::Url { url } => {
                        assert_eq!(url, "https://example.com/cat@home.png");
                    }
                    other => panic!("expected Url source, got {other:?}"),
                }
            }
            other => panic!("expected unmodified Image, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn h2_counter_increments_per_match() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let cfg = json!({
            "inbound": [
                { "name": "email", "pattern": r"\w+@\w+\.\w+", "replacement": "[E]" }
            ]
        });
        let plugin = PiiScrubberFactory.instantiate("p", cfg).unwrap();
        let req = make_request_with_text("a@b.com c@d.com");
        plugin.on_decoded_request(&make_ctx(), req).await.unwrap();

        let snapshot = snapshotter.snapshot().into_vec();
        let matches: u64 = snapshot
            .iter()
            .filter_map(|(key, _, _, value)| {
                if key.key().name() == "agent_shim_plugin_pii_scrubber_matches_total" {
                    match value {
                        DebugValue::Counter(v) => Some(*v),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .sum();
        assert_eq!(matches, 2, "two emails should produce two counter increments");
    }
}
