use std::collections::BTreeMap;

use agent_shim_core::{ModelMetadata, ModelSupports};

use super::token_manager::CopilotToken;
use crate::ProviderError;

/// Fetch the catalog from Copilot's `/models` endpoint and parse full
/// capability metadata into [`ModelMetadata`] entries.
pub async fn list_models(
    http: &reqwest::Client,
    token: &CopilotToken,
) -> Result<BTreeMap<String, ModelMetadata>, ProviderError> {
    let url = format!("{}/models", token.api_base.trim_end_matches('/'));
    let resp = http
        .get(&url)
        .bearer_auth(&token.token)
        .header(reqwest::header::USER_AGENT, super::headers::USER_AGENT)
        .header("Editor-Version", super::headers::EDITOR_VERSION)
        .header(
            "Editor-Plugin-Version",
            super::headers::EDITOR_PLUGIN_VERSION,
        )
        .header(
            "Copilot-Integration-Id",
            super::headers::COPILOT_INTEGRATION_ID,
        )
        .send()
        .await
        .map_err(|e| ProviderError::Network(e.to_string()))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(ProviderError::Upstream { status, body });
    }

    let body = resp
        .text()
        .await
        .map_err(|e| ProviderError::Network(e.to_string()))?;

    parse_models_response(&body)
}

/// Pure parser, exposed for tests. Walks the upstream `data: [...]` array
/// and synthesises a [`ModelMetadata`] per item. Items without `capabilities`
/// or fields therein deserialise to `None` cleanly (via `Option<>` fields).
pub fn parse_models_response(raw: &str) -> Result<BTreeMap<String, ModelMetadata>, ProviderError> {
    let json: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| ProviderError::Decode(format!("models response: {e}")))?;

    let mut out = BTreeMap::new();
    let data = json
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| ProviderError::Decode("models response missing `data` array".into()))?;

    for item in data {
        let Some(id) = item.get("id").and_then(|i| i.as_str()) else {
            continue;
        };
        let caps = item.get("capabilities");
        let limits = caps.and_then(|c| c.get("limits"));
        let supports = caps.and_then(|c| c.get("supports"));
        let family = caps
            .and_then(|c| c.get("family"))
            .and_then(|f| f.as_str())
            .map(|s| s.to_string());

        // Copilot surfaces reasoning capability in two shapes:
        //   - Anthropic family: `supports.thinking: true` (bool)
        //   - GPT/Gemini family: `supports.reasoning_effort: ["low","medium",...]`
        // We collapse both into a single `bool` for the `reasoning_effort`
        // capability flag while preserving the original array for callers
        // (e.g. `copilot models` CLI) that want to display the values.
        let reasoning_effort_values: Option<Vec<String>> = supports
            .and_then(|s| s.get("reasoning_effort"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .filter(|v: &Vec<String>| !v.is_empty());
        let reasoning_effort_flag = match (
            reasoning_effort_values.as_ref(),
            supports
                .and_then(|s| s.get("thinking"))
                .and_then(|v| v.as_bool()),
            supports
                .and_then(|s| s.get("adaptive_thinking"))
                .and_then(|v| v.as_bool()),
        ) {
            (Some(values), _, _) if !values.is_empty() => Some(true),
            (_, Some(b), _) => Some(b),
            (_, _, Some(b)) => Some(b),
            _ => None,
        };

        let metadata = ModelMetadata {
            context_window_tokens: limits
                .and_then(|l| l.get("max_context_window_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            max_output_tokens: limits
                .and_then(|l| l.get("max_output_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            family,
            // Top-level field (sibling of `capabilities`, `version`), NOT under
            // `capabilities.supports`. Drives endpoint selection: a responses-only
            // model like `gpt-5.5` carries `["/responses", "ws:/responses"]`.
            // Empty / missing collapses to `None` so callers fall back to
            // frontend-driven selection.
            supported_endpoints: item
                .get("supported_endpoints")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect::<Vec<String>>()
                })
                .filter(|v: &Vec<String>| !v.is_empty()),
            supports: ModelSupports {
                vision: supports
                    .and_then(|s| s.get("vision"))
                    .and_then(|v| v.as_bool()),
                tool_calls: supports
                    .and_then(|s| s.get("tool_calls"))
                    .and_then(|v| v.as_bool()),
                streaming: supports
                    .and_then(|s| s.get("streaming"))
                    .and_then(|v| v.as_bool()),
                structured_outputs: supports
                    .and_then(|s| s.get("structured_outputs"))
                    .and_then(|v| v.as_bool()),
                reasoning_effort: reasoning_effort_flag,
                reasoning_effort_values,
            },
            version: item
                .get("version")
                .and_then(|v| v.as_str())
                .map(String::from),
            adaptive_thinking: supports
                .and_then(|s| s.get("adaptive_thinking"))
                .and_then(|v| v.as_bool()),
            thinking_budget_min: supports
                .and_then(|s| s.get("min_thinking_budget"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            thinking_budget_max: supports
                .and_then(|s| s.get("max_thinking_budget"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
        };
        out.insert(id.to_string(), metadata);
    }

    Ok(out)
}
