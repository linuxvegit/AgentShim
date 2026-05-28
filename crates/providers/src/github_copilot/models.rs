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
                // Copilot exposes reasoning support as the `thinking` flag
                // for Claude-family models; for GPT-5 it surfaces as the
                // `reasoning_effort` array — neither of those is a plain
                // bool we can read here. Plan default keeps this `None` for
                // both shapes; future refinement can extract a bool from
                // the array form.
                reasoning_effort: supports
                    .and_then(|s| s.get("thinking"))
                    .and_then(|v| v.as_bool()),
            },
            version: item
                .get("version")
                .and_then(|v| v.as_str())
                .map(String::from),
        };
        out.insert(id.to_string(), metadata);
    }

    Ok(out)
}
