use serde::{Deserialize, Serialize};

use crate::extensions::ExtensionMap;
use crate::ids::RequestId;
use crate::message::{Message, SystemInstruction};
use crate::policy::ResolvedPolicy;
use crate::target::{FrontendInfo, FrontendModel};
use crate::tool::{ToolChoice, ToolDefinition};

/// Knobs that control the generation process.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GenerationOptions {
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    pub seed: Option<u64>,
    /// Reasoning / thinking control. Translated to provider-native form by
    /// each backend (e.g. OpenAI/Copilot `reasoning_effort`, Anthropic
    /// `thinking.budget_tokens`). `None` means the agent didn't ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningOptions>,
}

/// Frontend-agnostic reasoning controls.
///
/// Models accept a qualitative effort level (`minimal`/`low`/`medium`/`high`)
/// or — on Anthropic — an explicit token budget. We carry both so each backend
/// can pick the form it understands.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReasoningOptions {
    /// Qualitative effort (OpenAI / Copilot / GPT-5 / o-series style).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    /// Explicit reasoning token budget (Anthropic-style).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

/// Qualitative reasoning-effort levels accepted by Copilot / OpenAI / GPT-5.
///
/// Variant declaration order is **intensity order** — `Minimal` is the
/// weakest, `Max` the strongest. The derived `Ord`/`PartialOrd` rely on this
/// ordering, so new tiers must be inserted at the correct intensity position,
/// never appended for convenience.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Xhigh => "xhigh",
            ReasoningEffort::Max => "max",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "minimal" | "none" => Some(ReasoningEffort::Minimal),
            "low" => Some(ReasoningEffort::Low),
            "medium" | "default" => Some(ReasoningEffort::Medium),
            "high" => Some(ReasoningEffort::High),
            "xhigh" | "x-high" | "extra_high" => Some(ReasoningEffort::Xhigh),
            "max" => Some(ReasoningEffort::Max),
            _ => None,
        }
    }

    /// Pick the wire string for this effort from a model's advertised list.
    ///
    /// `advertised` is the upstream's exact `reasoning_effort` vocabulary
    /// (e.g. `["low","medium","high","xhigh","max"]`, or GPT-5's
    /// `["none","low","medium","high","xhigh"]`). The returned value is the
    /// **raw upstream string** for the chosen tier, so provider-specific
    /// spellings such as `"none"` survive verbatim instead of being
    /// re-derived (and corrupted) through [`ReasoningEffort::as_str`].
    ///
    /// Selection rule: the highest advertised tier `<= self`. When every
    /// advertised tier is stronger than `self` — i.e. the request asked for
    /// *less* than the model's floor — the lowest advertised tier is returned
    /// so we still emit a value the model accepts. Unparseable entries are
    /// ignored. Returns `None` only when `advertised` carries no recognisable
    /// tier, signalling the caller to fall back to its static compression.
    ///
    /// This is the single place that turns "the model says it supports
    /// `[..]`" into "this is the effort string we send", so both the
    /// OpenAI-Chat and OpenAI-Responses encoders agree.
    pub fn clamp_to_advertised<'a>(self, advertised: &'a [String]) -> Option<&'a str> {
        // Keep the original spelling alongside the parsed tier.
        let mut parsed: Vec<(ReasoningEffort, &'a str)> = advertised
            .iter()
            .filter_map(|s| Self::parse(s).map(|e| (e, s.as_str())))
            .collect();
        if parsed.is_empty() {
            return None;
        }
        // Sort ascending by intensity so `.rev()` walks strongest-first.
        parsed.sort_by_key(|(e, _)| *e);
        // Highest advertised tier at or below the request.
        if let Some((_, raw)) = parsed.iter().rev().find(|(e, _)| *e <= self) {
            return Some(raw);
        }
        // Request is below the model's floor: emit the lowest tier it offers.
        parsed.first().map(|(_, raw)| *raw)
    }
}

/// Desired output format constraints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Free-form text (default).
    Text,
    /// The model must respond with valid JSON (no schema enforcement).
    JsonObject,
    /// The model must follow the supplied JSON schema.
    JsonSchema {
        name: String,
        schema: serde_json::Value,
        strict: bool,
    },
}

/// Request-level metadata carried through the gateway for logging/tracing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// The fully-normalised, frontend-agnostic request handed to a backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalRequest {
    pub id: RequestId,
    pub frontend: FrontendInfo,
    pub model: FrontendModel,
    #[serde(default)]
    pub system: Vec<SystemInstruction>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub tool_choice: ToolChoice,
    #[serde(default)]
    pub generation: GenerationOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub metadata: RequestMetadata,
    /// `anthropic-*` headers captured from the inbound HTTP request, in the
    /// order they arrived. Frontends populate this; [`crate::RoutePolicy`]
    /// merges with route defaults to produce [`Self::resolved_policy`].
    /// Providers should NOT read this directly — use `resolved_policy`
    /// instead so the inbound-vs-default merge rule lives in one place.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inbound_anthropic_headers: Vec<(String, String)>,
    /// Per-request snapshot of the merged route policy. Populated by the
    /// gateway after route resolution, before the request is handed to a
    /// provider. Providers and logging read from this; no one should be
    /// re-deriving the merge.
    #[serde(default, skip_serializing_if = "is_default_resolved_policy")]
    pub resolved_policy: ResolvedPolicy,
    #[serde(default, skip_serializing_if = "ExtensionMap::is_empty")]
    pub extensions: ExtensionMap,
}

