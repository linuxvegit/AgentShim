//! DeepSeek provider — speaks OpenAI-Chat-shape with two quirks that will land
//! in subsequent tasks of Plan 02:
//!
//! - T4 adds a `reasoning_content` SSE-parser extension (interleaved deltas
//!   become canonical thinking blocks).
//! - T5 maps DeepSeek's prompt cache hit/miss usage fields onto the canonical
//!   `Usage` shape and strips Anthropic-style `cache_control` from outbound
//!   bodies (DeepSeek 400s if it sees them).
//!
//! T3 (this file) lands the bare scaffold: a struct, capabilities, and a
//! `BackendProvider` impl that delegates request encoding and response
//! parsing to `oai_chat_wire`. `from_config` and gateway wiring are deferred
//! to T6.

pub(crate) mod request;
pub(crate) mod response;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use tracing::{debug, warn};

use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream};

use crate::{BackendProvider, ProviderCapabilities, ProviderError};

pub struct DeepseekProvider {
    name: &'static str,
    base_url: String,
    api_key: String,
    default_headers: HeaderMap,
    _timeout: Duration,
    client: reqwest::Client,
    capabilities: ProviderCapabilities,
}

impl DeepseekProvider {
    pub fn new(
        name: &'static str,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        default_headers: BTreeMap<String, String>,
        timeout_secs: u64,
    ) -> Result<Self, ProviderError> {
        let mut headers = HeaderMap::new();
        for (k, v) in &default_headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| ProviderError::Encode(format!("invalid header name: {e}")))?;
            let val = HeaderValue::from_str(v)
                .map_err(|e| ProviderError::Encode(format!("invalid header value: {e}")))?;
            headers.insert(name, val);
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        Ok(Self {
            name,
            base_url: base_url.into(),
            api_key: api_key.into(),
            default_headers: headers,
            _timeout: Duration::from_secs(timeout_secs),
            client,
            capabilities: ProviderCapabilities {
                streaming: true,
                tool_use: true,
                vision: false,
                json_mode: true,
            },
        })
    }

    /// Build the upstream URL for chat completions.
    ///
    /// DeepSeek's documented base URL is `https://api.deepseek.com/v1`, so we
    /// join `/chat/completions` directly (no extra `/v1` prefix). Trim a
    /// trailing slash from `base_url` to be tolerant of operator config.
    fn chat_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{}/chat/completions", base)
    }

    /// Build the upstream URL for model listing.
    fn models_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{}/models", base)
    }
}

#[async_trait]
impl BackendProvider for DeepseekProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    async fn complete(
        &self,
        req: CanonicalRequest,
        target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError> {
        let body = request::build(&req, &target);
        let is_stream = req.stream;

        debug!(
            provider = self.name,
            model = %target.model,
            stream = is_stream,
            "sending request to deepseek"
        );

        let mut request_builder = self
            .client
            .post(self.chat_url())
            .bearer_auth(&self.api_key)
            .header(CONTENT_TYPE, "application/json")
            .json(&body);

        // Apply default headers
        for (k, v) in &self.default_headers {
            request_builder = request_builder.header(k, v);
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
                provider = self.name,
                status = status.as_u16(),
                body = %body_text,
                "deepseek upstream returned error"
            );
            return Err(ProviderError::Upstream {
                status: status.as_u16(),
                body: body_text,
            });
        }

        if is_stream {
            Ok(response::parse_stream(response.bytes_stream()))
        } else {
            let bytes = response
                .bytes()
                .await
                .map_err(|e| ProviderError::Network(e.to_string()))?;
            Ok(response::parse_unary(&bytes))
        }
    }

    async fn list_models(&self) -> Result<Option<BTreeSet<String>>, ProviderError> {
        let url = self.models_url();
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Ok(None);
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Decode(e.to_string()))?;

        let models = body
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|id| id.as_str()).map(String::from))
                    .collect::<BTreeSet<String>>()
            })
            .unwrap_or_default();

        if models.is_empty() {
            return Ok(None);
        }
        Ok(Some(models))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_provider_constructs_with_capabilities() {
        let provider = DeepseekProvider::new(
            "deepseek",
            "https://api.deepseek.com/v1",
            "test-key",
            BTreeMap::new(),
            30,
        )
        .expect("provider should construct");

        assert_eq!(provider.name(), "deepseek");

        let caps = provider.capabilities();
        assert!(caps.streaming, "streaming must be enabled");
        assert!(caps.tool_use, "tool_use must be enabled");
        assert!(!caps.vision, "vision must be disabled");
        assert!(caps.json_mode, "json_mode must be enabled");
    }

    #[test]
    fn chat_url_joins_base_and_path() {
        let provider = DeepseekProvider::new(
            "deepseek",
            "https://api.deepseek.com/v1",
            "test-key",
            BTreeMap::new(),
            30,
        )
        .unwrap();
        assert_eq!(
            provider.chat_url(),
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    #[test]
    fn chat_url_trims_trailing_slash_from_base() {
        let provider = DeepseekProvider::new(
            "deepseek",
            "https://api.deepseek.com/v1/",
            "test-key",
            BTreeMap::new(),
            30,
        )
        .unwrap();
        assert_eq!(
            provider.chat_url(),
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    #[test]
    fn models_url_joins_base_and_path() {
        let provider = DeepseekProvider::new(
            "deepseek",
            "https://api.deepseek.com/v1",
            "test-key",
            BTreeMap::new(),
            30,
        )
        .unwrap();
        assert_eq!(provider.models_url(), "https://api.deepseek.com/v1/models");
    }
}
