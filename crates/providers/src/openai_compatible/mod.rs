pub(crate) mod encode_request;
pub(crate) mod parse_stream;
pub(crate) mod parse_unary;
pub(crate) mod wire;

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use tracing::{debug, warn};

use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream};

use crate::{BackendProvider, ProviderCapabilities, ProviderError};

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
        let body = encode_request::build(&req, &target.model);
        let is_stream = req.stream;

        debug!(
            provider = self.name,
            model = %target.model,
            stream = is_stream,
            "sending request to upstream"
        );

        let mut request_builder = self
            .client
            .post(&self.chat_url())
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
                "upstream returned error"
            );
            return Err(ProviderError::Upstream {
                status: status.as_u16(),
                body: body_text,
            });
        }

        if is_stream {
            let byte_stream = response.bytes_stream();
            Ok(parse_stream::parse(byte_stream))
        } else {
            let bytes = response
                .bytes()
                .await
                .map_err(|e| ProviderError::Network(e.to_string()))?;
            Ok(parse_unary::parse(&bytes))
        }
    }
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
