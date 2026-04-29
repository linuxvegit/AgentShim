#![forbid(unsafe_code)]

pub mod github_copilot;
pub mod openai_compatible;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream};

/// A raw byte stream from an upstream provider, for passthrough proxying.
pub type RawByteStream =
    std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>;

#[derive(Debug, Clone, Default)]
pub struct ProviderCapabilities {
    pub streaming: bool,
    pub tool_use: bool,
    pub vision: bool,
    pub json_mode: bool,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("unknown provider: {0}")]
    UnknownProvider(String),
    #[error("upstream error (status={status}): {body}")]
    Upstream { status: u16, body: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("encode error: {0}")]
    Encode(String),
    #[error("capability mismatch: {0}")]
    CapabilityMismatch(String),
}

#[async_trait]
pub trait BackendProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> &ProviderCapabilities;
    async fn complete(
        &self,
        req: CanonicalRequest,
        target: BackendTarget,
    ) -> Result<CanonicalStream, ProviderError>;

    async fn list_models(
        &self,
    ) -> Result<Option<std::collections::BTreeSet<String>>, ProviderError> {
        Ok(None)
    }

    /// Proxy a raw request to the upstream and return the raw byte stream.
    /// Used for passthrough scenarios (e.g. Responses API → Responses API).
    /// Returns (content_type, byte_stream), or None if not supported.
    async fn proxy_raw(
        &self,
        _body: bytes::Bytes,
        _target: BackendTarget,
    ) -> Result<Option<(String, RawByteStream)>, ProviderError> {
        Ok(None)
    }
}

#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn BackendProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, name: String, provider: Arc<dyn BackendProvider>) {
        self.providers.insert(name, provider);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn BackendProvider>> {
        self.providers.get(name).cloned()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Arc<dyn BackendProvider>)> {
        self.providers.iter()
    }

    pub fn resolve(
        &self,
        req: CanonicalRequest,
        target: BackendTarget,
    ) -> Result<(Arc<dyn BackendProvider>, CanonicalRequest, BackendTarget), ProviderError> {
        let provider = self
            .get(&target.provider)
            .ok_or_else(|| ProviderError::UnknownProvider(target.provider.clone()))?;
        Ok((provider, req, target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_shim_core::{BackendTarget, CanonicalRequest, CanonicalStream};

    struct DummyProvider;

    #[async_trait]
    impl BackendProvider for DummyProvider {
        fn name(&self) -> &'static str {
            "dummy"
        }
        fn capabilities(&self) -> &ProviderCapabilities {
            &ProviderCapabilities {
                streaming: false,
                tool_use: false,
                vision: false,
                json_mode: false,
            }
        }
        async fn complete(
            &self,
            _req: CanonicalRequest,
            _target: BackendTarget,
        ) -> Result<CanonicalStream, ProviderError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn default_list_models_returns_none() {
        let p = DummyProvider;
        let result = p.list_models().await.unwrap();
        assert!(result.is_none());
    }
}
