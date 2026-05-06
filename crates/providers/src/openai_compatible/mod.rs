pub(crate) mod responses_api;

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use tracing::{debug, warn};

use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream};

use crate::{http_client, BackendProvider, ProviderCapabilities, ProviderError, RawByteStream};

pub struct OpenAiCompatibleProvider {
    name: &'static str,
    base_url: String,
    api_key: String,
    default_headers: HeaderMap,
    _timeout: Duration,
    client: reqwest::Client,
    capabilities: ProviderCapabilities,
}

impl OpenAiCompatibleProvider {
    pub fn new(
        name: &'static str,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        default_headers: std::collections::BTreeMap<String, String>,
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
            },
        })
    }

    /// Build the upstream URL for chat completions.
    fn chat_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{}/v1/chat/completions", base)
    }

    /// Build the upstream URL for the Responses API.
    fn responses_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{}/v1/responses", base)
    }

    fn rewrite_model(
        body: bytes::Bytes,
        upstream_model: &str,
    ) -> Result<bytes::Bytes, ProviderError> {
        let mut value: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| ProviderError::Decode(e.to_string()))?;
        let Some(object) = value.as_object_mut() else {
            return Err(ProviderError::Decode(
                "Responses request body must be a JSON object".to_string(),
            ));
        };
        object.insert(
            "model".to_string(),
            serde_json::Value::String(upstream_model.to_string()),
        );
        serde_json::to_vec(&value)
            .map(bytes::Bytes::from)
            .map_err(|e| ProviderError::Encode(e.to_string()))
    }
}

#[async_trait]
impl BackendProvider for OpenAiCompatibleProvider {
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
        let body = crate::oai_chat_wire::canonical_to_chat::build(&req, &target);
        let is_stream = req.stream;

        debug!(
            provider = self.name,
            model = %target.model,
            stream = is_stream,
            "sending request to upstream"
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

        // Forward Anthropic-style negotiation headers (e.g. anthropic-beta for
        // 1M context). Inbound value wins; falls back to per-route default.
        request_builder = apply_anthropic_passthrough_headers(request_builder, &req, &target);

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
                "upstream returned error"
            );
            return Err(ProviderError::Upstream {
                status: status.as_u16(),
                body: body_text,
            });
        }

        if is_stream {
            let byte_stream = response.bytes_stream();
            Ok(crate::oai_chat_wire::chat_sse_parser::parse(byte_stream))
        } else {
            let bytes = response
                .bytes()
                .await
                .map_err(|e| ProviderError::Network(e.to_string()))?;
            Ok(crate::oai_chat_wire::chat_unary_parser::parse(&bytes))
        }
    }

    async fn list_models(&self) -> Result<Option<BTreeSet<String>>, ProviderError> {
        let url = format!("{}/v1/models", self.base_url.trim_end_matches('/'));
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

    async fn proxy_raw(
        &self,
        body: bytes::Bytes,
        target: BackendTarget,
        frontend_kind: agent_shim_core::FrontendKind,
    ) -> Result<Option<(String, RawByteStream)>, ProviderError> {
        // OpenAI-compat's proxy_raw only knows the Responses API shape.
        // For any other frontend, fall back to the canonical encode/decode path.
        if frontend_kind != agent_shim_core::FrontendKind::OpenAiResponses {
            return Ok(None);
        }
        let body = Self::rewrite_model(body, &target.model)?;
        let mut request_builder = self
            .client
            .post(self.responses_url())
            .bearer_auth(&self.api_key)
            .header(CONTENT_TYPE, "application/json")
            .body(body);

        for (k, v) in &self.default_headers {
            request_builder = request_builder.header(k, v);
        }

        // Apply route default for `anthropic-beta` if configured (per-request
        // value isn't available here — proxy_raw runs before decode).
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
                provider = self.name,
                status = status.as_u16(),
                body = %body_text,
                "upstream returned error"
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

        Ok(Some((content_type, Box::pin(response.bytes_stream()))))
    }
}

use std::collections::BTreeSet;

/// Forward Anthropic-style negotiation headers onto the outbound HTTP request.
/// Reads the merged snapshot from `req.resolved_policy` — the gateway has
/// already applied the inbound-vs-route-default merge.
fn apply_anthropic_passthrough_headers(
    mut builder: reqwest::RequestBuilder,
    req: &CanonicalRequest,
    _target: &BackendTarget,
) -> reqwest::RequestBuilder {
    for (key, value) in &req.resolved_policy.anthropic_headers {
        builder = builder.header(key.as_str(), value.as_str());
    }
    builder
}

/// Build an `OpenAiCompatibleProvider` from gateway config upstreams.
pub fn from_config(
    upstream_name: &str,
    cfg: &agent_shim_config::OpenAiCompatibleUpstream,
) -> Result<OpenAiCompatibleProvider, ProviderError> {
    // Leak upstream_name as 'static for the name field.
    // This is acceptable since upstream names live for the lifetime of the process.
    let leaked: &'static str = Box::leak(upstream_name.to_string().into_boxed_str());
    OpenAiCompatibleProvider::new(
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

    #[tokio::test]
    async fn list_models_returns_discovered_models() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/v1/models")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                "object": "list",
                "data": [
                    {"id": "gpt-4o", "object": "model"},
                    {"id": "gpt-4o-mini", "object": "model"},
                    {"id": "deepseek-chat", "object": "model"}
                ]
            }"#,
            )
            .create_async()
            .await;

        let provider =
            OpenAiCompatibleProvider::new("test", server.url(), "test-key", Default::default(), 30)
                .unwrap();

        let result = provider.list_models().await.unwrap().unwrap();
        assert!(result.contains("gpt-4o"));
        assert!(result.contains("gpt-4o-mini"));
        assert!(result.contains("deepseek-chat"));
        assert_eq!(result.len(), 3);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_models_returns_none_on_404() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/v1/models")
            .with_status(404)
            .with_body("not found")
            .create_async()
            .await;

        let provider =
            OpenAiCompatibleProvider::new("test", server.url(), "test-key", Default::default(), 30)
                .unwrap();

        let result = provider.list_models().await.unwrap();
        assert!(result.is_none());
        mock.assert_async().await;
    }
}
