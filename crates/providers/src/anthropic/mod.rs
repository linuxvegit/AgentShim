//! Anthropic-as-backend provider — talks to api.anthropic.com (or any
//! API-compatible endpoint configured by the operator) using Anthropic's
//! Messages API. Implements the hybrid passthrough+canonical path described
//! in ADR-0001.
//!
//! Two paths share this provider:
//!
//! * **Passthrough** ([`passthrough::send`]) — when the inbound frontend is
//!   `anthropic_messages`, the gateway forwards the raw bytes through
//!   `proxy_raw` so all Anthropic-only features (`cache_control`, `thinking`,
//!   beta headers) round-trip byte-for-byte.
//! * **Canonical** ([`request::build`] + [`response::parse_stream`] /
//!   [`response::parse_unary`]) — for OpenAI Chat / OpenAI Responses inbound,
//!   the canonical request is encoded into Anthropic's `/v1/messages` JSON
//!   shape and the upstream's SSE / unary response is decoded back into a
//!   `CanonicalStream`. This is the inverse of the Anthropic frontend's
//!   `decode` + `encode_stream` modules.

pub(crate) mod passthrough;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod wire;

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use tracing::{debug, warn};

use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream, FrontendKind};

use crate::{http_client, BackendProvider, ProviderCapabilities, ProviderError, RawByteStream};

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

        let client = http_client::build(Duration::from_secs(timeout_secs))?;

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
        req: CanonicalRequest,
        target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError> {
        let body = request::build(&req, &target);
        let is_stream = req.stream;

        debug!(
            provider = self.name,
            model = %target.model,
            stream = is_stream,
            "sending canonical request to Anthropic"
        );

        let mut request_builder = self
            .client
            .post(self.messages_url())
            .header("x-api-key", self.api_key.as_str())
            .header("anthropic-version", self.anthropic_version.as_str())
            .header(CONTENT_TYPE, "application/json")
            .json(&body);

        // Configured default headers (e.g. an organization-level operator override).
        for (k, v) in &self.default_headers {
            request_builder = request_builder.header(k, v);
        }

        // Replay any captured per-request `anthropic-*` headers + per-route
        // `default_anthropic_beta`. The pipeline already merged them into
        // `req.resolved_policy.anthropic_headers` via `RoutePolicy::resolve`.
        for (key, value) in &req.resolved_policy.anthropic_headers {
            request_builder = request_builder.header(key.as_str(), value.as_str());
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
                "anthropic upstream returned error"
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
