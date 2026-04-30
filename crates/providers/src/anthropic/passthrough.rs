//! Passthrough path: forward raw inbound bytes (Anthropic Messages shape)
//! to api.anthropic.com unchanged. Rewrites the top-level `model` field on
//! the body to `target.model` so route-level upstream-model overrides apply.

use bytes::Bytes;
use reqwest::header::CONTENT_TYPE;
use tracing::warn;

use agent_shim_core::BackendTarget;

use super::AnthropicProvider;
use crate::{BackendProvider, ProviderError, RawByteStream};

pub(crate) async fn send(
    provider: &AnthropicProvider,
    body: Bytes,
    target: BackendTarget,
) -> Result<(String, RawByteStream), ProviderError> {
    let body = rewrite_model(body, &target.model)?;

    let mut request_builder = provider
        .client
        .post(provider.messages_url())
        .header("x-api-key", provider.api_key.as_str())
        .header("anthropic-version", provider.anthropic_version.as_str())
        .header(CONTENT_TYPE, "application/json")
        .body(body);

    // Configured default headers (e.g. an organization-level operator override).
    for (k, v) in &provider.default_headers {
        request_builder = request_builder.header(k, v);
    }

    // Per-route default `anthropic-beta` if configured. proxy_raw runs before
    // decode, so per-request inbound headers aren't merged here — the inbound
    // request's anthropic-beta will be replayed on the wire by the original
    // bytes (since this is a byte passthrough). This default only kicks in
    // when the inbound request didn't supply one.
    if let Some(beta) = &target.policy.default_anthropic_beta {
        request_builder = request_builder.header("anthropic-beta", beta.as_str());
    }

    let response = request_builder
        .send()
        .await
        .map_err(|e| ProviderError::Network(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable>".to_string());
        warn!(
            provider = provider.name(),
            status = status.as_u16(),
            body = %body_text,
            "anthropic upstream returned error"
        );
        return Err(ProviderError::Upstream {
            status: status.as_u16(),
            body: body_text,
        });
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/event-stream")
        .to_string();

    Ok((content_type, Box::pin(response.bytes_stream())))
}

/// Replace the top-level `model` field in the JSON request body with the
/// route's upstream model name. Mirror of `OpenAiCompatibleProvider::rewrite_model`.
fn rewrite_model(body: Bytes, upstream_model: &str) -> Result<Bytes, ProviderError> {
    let mut value: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| ProviderError::Decode(e.to_string()))?;
    let Some(object) = value.as_object_mut() else {
        return Err(ProviderError::Decode(
            "Anthropic Messages request body must be a JSON object".to_string(),
        ));
    };
    object.insert(
        "model".to_string(),
        serde_json::Value::String(upstream_model.to_string()),
    );
    serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|e| ProviderError::Encode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_model_replaces_top_level_field() {
        let body = Bytes::from(r#"{"model":"claude-3-5-sonnet-20241022","messages":[]}"#);
        let rewritten = rewrite_model(body, "claude-opus-4-7").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(v["model"], "claude-opus-4-7");
    }

    #[test]
    fn rewrite_model_preserves_other_fields() {
        let body = Bytes::from(
            r#"{"model":"claude-3-5-sonnet-20241022","messages":[{"role":"user","content":"hi"}],"max_tokens":1024}"#,
        );
        let rewritten = rewrite_model(body, "claude-opus-4-7").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(v["model"], "claude-opus-4-7");
        assert_eq!(v["max_tokens"], 1024);
        assert_eq!(v["messages"][0]["role"], "user");
    }

    #[test]
    fn rewrite_model_rejects_non_object_body() {
        let body = Bytes::from(r#"["not","an","object"]"#);
        assert!(rewrite_model(body, "claude-opus-4-7").is_err());
    }
}
