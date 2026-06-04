//! Zhipu GLM provider — speaks OpenAI-Chat shape via open.bigmodel.cn,
//! with two quirks:
//!
//! - Translates canonical `ReasoningEffort` to GLM's `thinking:{type:enabled|disabled}`
//!   field and strips the OpenAI-style `reasoning_effort` field that Zhipu rejects.
//! - Streams interleaved `reasoning_content` deltas — re-uses the deepseek parser.

pub(crate) mod request;
pub(crate) mod response;

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use tracing::{debug, warn};

use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream};

use crate::{http_client, BackendProvider, ProviderCapabilities, ProviderError};

pub struct GlmProvider {
    name: &'static str,
    base_url: String,
    api_key: String,
    default_headers: HeaderMap,
    _timeout: Duration,
    client: crate::ProviderHttpClient,
    capabilities: ProviderCapabilities,
}

impl GlmProvider {
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

        let client = http_client::build(Duration::from_secs(timeout_secs))?;

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
                vision: true,
                json_mode: true,
                accepts_xhigh: false,
            },
        })
    }

    /// Build the upstream URL for chat completions.
    /// Zhipu's documented base URL is `https://open.bigmodel.cn/api/paas/v4`,
    /// so we join `/chat/completions` directly.
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
impl BackendProvider for GlmProvider {
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
        let body = request::build(&req, &target, self.capabilities.accepts_xhigh)?;
        let is_stream = req.stream;

        debug!(
            provider = self.name,
            model = %target.model,
            stream = is_stream,
            "sending request to GLM"
        );

        let mut request_builder = self
            .client
            .post(self.chat_url())
            .bearer_auth(&self.api_key)
            .header(CONTENT_TYPE, "application/json")
            .json(&body);

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
                "GLM upstream returned error"
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

    async fn list_models(
        &self,
    ) -> Result<
        Option<std::collections::BTreeMap<String, agent_shim_core::ModelMetadata>>,
        ProviderError,
    > {
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
                    .map(|id| (id, agent_shim_core::ModelMetadata::default()))
                    .collect::<std::collections::BTreeMap<String, agent_shim_core::ModelMetadata>>()
            })
            .unwrap_or_default();

        if models.is_empty() {
            return Ok(None);
        }
        Ok(Some(models))
    }
}

/// Build a `GlmProvider` from gateway config upstreams.
pub fn from_config(
    upstream_name: &str,
    cfg: &agent_shim_config::GlmUpstream,
) -> Result<GlmProvider, ProviderError> {
    let leaked: &'static str = Box::leak(upstream_name.to_string().into_boxed_str());
    GlmProvider::new(
        leaked,
        &cfg.base_url,
        cfg.api_key.expose(),
        cfg.default_headers.clone(),
        cfg.request_timeout_secs,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glm_provider_constructs_with_capabilities() {
        let provider = GlmProvider::new(
            "glm",
            "https://open.bigmodel.cn/api/paas/v4",
            "test-key",
            BTreeMap::new(),
            30,
        )
        .expect("provider should construct");

        assert_eq!(provider.name(), "glm");
        let caps = provider.capabilities();
        assert!(caps.streaming);
        assert!(caps.tool_use);
        assert!(caps.vision);
        assert!(caps.json_mode);
        assert!(!caps.accepts_xhigh);
    }

    #[test]
    fn chat_url_joins_base_and_path() {
        let provider = GlmProvider::new(
            "glm",
            "https://open.bigmodel.cn/api/paas/v4",
            "test-key",
            BTreeMap::new(),
            30,
        )
        .unwrap();
        assert_eq!(
            provider.chat_url(),
            "https://open.bigmodel.cn/api/paas/v4/chat/completions"
        );
    }

    #[test]
    fn chat_url_trims_trailing_slash_from_base() {
        let provider = GlmProvider::new(
            "glm",
            "https://open.bigmodel.cn/api/paas/v4/",
            "test-key",
            BTreeMap::new(),
            30,
        )
        .unwrap();
        assert_eq!(
            provider.chat_url(),
            "https://open.bigmodel.cn/api/paas/v4/chat/completions"
        );
    }

    #[test]
    fn models_url_joins_base_and_path() {
        let provider = GlmProvider::new(
            "glm",
            "https://open.bigmodel.cn/api/paas/v4",
            "test-key",
            BTreeMap::new(),
            30,
        )
        .unwrap();
        assert_eq!(
            provider.models_url(),
            "https://open.bigmodel.cn/api/paas/v4/models"
        );
    }

    #[tokio::test]
    async fn list_models_returns_discovered_models() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/models")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "object": "list",
                    "data": [
                        {"id": "glm-4.6", "object": "model"},
                        {"id": "glm-5.1", "object": "model"}
                    ]
                }"#,
            )
            .create_async()
            .await;

        let provider =
            GlmProvider::new("glm", server.url(), "test-key", BTreeMap::new(), 30).unwrap();

        let result = provider.list_models().await.unwrap().unwrap();
        assert!(result.contains_key("glm-4.6"));
        assert!(result.contains_key("glm-5.1"));
        assert_eq!(result.len(), 2);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_models_returns_none_on_404() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/models")
            .with_status(404)
            .with_body("not found")
            .create_async()
            .await;

        let provider =
            GlmProvider::new("glm", server.url(), "test-key", BTreeMap::new(), 30).unwrap();

        let result = provider.list_models().await.unwrap();
        assert!(result.is_none());
        mock.assert_async().await;
    }
}
