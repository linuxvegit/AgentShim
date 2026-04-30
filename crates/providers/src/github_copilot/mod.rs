pub mod auth;
pub mod credential_store;
pub mod endpoint;
pub mod headers;
pub mod models;
pub mod token_manager;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT,
};
use tracing::{debug, warn};
use uuid::Uuid;

use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream};

use crate::{
    openai_compatible::{encode_request, parse_stream, parse_unary, responses_api},
    BackendProvider, ProviderCapabilities, ProviderError, RawByteStream,
};
use credential_store::StoredCredentials;
use headers::{
    COPILOT_INTEGRATION_ID, EDITOR_PLUGIN_VERSION, EDITOR_VERSION, OPENAI_INTENT,
    USER_AGENT as COPILOT_USER_AGENT,
};
use token_manager::{CopilotToken, CopilotTokenManager};

pub struct CopilotProvider {
    manager: CopilotTokenManager,
    http: reqwest::Client,
    capabilities: ProviderCapabilities,
}

impl CopilotProvider {
    fn build_http_client() -> Result<reqwest::Client, ProviderError> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| ProviderError::Network(e.to_string()))
    }

    /// Create and start a `CopilotProvider` using credentials stored at `credential_path`.
    /// Credentials are loaded lazily on first request, so the file doesn't need to exist at startup.
    pub fn spawn(credential_path: PathBuf) -> Result<Self, ProviderError> {
        let http = Self::build_http_client()?;

        let manager = CopilotTokenManager::new_lazy(http.clone(), credential_path);

        Ok(Self {
            manager,
            http,
            capabilities: ProviderCapabilities {
                streaming: true,
                tool_use: true,
                vision: false,
                json_mode: true,
            },
        })
    }

    /// Internal constructor, also used in tests.
    pub fn spawn_with_creds(
        creds: StoredCredentials,
        base_url: String,
    ) -> Result<Self, ProviderError> {
        let http = Self::build_http_client()?;

        let manager = CopilotTokenManager::new(http.clone(), creds, base_url);

        Ok(Self {
            manager,
            http,
            capabilities: ProviderCapabilities {
                streaming: true,
                tool_use: true,
                vision: false,
                json_mode: true,
            },
        })
    }

    fn build_copilot_headers(
        token: &CopilotToken,
        request_id: &str,
        stream: bool,
    ) -> Result<HeaderMap, ProviderError> {
        let mut headers = HeaderMap::new();

        let auth_val = HeaderValue::from_str(&format!("Bearer {}", token.token))
            .map_err(|e| ProviderError::Encode(format!("auth header: {e}")))?;
        headers.insert(AUTHORIZATION, auth_val);

        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        headers.insert(USER_AGENT, HeaderValue::from_static(COPILOT_USER_AGENT));

        let insert =
            |headers: &mut HeaderMap, name: &'static str, val: &str| -> Result<(), ProviderError> {
                headers.insert(
                    HeaderName::from_static(name),
                    HeaderValue::from_str(val)
                        .map_err(|e| ProviderError::Encode(format!("header {name}: {e}")))?,
                );
                Ok(())
            };

        insert(&mut headers, "editor-version", EDITOR_VERSION)?;
        insert(&mut headers, "editor-plugin-version", EDITOR_PLUGIN_VERSION)?;
        insert(
            &mut headers,
            "copilot-integration-id",
            COPILOT_INTEGRATION_ID,
        )?;
        insert(&mut headers, "openai-intent", OPENAI_INTENT)?;
        insert(&mut headers, "x-request-id", request_id)?;

        if stream {
            insert(&mut headers, "accept", "text/event-stream")?;
        }

        Ok(headers)
    }

    /// Build an outgoing HTTP request for testing purposes (header verification).
    pub async fn build_request_for_test(
        &self,
        token: &CopilotToken,
        body: &serde_json::Value,
        request_id: &str,
        stream: bool,
    ) -> Result<reqwest::Request, ProviderError> {
        let url = format!("{}/chat/completions", token.api_base.trim_end_matches('/'));
        let headers = Self::build_copilot_headers(token, request_id, stream)?;

        let mut builder = self.http.post(&url).json(body);
        for (k, v) in &headers {
            builder = builder.header(k, v);
        }

        builder
            .build()
            .map_err(|e| ProviderError::Network(e.to_string()))
    }
}

