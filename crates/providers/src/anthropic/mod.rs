//! Anthropic-as-backend provider — talks to api.anthropic.com (or any
//! API-compatible endpoint configured by the operator) using Anthropic's
//! Messages API. Implements the hybrid passthrough+canonical path described
//! in ADR-0001.
//!
//! v0.2 ships the passthrough path: when the inbound frontend is
//! `anthropic_messages`, the gateway forwards the raw bytes through
//! `proxy_raw` so all Anthropic-only features (`cache_control`, `thinking`,
//! beta headers) round-trip byte-for-byte. The canonical path (for OpenAI
//! Chat/Responses inbound) is added in Task 5.

pub(crate) mod passthrough;

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream, FrontendKind};

use crate::{BackendProvider, ProviderCapabilities, ProviderError, RawByteStream};

pub struct AnthropicProvider {
    name: &'static str,
    base_url: String,
    api_key: String,
    anthropic_version: String,
    default_headers: HeaderMap,
    client: reqwest::Client,
    capabilities: ProviderCapabilities,
}

impl AnthropicProvider {
    pub fn new(
        name: &'static str,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        anthropic_version: impl Into<String>,
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
            anthropic_version: anthropic_version.into(),
            default_headers: headers,
            client,
            capabilities: ProviderCapabilities {
                streaming: true,
                tool_use: true,
                vision: true,
                json_mode: false,
            },
        })
    }

    pub(crate) fn messages_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{}/v1/messages", base)
    }
}

#[async_trait]
impl BackendProvider for AnthropicProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    async fn complete(
        &self,
        _req: CanonicalRequest,
        _target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError> {
        // Canonical path is implemented in Task 5. For now it returns an
        // error so any non-Anthropic-frontend route fails fast with a clear
        // message rather than silently producing wrong output.
        Err(ProviderError::CapabilityMismatch(
            "Anthropic provider canonical path not yet implemented (Plan 01 Task 5)".into(),
        ))
    }

    async fn proxy_raw(
        &self,
        body: bytes::Bytes,
        target: BackendTarget,
        frontend_kind: FrontendKind,
    ) -> Result<Option<(String, RawByteStream)>, ProviderError> {
        // Passthrough path: only handle Anthropic Messages inbound.
        if frontend_kind != FrontendKind::AnthropicMessages {
            return Ok(None);
        }
        passthrough::send(self, body, target).await.map(Some)
    }
}

/// Build an `AnthropicProvider` from gateway config upstreams.
pub fn from_config(
    upstream_name: &str,
    cfg: &agent_shim_config::AnthropicUpstream,
) -> Result<AnthropicProvider, ProviderError> {
    let leaked: &'static str = Box::leak(upstream_name.to_string().into_boxed_str());
    AnthropicProvider::new(
        leaked,
        &cfg.base_url,
        cfg.api_key.expose(),
        &cfg.anthropic_version,
        cfg.default_headers.clone(),
        cfg.request_timeout_secs,
    )
}