fn is_default_resolved_policy(p: &ResolvedPolicy) -> bool {
    p == &ResolvedPolicy::default()
}

#[cfg(test)]
mod effort_tests {
    use super::*;

    #[test]
    fn parse_known_efforts() {
        assert_eq!(
            ReasoningEffort::parse("minimal"),
            Some(ReasoningEffort::Minimal)
        );
        assert_eq!(ReasoningEffort::parse("low"), Some(ReasoningEffort::Low));
        assert_eq!(
            ReasoningEffort::parse("medium"),
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(ReasoningEffort::parse("high"), Some(ReasoningEffort::High));
        assert_eq!(
            ReasoningEffort::parse("xhigh"),
            Some(ReasoningEffort::Xhigh)
        );
        assert_eq!(ReasoningEffort::parse("max"), Some(ReasoningEffort::Max));
    }

    #[test]
    fn parse_aliases() {
        assert_eq!(
            ReasoningEffort::parse("none"),
            Some(ReasoningEffort::Minimal)
        );
        assert_eq!(
            ReasoningEffort::parse("default"),
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(
            ReasoningEffort::parse("x-high"),
            Some(ReasoningEffort::Xhigh)
        );
        assert_eq!(
            ReasoningEffort::parse("extra_high"),
            Some(ReasoningEffort::Xhigh)
        );
        assert_eq!(ReasoningEffort::parse("MAX"), Some(ReasoningEffort::Max));
    }

    #[test]
    fn parse_rejects_unknown() {
        assert_eq!(ReasoningEffort::parse("super"), None);
        assert_eq!(ReasoningEffort::parse(""), None);
    }

    #[test]
    fn as_str_round_trip() {
        for e in [
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Xhigh,
            ReasoningEffort::Max,
        ] {
            assert_eq!(ReasoningEffort::parse(e.as_str()), Some(e));
        }
    }

    #[test]
    fn effort_ordering_is_intensity() {
        assert!(ReasoningEffort::Minimal < ReasoningEffort::Low);
        assert!(ReasoningEffort::High < ReasoningEffort::Xhigh);
        assert!(ReasoningEffort::Xhigh < ReasoningEffort::Max);
        assert!(ReasoningEffort::Max > ReasoningEffort::Medium);
    }

    fn vals(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn clamp_picks_exact_tier_when_advertised() {
        // claude-opus-4.8: max is advertised, so Max passes through as "max".
        let adv = vals(&["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(ReasoningEffort::Max.clamp_to_advertised(&adv), Some("max"));
        assert_eq!(
            ReasoningEffort::Xhigh.clamp_to_advertised(&adv),
            Some("xhigh")
        );
    }

    #[test]
    fn clamp_steps_down_to_highest_available() {
        // claude-opus-4.6 / sonnet-4.6: max present but NO xhigh. A request
        // for Xhigh must drop to "high" (the highest tier <= Xhigh), never
        // emit "max" (which is stronger) nor "xhigh" (not advertised).
        let adv = vals(&["low", "medium", "high", "max"]);
        assert_eq!(
            ReasoningEffort::Xhigh.clamp_to_advertised(&adv),
            Some("high")
        );
        // Max still lands on "max".
        assert_eq!(ReasoningEffort::Max.clamp_to_advertised(&adv), Some("max"));
    }

    #[test]
    fn clamp_max_falls_to_xhigh_when_no_max() {
        // gpt-5.5: advertises xhigh but not max. Max clamps to "xhigh".
        let adv = vals(&["none", "low", "medium", "high", "xhigh"]);
        assert_eq!(
            ReasoningEffort::Max.clamp_to_advertised(&adv),
            Some("xhigh")
        );
    }

    #[test]
    fn clamp_preserves_raw_spelling_below_floor() {
        // gpt-5.x advertises "none" (not "minimal"). A Minimal request must
        // round-trip the upstream's own spelling, and a below-floor request
        // falls to the lowest advertised tier verbatim.
        let adv = vals(&["none", "low", "medium", "high", "xhigh"]);
        assert_eq!(
            ReasoningEffort::Minimal.clamp_to_advertised(&adv),
            Some("none")
        );
    }

    #[test]
    fn clamp_returns_none_on_empty_or_unparseable() {
        assert_eq!(ReasoningEffort::Max.clamp_to_advertised(&[]), None);
        let junk = vals(&["turbo", "ludicrous"]);
        assert_eq!(ReasoningEffort::Max.clamp_to_advertised(&junk), None);
    }
}