#[async_trait]
impl BackendProvider for CopilotProvider {
    fn name(&self) -> &'static str {
        "github_copilot"
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    async fn complete(
        &self,
        req: CanonicalRequest,
        target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError> {
        let token = self.manager.get().await?;
        let api_base = token.api_base.clone();
        let request_id = Uuid::new_v4().to_string();
        let is_stream = req.stream;

        let use_responses_api = req.frontend.kind == agent_shim_core::FrontendKind::OpenAiResponses;

        let (url, body_value) = if use_responses_api {
            let url = format!("{}/v1/responses", api_base.trim_end_matches('/'));
            let body = responses_api::encode_request::build(&req, &target);
            (url, body)
        } else {
            let url = format!("{}/chat/completions", api_base.trim_end_matches('/'));
            let body = serde_json::to_value(encode_request::build(&req, &target))
                .unwrap_or_default();
            (url, body)
        };

        debug!(
            provider = "github_copilot",
            model = %target.model,
            stream = is_stream,
            responses_api = use_responses_api,
            "sending request to Copilot"
        );

        let headers = Self::build_copilot_headers(&token, &request_id, is_stream)?;

        let mut builder = self.http.post(&url).json(&body_value);
        for (k, v) in &headers {
            builder = builder.header(k, v);
        }

        // Replay the merged `anthropic-*` snapshot (inbound headers + route
        // defaults already merged by RoutePolicy::resolve in the gateway).
        for (key, value) in &req.resolved_policy.anthropic_headers {
            builder = builder.header(key.as_str(), value.as_str());
        }

        let response = builder
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();
        tracing::debug!(
            provider = "github_copilot",
            %status,
            %content_type,
            "upstream response"
        );

        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.manager.invalidate().await;
            return Err(ProviderError::Upstream {
                status: 401,
                body: "Copilot token expired, invalidated – retry".to_string(),
            });
        }

        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            warn!(
                provider = "github_copilot",
                status = status.as_u16(),
                body = %body_text,
                "upstream returned error"
            );
            return Err(ProviderError::Upstream {
                status: status.as_u16(),
                body: body_text,
            });
        }

        if use_responses_api {
            // Responses API always streams (even non-streaming returns SSE)
            Ok(responses_api::parse_stream::parse(response.bytes_stream()))
        } else if is_stream {
            Ok(parse_stream::parse(response.bytes_stream()))
        } else {
            let bytes = response
                .bytes()
                .await
                .map_err(|e| ProviderError::Network(e.to_string()))?;
            Ok(parse_unary::parse(&bytes))
        }
    }

    async fn list_models(&self) -> Result<Option<BTreeSet<String>>, ProviderError> {
        let token = self.manager.get().await?;
        let models = models::list_models(&self.http, &token).await?;
        if models.is_empty() {
            return Ok(None);
        }
        Ok(Some(models))
    }

    async fn proxy_raw(
        &self,
        body: bytes::Bytes,
        target: BackendTarget,
    ) -> Result<Option<(String, RawByteStream)>, ProviderError> {
        let token = self.manager.get().await?;
        let api_base = token.api_base.clone();
        let url = format!("{}/v1/responses", api_base.trim_end_matches('/'));
        let request_id = Uuid::new_v4().to_string();
        let body = rewrite_responses_model(body, &target.model)?;

        let headers = Self::build_copilot_headers(&token, &request_id, true)?;

        let mut builder = self
            .http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        for (k, v) in &headers {
            builder = builder.header(k, v);
        }

        // Apply per-route default `anthropic-beta` (per-request value would
        // require decoding the body, which proxy_raw deliberately avoids).
        if let Some(beta) = &target.policy.default_anthropic_beta {
            builder = builder.header("anthropic-beta", beta.as_str());
        }

        let response = builder
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.manager.invalidate().await;
            return Err(ProviderError::Upstream {
                status: 401,
                body: "Copilot token expired, invalidated – retry".to_string(),
            });
        }
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Upstream {
                status: status.as_u16(),
                body: body_text,
            });
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("text/event-stream")
            .to_string();

        Ok(Some((content_type, Box::pin(response.bytes_stream()))))
    }
}

fn rewrite_responses_model(
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
