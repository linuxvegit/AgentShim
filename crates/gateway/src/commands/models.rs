//! `agent-shim models <name>` — print the model catalog for one upstream
//! configured in `gateway.yaml`. Mirrors `copilot models` shape for every
//! provider type without requiring device-flow credentials.
//!
//! Reuses `AppState::new` for provider registry construction — the same
//! code path as `serve` and `show-catalog`. Single source of truth at
//! the cost of running discovery for every other upstream during a
//! one-shot CLI invocation; this is the price of not maintaining a
//! parallel match-on-`UpstreamConfig` factory.

use std::path::Path;

use anyhow::Result;

use agent_shim_config::UpstreamConfig;

use super::render_models::{self, RenderHeader};
use crate::state::AppState;

pub async fn run(name: &str, config_path: &Path, format: &str) -> Result<()> {
    let cfg = agent_shim_config::load_from_path(config_path)
        .map_err(|e| anyhow::anyhow!("Failed to load config: {}", e))?;
    agent_shim_config::validate(&cfg)
        .map_err(|e| anyhow::anyhow!("Config validation failed: {}", e))?;

    // Look up the kind label first (before consuming cfg into AppState)
    // so the not-found error can list it cleanly.
    let kind = cfg.upstreams.get(name).map(provider_kind_label);
    let available = available_names(&cfg.upstreams);

    let (state, _reload_rx) = AppState::new(cfg).await?;

    let provider = state
        .core
        .providers
        .get(name)
        .ok_or_else(|| missing_upstream_error(name, &available))?;

    let map = provider
        .list_models()
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "upstream '{name}' (provider kind '{}') does not support model discovery",
                kind.unwrap_or("unknown")
            )
        })?;

    let header = RenderHeader {
        line1: format!(
            "Upstream '{name}' (type: {})",
            kind.unwrap_or("unknown")
        ),
    };
    render_models::render(&map, format, &header)
}

/// Variant tag matching the `#[serde(tag = "type", rename_all = "snake_case")]`
/// shape on `UpstreamConfig`. The match is exhaustive on purpose — adding
/// a new variant should force a compile error here so the operator-facing
/// error string stays accurate.
fn provider_kind_label(cfg: &UpstreamConfig) -> &'static str {
    match cfg {
        UpstreamConfig::OpenAiCompatible(_) => "open_ai_compatible",
        UpstreamConfig::GithubCopilot(_) => "github_copilot",
        UpstreamConfig::Anthropic(_) => "anthropic",
        UpstreamConfig::Deepseek(_) => "deepseek",
        UpstreamConfig::Gemini(_) => "gemini",
        UpstreamConfig::Glm(_) => "glm",
    }
}

fn available_names(
    upstreams: &std::collections::BTreeMap<String, UpstreamConfig>,
) -> Vec<String> {
    // BTreeMap iteration is already sorted; collect into Vec so callers
    // can format the comma-separated list directly.
    upstreams.keys().cloned().collect()
}

fn missing_upstream_error(name: &str, available: &[String]) -> anyhow::Error {
    let available_str = if available.is_empty() {
        "none".to_string()
    } else {
        available.join(", ")
    };
    let mut msg = format!("no upstream named '{name}' in config (available: {available_str})");
    if name == "copilot" {
        msg.push_str(
            "\n  (hint: `agent-shim copilot models` uses the device-flow credential \
             and does not require a config entry)",
        );
    }
    anyhow::anyhow!(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_config::{
        AnthropicUpstream, DeepseekUpstream, GeminiUpstream, GithubCopilotUpstream, GlmUpstream,
        OpenAiCompatibleUpstream, Secret, Tier, UpstreamConfig,
    };
    use std::collections::BTreeMap;

    fn openai_compat() -> UpstreamConfig {
        UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
            base_url: "https://api.example.com".into(),
            api_key: Secret::new("k"),
            default_headers: Default::default(),
            request_timeout_secs: 30,
            tier: Tier::Standard,
            cost: None,
            p95_latency_budget_ms: None,
        })
    }

    #[test]
    fn provider_kind_label_covers_every_variant() {
        assert_eq!(provider_kind_label(&openai_compat()), "open_ai_compatible");
        assert_eq!(
            provider_kind_label(&UpstreamConfig::GithubCopilot(GithubCopilotUpstream {
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            })),
            "github_copilot"
        );
        assert_eq!(
            provider_kind_label(&UpstreamConfig::Anthropic(AnthropicUpstream {
                base_url: "https://api.anthropic.com".into(),
                api_key: Secret::new("k"),
                anthropic_version: "2023-06-01".into(),
                default_headers: Default::default(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            })),
            "anthropic"
        );
        assert_eq!(
            provider_kind_label(&UpstreamConfig::Deepseek(DeepseekUpstream {
                base_url: "https://api.deepseek.com".into(),
                api_key: Secret::new("k"),
                default_headers: Default::default(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            })),
            "deepseek"
        );
        assert_eq!(
            provider_kind_label(&UpstreamConfig::Gemini(GeminiUpstream {
                base_url: "https://generativelanguage.googleapis.com".into(),
                api_key: Secret::new("k"),
                default_headers: Default::default(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            })),
            "gemini"
        );
        assert_eq!(
            provider_kind_label(&UpstreamConfig::Glm(GlmUpstream {
                base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
                api_key: Secret::new("k"),
                default_headers: Default::default(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            })),
            "glm"
        );
    }

    #[test]
    fn missing_upstream_error_includes_available_names_sorted() {
        let err = missing_upstream_error("nope", &["a".into(), "b".into()]).to_string();
        assert!(err.contains("no upstream named 'nope'"), "{err}");
        assert!(err.contains("available: a, b"), "{err}");
    }

    #[test]
    fn missing_upstream_error_empty_available_says_none() {
        let err = missing_upstream_error("nope", &[]).to_string();
        assert!(err.contains("available: none"), "{err}");
    }

    #[test]
    fn missing_upstream_error_copilot_includes_device_flow_hint() {
        let err = missing_upstream_error("copilot", &["glm".into()]).to_string();
        assert!(err.contains("`agent-shim copilot models`"), "{err}");
        assert!(err.contains("device-flow"), "{err}");
    }

    #[test]
    fn missing_upstream_error_non_copilot_omits_hint() {
        let err = missing_upstream_error("glm-typo", &["glm".into()]).to_string();
        assert!(!err.contains("device-flow"), "{err}");
    }

    #[test]
    fn available_names_returns_sorted_keys() {
        let mut upstreams: BTreeMap<String, UpstreamConfig> = BTreeMap::new();
        upstreams.insert("zeta".into(), openai_compat());
        upstreams.insert("alpha".into(), openai_compat());
        let names = available_names(&upstreams);
        assert_eq!(names, vec!["alpha".to_string(), "zeta".to_string()]);
    }
}
