use std::collections::BTreeSet;

use super::token_manager::CopilotToken;
use crate::ProviderError;

/// Fetch the list of available model IDs from the Copilot API.
pub async fn list_models(
    http: &reqwest::Client,
    token: &CopilotToken,
) -> Result<BTreeSet<String>, ProviderError> {
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

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| ProviderError::Decode(format!("models response: {e}")))?;

    let mut ids = BTreeSet::new();
    if let Some(data) = json.get("data").and_then(|d| d.as_array()) {
        for item in data {
            if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                ids.insert(id.to_string());
            }
        }
    }
    Ok(ids)
}
